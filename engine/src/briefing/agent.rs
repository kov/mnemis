//! The written briefing: an agent run over a whole period that writes what
//! came in and what matters, and picks the emails worth reading. It starts from the
//! period's tagged mail, reads what deserves it, and can look up earlier mail
//! (locally or on the server) to explain an email's context. It runs on
//! demand, and the apps call [`refresh_if_stale`] for the user's chosen
//! period when they are in the foreground; a run can take minutes, so sync
//! never does.
//!
//! One briefing is kept per period length, with the newest email it saw, so
//! the briefing can say how much mail arrived since.

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
to-dos found in them, the open to-dos with dates and the emails the user is \
waiting for.\n\n\
Look into what deserves it before you write: read emails with fetch_messages \
when the sender and subject aren't enough, and use search_messages (or \
search_mail_server for mail older than the last 90 days) to find what an email \
follows from: an answer to something the user sent, the next step of an \
order or a process, a change from what was said before. Call highlight for \
each email the user should read, most important first, at most 8, with one \
sentence saying why. Don't highlight what is already a to-do unless there is \
more to it, and don't create or change to-dos.\n\n\
Emails marked SUSPICIOUS failed checks that point to phishing (a forged date \
or sender, a misleading link, a look-alike sender); the mark says which, and \
what an earlier look found. Never pass on what one claims as fact. For one \
that would otherwise matter and hasn't been looked into, look into it: fetch \
it, check the sender's real address and domain and where its links lead, and \
compare it with earlier mail from the genuine sender (search_messages). Then \
call record_suspicion with your verdict and the concrete evidence; the user \
sees it with the email's warning. Leave mail you judge phishing out of the \
highlights and the briefing entirely: the app lists it under Suspicious. \
Mail you find genuine you treat like any other. Call record_suspicion too \
when an unmarked email turns out to be phishing.\n\n\
Then answer with the briefing itself: one to three short paragraphs of plain \
prose on what came in and what matters, by theme rather than email by email, \
without repeating the highlights word for word. When to-dos are overdue or \
due in the next two days, say so first with their dates. Refer to a to-do by \
its title, never by its A-N id, written as a markdown link to it: \
[Pay the plumber](A-12). No bullet points or lists, no greeting, no \
headings. When the user adds instructions for this briefing, follow them. \
Write in the language of the user's notes, English if there are none.";

#[derive(Serialize, Deserialize)]
struct Highlight {
    external_id: String,
    why: String,
}

/// Write the briefing for the last `days` days and store it. `instructions`
/// are the user's, for this one (what to look at, what to leave out).
pub async fn write(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    days: i64,
    instructions: Option<&str>,
    char_budget: usize,
) -> Result<()> {
    let _one_at_a_time = LOCK.lock().await;
    let days = days.clamp(1, super::MAX_DAYS);
    let since = super::since(days);
    let mail = super::period_mail(pool, since).await?;
    let up_to = mail.iter().map(|m| m.id).max().unwrap_or(0);

    let mut history = vec![InputItem::Message {
        role: Role::User,
        content: prompt(pool, days, since, &mail, instructions, char_budget / 2).await?,
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
    let summary = link_todos(summary.trim());
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

/// The summary's to-do links, `[title](A-12)`, as app links
/// (`mnemis://action/12`).
fn link_todos(summary: &str) -> String {
    summary
        .replace("](A-", "](mnemis://action/")
        .replace("](a-", "](mnemis://action/")
}

/// Write the summary for the last `days` days (the user's chosen period)
/// when there is none, or it is older than a few hours and mail arrived
/// since. Returns whether it did.
pub async fn refresh_if_stale(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    days: i64,
    char_budget: usize,
) -> Result<bool> {
    let days = days.clamp(1, super::MAX_DAYS);
    let stored: Option<(i64, i64)> =
        sqlx::query_as("SELECT generated_at, up_to FROM briefings WHERE days = ?")
            .bind(days)
            .fetch_optional(pool)
            .await?;
    if stored.is_some_and(|(at, _)| Utc::now().timestamp() - at < STALE_SECS) {
        return Ok(false);
    }
    let up_to = stored.map_or(0, |(_, up_to)| up_to);
    let mail = super::period_mail(pool, super::since(days)).await?;
    if !mail.iter().any(|m| m.id > up_to) {
        return Ok(false);
    }
    write(pool, llm, days, None, char_budget).await?;
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
                ..m.dto_with_warnings(pool).await?
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
    instructions: Option<&str>,
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
    let dated = dated_todos(pool).await?;
    if !dated.is_empty() {
        out.push_str("\n# Open to-dos with dates, soonest first\n");
        out.push_str(&dated);
    }
    if let Some(i) = instructions.map(str::trim).filter(|i| !i.is_empty()) {
        out.push_str(&format!(
            "\n# The user's instructions for this briefing\n{i}\n"
        ));
    }

    let ids: Vec<i64> = mail.iter().map(|m| m.id).collect();
    let warnings = super::warnings(pool, &ids).await?;
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
        let flagged = warnings.get(&m.id);
        let mut line = format!(
            "{} | {} | from {} | {} | {tag} | {}",
            m.external_id,
            local(m.received_at),
            m.from,
            m.subject,
            m.reason
        );
        if let Some(w) = flagged {
            line.push_str(&format!(" | SUSPICIOUS: {}", w.join("; ")));
        }
        line.push('\n');
        // Suspicious mail is always listed: the user may need warning.
        if flagged.is_none() && (m.interest == Some(0) || out.len() + line.len() > char_budget) {
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

/// The open to-dos with a due date, one line each, saying how far off it is.
async fn dated_todos(pool: &SqlitePool) -> Result<String> {
    let rows: Vec<(i64, String, i64)> = sqlx::query_as(
        "SELECT id, title, due_at FROM actions \
         WHERE due_at IS NOT NULL AND status IN ('pending', 'auto_claimed', 'claimed') \
         ORDER BY due_at LIMIT 30",
    )
    .fetch_all(pool)
    .await
    .context("listing dated to-dos")?;
    let today = Local::now().date_naive();
    let mut out = String::new();
    for (id, title, due_at) in rows {
        // The day is stored as midnight UTC of the date.
        let Some(due) = chrono::DateTime::<Utc>::from_timestamp(due_at, 0) else {
            continue;
        };
        let due = due.date_naive();
        let when = match (due - today).num_days() {
            n if n < -1 => format!("overdue by {} days", -n),
            -1 => "overdue since yesterday".to_string(),
            0 => "due today".to_string(),
            1 => "due tomorrow".to_string(),
            n => format!("due in {n} days"),
        };
        out.push_str(&format!(
            "- A-{id}: {title} — {} ({when})\n",
            due.format("%a %Y-%m-%d")
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
    defs.push(ToolDef::function(
        "record_suspicion".to_string(),
        "Record what looking into a suspicious email showed: whether it is phishing, and \
         the concrete evidence (the sender's real domain, where the links lead, how it \
         differs from the genuine sender's mail). The user sees it with the email's warning."
            .to_string(),
        json!({
            "type": "object",
            "properties": {
                "external_id": { "type": "string" },
                "phishing": { "type": "boolean" },
                "finding": { "type": "string", "description": "one or two sentences" }
            },
            "required": ["external_id", "phishing", "finding"]
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
    let result = match name {
        "highlight" => highlight(pool, highlights, arguments).await,
        "record_suspicion" => record_suspicion(pool, arguments).await,
        "search_mail_server" => crate::chat::tools::search_mail_server(pool, arguments).await,
        name if READ_TOOLS.contains(&name) => {
            let DispatchOutput { output, .. } =
                tools::dispatch(pool, ToolScope::Global, budget, name, arguments).await;
            Ok(output)
        }
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    };
    result.unwrap_or_else(|e| json!({ "error": format!("{e:#}") }).to_string())
}

/// Pick an email. Mail judged phishing is refused, and flagged mail nobody
/// looked into yet is sent back to be looked into first.
async fn highlight(
    pool: &SqlitePool,
    highlights: &mut Vec<Highlight>,
    arguments: &str,
) -> Result<String> {
    let h: Highlight = serde_json::from_str(arguments).context("parsing highlight args")?;
    let id = message_id(pool, &h.external_id).await?;
    if highlights.iter().any(|p| p.external_id == h.external_id) {
        return Ok(json!({ "ok": "already highlighted" }).to_string());
    }
    if crate::mail::suspicion::judged_phishing(pool, &[id])
        .await?
        .contains(&id)
    {
        anyhow::bail!(
            "not highlighted: it was judged phishing. Leave it out of the highlights and the \
             briefing; the app lists it under Suspicious with the finding."
        );
    }
    let looked_into: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM suspicion_findings WHERE external_id = ?")
            .bind(&h.external_id)
            .fetch_optional(pool)
            .await?;
    if let Some(w) = super::warnings(pool, &[id]).await?.remove(&id)
        && looked_into.is_none()
    {
        anyhow::bail!(
            "not highlighted: it is flagged as suspicious ({}). Look into it (fetch it, check \
             the sender's real address and where its links lead, compare it with earlier mail \
             from the genuine sender) and call record_suspicion with your verdict. Highlight \
             it only if you find it genuine.",
            w.join("; ")
        );
    }
    anyhow::ensure!(
        highlights.len() < MAX_HIGHLIGHTS,
        "that's {MAX_HIGHLIGHTS} already: write the briefing now"
    );
    highlights.push(h);
    Ok(json!({ "ok": "highlighted", "count": highlights.len() }).to_string())
}

#[derive(Deserialize)]
struct SuspicionArgs {
    external_id: String,
    phishing: bool,
    finding: String,
}

async fn record_suspicion(pool: &SqlitePool, arguments: &str) -> Result<String> {
    let args: SuspicionArgs =
        serde_json::from_str(arguments).context("parsing record_suspicion args")?;
    message_id(pool, &args.external_id).await?;
    anyhow::ensure!(!args.finding.trim().is_empty(), "say what you found");
    crate::mail::suspicion::save_finding(pool, &args.external_id, args.phishing, &args.finding)
        .await?;
    Ok(json!({ "ok": "recorded" }).to_string())
}

async fn message_id(pool: &SqlitePool, external_id: &str) -> Result<i64> {
    let id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM messages WHERE external_id = ? LIMIT 1")
            .bind(external_id)
            .fetch_optional(pool)
            .await?;
    id.with_context(|| format!("no email {external_id}"))
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
        write(&pool, &llm, 1, None, 20_000).await?;

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
        assert!(!refresh_if_stale(&pool, &MockLlm::new(vec![]), 1, 20_000).await?);
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
        assert!(refresh_if_stale(&pool, &llm, 1, 20_000).await?);
        let b = super::super::load(&pool, 1).await?.written.unwrap();
        assert_eq!((b.summary.as_str(), b.newer_mail), ("Ana wrote again.", 0));
        // Stale but nothing new: no model call.
        sqlx::query("UPDATE briefings SET generated_at = 0")
            .execute(&pool)
            .await?;
        assert!(!refresh_if_stale(&pool, &MockLlm::new(vec![]), 1, 20_000).await?);
        Ok(())
    }

    #[tokio::test]
    async fn the_agent_sees_due_to_dos_and_the_users_instructions() -> Result<()> {
        let (_tmp, pool) = setup().await?;
        // Due days are stored as midnight UTC of the date.
        let day = |offset: i64| {
            let date = Local::now().date_naive() + chrono::Duration::days(offset);
            date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()
        };
        for (id, title, status, due) in [
            (1, "Pay the plumber", "claimed", day(-3)),
            (2, "Send the form", "pending", day(0)),
            (3, "Book flights", "pending", day(1)),
            (4, "Already done", "done", day(-1)),
        ] {
            sqlx::query(
                "INSERT INTO actions (id, title, confidence, status, extracted_at, due_at) \
                 VALUES (?, ?, 'high', ?, 0, ?)",
            )
            .bind(id)
            .bind(title)
            .bind(status)
            .bind(due)
            .execute(&pool)
            .await?;
        }
        let since = super::super::since(1);
        let prompt = prompt(&pool, 1, since, &[], Some("Skip the newsletters"), 20_000).await?;
        assert!(prompt.contains("Pay the plumber"), "{prompt}");
        assert!(prompt.contains("(overdue by 3 days)"), "{prompt}");
        assert!(prompt.contains("Send the form"), "{prompt}");
        assert!(prompt.contains("(due today)"), "{prompt}");
        assert!(prompt.contains("(due tomorrow)"), "{prompt}");
        assert!(!prompt.contains("Already done"), "{prompt}");
        assert!(prompt.contains("Skip the newsletters"), "{prompt}");
        Ok(())
    }

    /// Dated days after it arrived: a forged date.
    async fn forge(pool: &SqlitePool, id: i64) -> Result<()> {
        sqlx::query("UPDATE messages SET posted_at = received_at + 5 * 86400 WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(())
    }

    async fn run_tool(pool: &SqlitePool, name: &str, arguments: serde_json::Value) -> String {
        dispatch(
            pool,
            &mut FetchBudget::unlimited(),
            &mut Vec::new(),
            name,
            &arguments.to_string(),
        )
        .await
    }

    #[tokio::test]
    async fn suspicious_mail_is_looked_into_and_phishing_left_out() -> Result<()> {
        let (_tmp, pool) = setup().await?;
        arrive(&pool, 1, 1, 2, "Your loan payment is pending", 2).await?;
        tag(&pool, 1, "transactional", 3, "a payment is pending").await?;
        forge(&pool, 1).await?;
        arrive(&pool, 2, 1, 1, "Dinner on Friday?", 2).await?;
        tag(&pool, 2, "person", 3, "Ana invites you").await?;
        forge(&pool, 2).await?;
        let mail = super::super::period_mail(&pool, super::super::since(1)).await?;
        let prompt = prompt(&pool, 1, super::super::since(1), &mail, None, 20_000).await?;
        assert!(prompt.contains("| SUSPICIOUS: "), "{prompt}");

        // Flagged and not looked into: sent back.
        let pick = |id: &str| json!({"external_id": id, "why": "worth it"});
        let out = run_tool(&pool, "highlight", pick("<m1@x>")).await;
        assert!(out.contains("record_suspicion"), "{out}");

        // Judged phishing: never highlighted, out of Worth reading, and the
        // finding goes with its warning everywhere, chat included.
        let finding = json!({"external_id": "<m1@x>", "phishing": true,
                             "finding": "sent from mercadopago-cobranca.top, not mercadopago.com"});
        run_tool(&pool, "record_suspicion", finding).await;
        let out = run_tool(&pool, "highlight", pick("<m1@x>")).await;
        assert!(out.contains("judged phishing"), "{out}");
        let fetched = run_tool(&pool, "fetch_messages", json!({"external_ids": ["<m1@x>"]})).await;
        assert!(
            fetched.contains("likely phishing: sent from mercadopago-cobranca.top"),
            "{fetched}"
        );

        // Looked into and genuine: highlighted like any other.
        let finding = json!({"external_id": "<m2@x>", "phishing": false,
                             "finding": "Ana's usual address; her phone's clock is off"});
        run_tool(&pool, "record_suspicion", finding).await;
        let llm = MockLlm::new(vec![
            mock::turn(vec![call("highlight", pick("<m2@x>"))]),
            mock::no_tools("Ana asked about dinner; see [Answer Ana](A-7)."),
        ]);
        write(&pool, &llm, 1, None, 20_000).await?;
        let day = super::super::load(&pool, 1).await?;
        let worth: Vec<&str> = day
            .worth_reading
            .iter()
            .map(|m| m.subject.as_str())
            .collect();
        assert_eq!(worth, ["Dinner on Friday?"]);
        assert!(
            day.suspicious
                .iter()
                .any(|m| m.warnings.iter().any(|w| w.contains("likely phishing")))
        );
        let written = day.written.unwrap();
        assert_eq!(written.highlights[0].subject, "Dinner on Friday?");
        assert!(
            written.highlights[0]
                .warnings
                .iter()
                .any(|w| w.contains("probably genuine"))
        );
        assert!(
            written.summary.contains("[Answer Ana](mnemis://action/7)"),
            "{}",
            written.summary
        );
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
        write(&pool, &llm, 3, None, 20_000).await?;
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
