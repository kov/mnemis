//! The written briefing: an agent run over a whole period that writes what
//! came in and what matters, and picks the emails worth reading. It starts from the
//! period's tagged mail, reads what deserves it, and can look up earlier mail
//! (locally or on the server) to explain an email's context. It runs on
//! demand, and the apps call [`refresh_if_stale`] for the last day when they
//! are in the foreground; a run can take minutes, so sync never does.
//!
//! One briefing is kept per period length, with the newest email it saw, so
//! the briefing can say how much mail arrived since.

use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::{Local, TimeZone, Utc};
use mnemis_types::{BriefingMailDto, WrittenBriefingDto};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;

use super::Mail;
use crate::agent;
use crate::extract::expected::text;
use crate::extract::tools::{self, DispatchOutput, FetchBudget, ToolScope};
use crate::llm::{InputItem, LlmTransport, OutputItem, Role, ToolDef};

/// Model calls one briefing may take; the last is made without tools, so the
/// run always ends with the briefing.
const TURNS: usize = 10;
/// Most emails a briefing picks.
const MAX_HIGHLIGHTS: usize = 8;
/// Sync rewrites the last day's briefing at most this often.
const STALE_SECS: i64 = 4 * 3_600;

/// Briefings rewrite one row per period; two at once would waste a run.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SYSTEM: &str = "You write the user's mail briefing: what came in over a \
period and what of it they should read. You are given the period's emails, \
already sorted (category, interest 0 noise to 3 important, a reason), the \
to-dos found in them and the emails the user is waiting for.\n\n\
Look into what deserves it before you write: read emails with fetch_messages \
when the sender and subject aren't enough, and use search_messages (or \
search_mail_server for mail older than the last 90 days) to find what an email \
follows from: an answer to something the user sent, the next step of an \
order or a process, a change from what was said before. Call highlight for \
each email the user should read, most important first, at most 8, with one \
sentence saying why. Don't highlight what is already a to-do unless there is \
more to it, and don't create or change to-dos.\n\n\
Then answer with the briefing itself: one to three short paragraphs of plain \
prose on what came in and what matters, by theme rather than email by email, \
without repeating the highlights word for word. No bullet points or lists, no \
greeting, no headings. \
Write in the language of the user's notes, English if there are none.";

#[derive(Serialize, Deserialize)]
struct Highlight {
    external_id: String,
    why: String,
}

/// Write the briefing for the last `days` days and store it.
pub async fn write(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    days: i64,
    char_budget: usize,
) -> Result<()> {
    let _one_at_a_time = LOCK.lock().await;
    let days = days.clamp(1, super::MAX_DAYS);
    let since = super::since(days);
    let mail = super::period_mail(pool, since).await?;
    let up_to = mail.iter().map(|m| m.id).max().unwrap_or(0);

    let mut history = vec![InputItem::Message {
        role: Role::User,
        content: prompt(pool, days, since, &mail, char_budget / 2).await?,
    }];
    let defs = definitions();
    let mut budget = FetchBudget::new(char_budget / 2);
    let mut highlights: Vec<Highlight> = Vec::new();
    let mut summary = String::new();
    for step in 0..TURNS {
        let tools: &[ToolDef] = if step + 1 == TURNS { &[] } else { &defs };
        let response =
            agent::send_with_stall_retry(llm, SYSTEM, &history, tools, step, None, |_| {}).await?;
        summary = text(&response);
        let mut calls = Vec::new();
        for item in response.output {
            match item {
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    history.push(InputItem::FunctionCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    calls.push((call_id, name, arguments));
                }
                OutputItem::Thinking { block } => history.push(InputItem::Thinking { block }),
                _ => {}
            }
        }
        if calls.is_empty() {
            break;
        }
        for (call_id, name, arguments) in calls {
            let output = dispatch(pool, &mut budget, &mut highlights, &name, &arguments).await;
            history.push(InputItem::FunctionCallOutput { call_id, output });
        }
    }
    let summary = summary.trim();
    anyhow::ensure!(!summary.is_empty(), "the briefing came back empty");
    sqlx::query(
        "INSERT OR REPLACE INTO briefings (days, since, generated_at, up_to, summary, highlights_json) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(days)
    .bind(since)
    .bind(Utc::now().timestamp())
    .bind(up_to)
    .bind(summary)
    .bind(serde_json::to_string(&highlights)?)
    .execute(pool)
    .await
    .context("saving the briefing")?;
    Ok(())
}

/// Rewrite the last day's briefing when it is older than a few hours and
/// mail arrived since. Returns whether it did.
pub async fn refresh_if_stale(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    char_budget: usize,
) -> Result<bool> {
    let stored: Option<(i64, i64)> =
        sqlx::query_as("SELECT generated_at, up_to FROM briefings WHERE days = 1")
            .fetch_optional(pool)
            .await?;
    if stored.is_some_and(|(at, _)| Utc::now().timestamp() - at < STALE_SECS) {
        return Ok(false);
    }
    let up_to = stored.map_or(0, |(_, up_to)| up_to);
    let mail = super::period_mail(pool, super::since(1)).await?;
    if !mail.iter().any(|m| m.id > up_to) {
        return Ok(false);
    }
    write(pool, llm, 1, char_budget).await?;
    Ok(true)
}

/// The stored briefing for `days`, its highlights resolved; `mail` is the
/// period's, to count what arrived after it.
pub(super) async fn stored(
    pool: &SqlitePool,
    days: i64,
    mail: &[Mail],
) -> Result<Option<WrittenBriefingDto>> {
    let row: Option<(i64, i64, String, String)> = sqlx::query_as(
        "SELECT generated_at, up_to, summary, highlights_json FROM briefings WHERE days = ?",
    )
    .bind(days)
    .fetch_optional(pool)
    .await
    .context("loading the briefing")?;
    let Some((generated_at, up_to, summary, highlights)) = row else {
        return Ok(None);
    };
    let mut resolved = Vec::new();
    for h in serde_json::from_str::<Vec<Highlight>>(&highlights).unwrap_or_default() {
        if let Some(m) = super::mail_by_external_id(pool, &h.external_id).await? {
            resolved.push(BriefingMailDto {
                reason: h.why,
                ..m.dto()
            });
        }
    }
    Ok(Some(WrittenBriefingDto {
        generated_at,
        summary,
        highlights: resolved,
        newer_mail: mail.iter().filter(|m| m.id > up_to).count() as i64,
    }))
}

/// Start a chat about the briefing for `days`, for the user's follow-up
/// questions. Returns the chat's id.
pub async fn start_chat(pool: &SqlitePool, days: i64) -> Result<i64> {
    crate::chat::store::create_chat(pool, Some("briefing"), Some(days.clamp(1, super::MAX_DAYS)))
        .await
}

/// The chat prompt's account of the briefing for `days`.
pub(crate) async fn seed(pool: &SqlitePool, days: i64) -> Result<Option<String>> {
    let since = super::since(days);
    let mail = super::period_mail(pool, since).await?;
    let mut s = format!(
        "This conversation is about the user's mail over the last {} ({} emails since {}). \
         Use list_messages with that since date to go through it.",
        period(days),
        mail.len(),
        local(since)
    );
    if let Some(b) = stored(pool, days, &mail).await? {
        s.push_str(&format!(
            "\nThe briefing you wrote at {}:\n{}\n",
            local(b.generated_at),
            b.summary
        ));
        if !b.highlights.is_empty() {
            s.push_str("The emails you said to read:\n");
            for h in b.highlights {
                let external_id: String =
                    sqlx::query_scalar("SELECT external_id FROM messages WHERE id = ?")
                        .bind(h.message_id)
                        .fetch_one(pool)
                        .await?;
                s.push_str(&format!(
                    "- {external_id}: {} — {} ({})\n",
                    h.from, h.subject, h.reason
                ));
            }
        }
    }
    Ok(Some(s))
}

fn period(days: i64) -> String {
    if days == 1 {
        "day".to_string()
    } else {
        format!("{days} days")
    }
}

fn local(t: i64) -> String {
    Local
        .timestamp_opt(t, 0)
        .single()
        .map(|t| t.format("%a %Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// The run's opening message: the period's mail, most interesting first,
/// within `char_budget`; noise only counted.
async fn prompt(
    pool: &SqlitePool,
    days: i64,
    since: i64,
    mail: &[Mail],
    char_budget: usize,
) -> Result<String> {
    let mut out = format!(
        "Now: {}. The period: the last {} (since {}).\n",
        local(Utc::now().timestamp()),
        period(days),
        local(since)
    );
    let profile = crate::settings::get_user_profile(pool).await?;
    out.push_str(&format!("\n# The user\n{}\n", profile.display_name));
    if let Some(cp) = profile.custom_prompt.filter(|c| !c.trim().is_empty()) {
        out.push_str(&format!("About them:\n{}\n", cp.trim()));
    }
    if let Some(interests) = crate::memory::interests(pool).await? {
        out.push_str(&format!(
            "What they want to hear about in their mail:\n{interests}\n"
        ));
    }
    let watches = crate::extract::expected::open(pool).await?;
    if !watches.is_empty() {
        out.push_str("\n# Emails they are waiting for\n");
        for w in watches {
            out.push_str(&format!("- {}\n", w.looking_for));
        }
    }
    let todos = super::new_todos(pool, since).await?;
    if !todos.is_empty() {
        out.push_str("\n# To-dos found in this mail\n");
        for t in todos {
            out.push_str(&format!("- A-{}: {}\n", t.action_id, t.title));
        }
    }

    let mut ordered: Vec<&Mail> = mail.iter().collect();
    ordered.sort_by_key(|m| (-m.interest.unwrap_or(1), -m.received_at));
    out.push_str(&format!(
        "\n# The emails ({}), most interesting first\n",
        mail.len()
    ));
    let mut left_out: std::collections::BTreeMap<&str, usize> = Default::default();
    for m in ordered {
        let tag = match (m.category, m.interest) {
            (Some(c), Some(i)) => format!("{} {i}", c.as_str()),
            _ => "not sorted".to_string(),
        };
        let line = format!(
            "{} | {} | from {} | {} | {tag} | {}\n",
            m.external_id,
            local(m.received_at),
            m.from,
            m.subject,
            m.reason
        );
        if m.interest == Some(0) || out.len() + line.len() > char_budget {
            *left_out
                .entry(m.category.map_or("other", |c| c.as_str()))
                .or_default() += 1;
            continue;
        }
        out.push_str(&line);
    }
    if !left_out.is_empty() {
        let counts: Vec<String> = left_out.iter().map(|(c, n)| format!("{n} {c}")).collect();
        out.push_str(&format!(
            "Not listed (noise, or past the space): {}. list_messages finds them.\n",
            counts.join(", ")
        ));
    }
    Ok(out)
}

const READ_TOOLS: [&str; 6] = [
    "search_messages",
    "fetch_messages",
    "list_messages",
    "search_mail_server",
    "list_actions",
    "get_action",
];

fn definitions() -> Vec<ToolDef> {
    let mut defs: Vec<ToolDef> = tools::chat_definitions()
        .into_iter()
        .filter(|d| READ_TOOLS.contains(&d.name.as_str()))
        .collect();
    defs.push(ToolDef::function(
        "highlight".to_string(),
        "Pick an email the user should read, by external_id, with one sentence saying why. \
         Call it once per email, most important first."
            .to_string(),
        json!({
            "type": "object",
            "properties": {
                "external_id": { "type": "string" },
                "why": { "type": "string" }
            },
            "required": ["external_id", "why"]
        }),
    ));
    defs
}

async fn dispatch(
    pool: &SqlitePool,
    budget: &mut FetchBudget,
    highlights: &mut Vec<Highlight>,
    name: &str,
    arguments: &str,
) -> String {
    match name {
        "highlight" => match highlight(pool, highlights, arguments).await {
            Ok(out) => out,
            Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
        },
        "search_mail_server" => match crate::chat::tools::search_mail_server(pool, arguments).await
        {
            Ok(out) => out,
            Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
        },
        name if READ_TOOLS.contains(&name) => {
            let DispatchOutput { output, .. } =
                tools::dispatch(pool, ToolScope::Global, budget, name, arguments).await;
            output
        }
        other => json!({ "error": format!("unknown tool: {other}") }).to_string(),
    }
}

async fn highlight(
    pool: &SqlitePool,
    highlights: &mut Vec<Highlight>,
    arguments: &str,
) -> Result<String> {
    let h: Highlight = serde_json::from_str(arguments).context("parsing highlight args")?;
    let exists: Option<i64> =
        sqlx::query_scalar("SELECT id FROM messages WHERE external_id = ? LIMIT 1")
            .bind(&h.external_id)
            .fetch_optional(pool)
            .await?;
    anyhow::ensure!(exists.is_some(), "no email {}", h.external_id);
    let picked: HashSet<&str> = highlights.iter().map(|h| h.external_id.as_str()).collect();
    if picked.contains(h.external_id.as_str()) {
        return Ok(json!({ "ok": "already highlighted" }).to_string());
    }
    anyhow::ensure!(
        highlights.len() < MAX_HIGHLIGHTS,
        "that's {MAX_HIGHLIGHTS} already: write the briefing now"
    );
    highlights.push(h);
    Ok(json!({ "ok": "highlighted", "count": highlights.len() }).to_string())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{arrive, setup, tag};
    use super::*;
    use crate::test_util::{MockLlm, mock};

    fn call(name: &str, arguments: serde_json::Value) -> OutputItem {
        OutputItem::FunctionCall {
            call_id: format!("c-{name}"),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    #[tokio::test]
    async fn writes_a_briefing_with_the_emails_to_read() -> Result<()> {
        let (_tmp, pool) = setup().await?;
        arrive(&pool, 1, 1, 1, "Dinner on Friday?", 2).await?;
        tag(&pool, 1, "person", 3, "Ana invites you to dinner").await?;
        arrive(&pool, 2, 1, 2, "50% off", 5).await?;
        tag(&pool, 2, "newsletter", 0, "a sale").await?;

        let llm = MockLlm::new(vec![
            mock::turn(vec![call(
                "fetch_messages",
                json!({"external_ids": ["<m1@x>"]}),
            )]),
            mock::turn(vec![
                call(
                    "highlight",
                    json!({"external_id": "<m1@x>", "why": "Ana wants an answer by Thursday"}),
                ),
                call(
                    "highlight",
                    json!({"external_id": "<nope@x>", "why": "made up"}),
                ),
            ]),
            mock::no_tools("Ana invited you to dinner on Friday; the rest was a sale."),
        ]);
        write(&pool, &llm, 1, 20_000).await?;

        let day = super::super::load(&pool, 1).await?;
        let b = day.written.expect("stored");
        assert!(b.summary.contains("dinner"), "{}", b.summary);
        assert_eq!(
            b.highlights.len(),
            1,
            "an email that isn't there is refused"
        );
        assert_eq!(b.highlights[0].subject, "Dinner on Friday?");
        assert_eq!(b.highlights[0].reason, "Ana wants an answer by Thursday");
        assert_eq!(b.newer_mail, 0);

        // Fresh: sync leaves it alone, even with new mail.
        arrive(&pool, 3, 1, 1, "Re: Dinner", 0).await?;
        assert!(!refresh_if_stale(&pool, &MockLlm::new(vec![]), 20_000).await?);
        assert_eq!(
            super::super::load(&pool, 1)
                .await?
                .written
                .unwrap()
                .newer_mail,
            1
        );
        // Stale: rewritten.
        sqlx::query("UPDATE briefings SET generated_at = 0")
            .execute(&pool)
            .await?;
        let llm = MockLlm::new(vec![mock::no_tools("Ana wrote again.")]);
        assert!(refresh_if_stale(&pool, &llm, 20_000).await?);
        let b = super::super::load(&pool, 1).await?.written.unwrap();
        assert_eq!((b.summary.as_str(), b.newer_mail), ("Ana wrote again.", 0));
        // Stale but nothing new: no model call.
        sqlx::query("UPDATE briefings SET generated_at = 0")
            .execute(&pool)
            .await?;
        assert!(!refresh_if_stale(&pool, &MockLlm::new(vec![]), 20_000).await?);
        Ok(())
    }

    #[tokio::test]
    async fn a_chat_about_the_briefing_knows_it() -> Result<()> {
        let (_tmp, pool) = setup().await?;
        arrive(&pool, 1, 1, 1, "Dinner on Friday?", 2).await?;
        let llm = MockLlm::new(vec![
            mock::turn(vec![call(
                "highlight",
                json!({"external_id": "<m1@x>", "why": "Ana asks"}),
            )]),
            mock::no_tools("Ana invited you to dinner."),
        ]);
        write(&pool, &llm, 3, 20_000).await?;
        let chat = start_chat(&pool, 3).await?;
        let prompt = crate::chat::prompt::build_system_prompt(&pool, chat).await?;
        assert!(prompt.contains("last 3 days"), "{prompt}");
        assert!(prompt.contains("Ana invited you to dinner."), "{prompt}");
        assert!(
            prompt.contains("<m1@x>: Ana — Dinner on Friday?"),
            "{prompt}"
        );
        Ok(())
    }
}
