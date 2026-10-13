//! Chat-only tools layered over the shared extraction tool set.
//!
//! The shared tools (search/fetch ingested messages, action CRUD) are
//! dispatched by [`crate::extract::tools::dispatch`]. This module adds the two
//! tools that need the *conversation* in scope — searching and recalling the
//! chat's own earlier turns — which the model uses to recover wording that the
//! compaction summary ([`crate::chat::compact`]) condensed away. They're
//! dispatched here, where `chat_id` is known; anything else falls through to
//! the shared dispatcher with `Global` scope.

use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;

use crate::chat::store;
use crate::extract::tools::{self, DispatchOutput, FetchBudget, ToolScope};

/// Per-call cap on total recalled text, so a `recall_turns` over fat turns
/// can't blow the context window back open right after we compacted it.
const RECALL_CHAR_CAP: usize = 12_000;
/// Per-turn cap within a recall, so one huge turn can't eat the whole budget.
const RECALL_PER_TURN_CAP: usize = 4_000;

#[derive(Deserialize)]
struct SearchConversationArgs {
    query: String,
}

#[derive(Deserialize)]
struct RecallTurnsArgs {
    turn_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct DraftEmailArgs {
    kind: String,
    message_external_id: Option<String>,
    to: Option<String>,
    cc: Option<String>,
    subject: Option<String>,
    body: String,
}

/// Dispatch a chat tool call. The conversation-scoped recall tools are handled
/// here (they need `chat_id`); every other tool falls through to the shared
/// extraction dispatcher at `Global` scope (the chat ranges over all sources).
pub(crate) async fn dispatch(
    pool: &SqlitePool,
    chat_id: i64,
    fetch_budget: &mut FetchBudget,
    name: &str,
    arguments: &str,
) -> DispatchOutput {
    match name {
        "search_conversation" => DispatchOutput {
            output: search_conversation(pool, chat_id, arguments).await,
            recorded_action: false,
        },
        "recall_turns" => DispatchOutput {
            output: recall_turns(pool, chat_id, arguments).await,
            recorded_action: false,
        },
        "search_mail_server" => DispatchOutput {
            output: match search_mail_server(pool, arguments).await {
                Ok(json) => json,
                Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
            },
            recorded_action: false,
        },
        "draft_email" => DispatchOutput {
            output: match draft_email(pool, chat_id, arguments).await {
                Ok(json) => json,
                Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
            },
            recorded_action: false,
        },
        "watch_for_mail" | "cancel_watch" => DispatchOutput {
            output: match watch_tool(pool, chat_id, name, arguments).await {
                Ok(json) => json,
                Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
            },
            recorded_action: false,
        },
        "set_interests" => DispatchOutput {
            output: match set_interests(pool, arguments).await {
                Ok(json) => json,
                Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
            },
            recorded_action: false,
        },
        _ => tools::dispatch(pool, ToolScope::Global, fetch_budget, name, arguments).await,
    }
}

#[derive(Deserialize)]
struct WatchForMailArgs {
    looking_for: String,
    then: String,
    days: Option<i64>,
}

#[derive(Deserialize)]
struct CancelWatchArgs {
    watch_id: String,
}

/// Save or cancel a watch on incoming mail (see `extract::expected`).
async fn watch_tool(
    pool: &SqlitePool,
    chat_id: i64,
    name: &str,
    arguments: &str,
) -> anyhow::Result<String> {
    use crate::extract::expected;
    use anyhow::Context;
    if name == "watch_for_mail" {
        let args: WatchForMailArgs =
            serde_json::from_str(arguments).context("parsing watch_for_mail args")?;
        let days = args.days.unwrap_or(expected::DEFAULT_DAYS);
        let id = expected::create(
            pool,
            Some(chat_id),
            &args.looking_for,
            &args.then,
            Some(days),
        )
        .await?;
        return Ok(json!({
            "watch_id": format!("W-{id}"),
            "waits": if days == 0 { "until cancelled".to_string() } else { format!("{days} days") },
        })
        .to_string());
    }
    let args: CancelWatchArgs =
        serde_json::from_str(arguments).context("parsing cancel_watch args")?;
    let id: i64 = args
        .watch_id
        .trim()
        .trim_start_matches(['W', 'w'])
        .trim_start_matches('-')
        .parse()
        .context("watch_id must look like W-N")?;
    anyhow::ensure!(expected::cancel(pool, id).await?, "no open watch W-{id}");
    Ok(json!({ "cancelled": format!("W-{id}") }).to_string())
}

#[derive(Deserialize)]
struct SetInterestsArgs {
    interests: String,
}

/// Replace the interests note the briefing judges mail by.
async fn set_interests(pool: &SqlitePool, arguments: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let args: SetInterestsArgs =
        serde_json::from_str(arguments).context("parsing set_interests args")?;
    crate::memory::save_interests(pool, &args.interests).await?;
    Ok(json!({ "saved": crate::memory::interests(pool).await? }).to_string())
}

#[derive(Deserialize)]
struct SearchMailServerArgs {
    from: Option<String>,
    subject: Option<String>,
    text: Option<String>,
    since: Option<String>,
    before: Option<String>,
    limit: Option<usize>,
}

/// Search the mail servers, then describe the hits from the rows the search
/// stored.
pub(crate) async fn search_mail_server(
    pool: &SqlitePool,
    arguments: &str,
) -> anyhow::Result<String> {
    use anyhow::Context;
    let args: SearchMailServerArgs =
        serde_json::from_str(arguments).context("parsing search_mail_server args")?;
    let day = |s: &Option<String>, name: &str| -> anyhow::Result<Option<chrono::NaiveDate>> {
        s.as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                chrono::NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d")
                    .with_context(|| format!("{name} must be YYYY-MM-DD"))
            })
            .transpose()
    };
    let text = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    let criteria = crate::mail::search::Criteria {
        since: day(&args.since, "since")?,
        before: day(&args.before, "before")?,
        from: text(args.from),
        subject: text(args.subject),
        text: text(args.text),
    };
    anyhow::ensure!(
        !criteria.is_empty(),
        "give at least one of from, subject, text, since or before"
    );
    let limit = args.limit.unwrap_or(20).clamp(1, 50);
    let (ids, errors) = crate::orchestrator::search_mail_server(pool, &criteria, limit).await?;
    let mut hits = Vec::new();
    for id in ids {
        let (external_id, posted_at, author, address, subject, folder): (
            String,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT m.external_id, m.posted_at, p.display_name, p.external_id, m.subject, \
                    c.external_id \
             FROM messages m JOIN channels c ON c.id = m.channel_id \
             LEFT JOIN people p ON p.id = m.author_id WHERE m.id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await?;
        hits.push(json!({
            "external_id": external_id,
            "posted_at": chrono::DateTime::<chrono::Utc>::from_timestamp(posted_at, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default(),
            "author": author.or(address),
            "subject": subject,
            "folder": crate::source::imap_utf7::decode_mailbox_name(&folder),
        }));
    }
    let mut out = json!({ "messages": hits });
    if !errors.is_empty() {
        out["errors"] = json!(errors);
    }
    Ok(out.to_string())
}

async fn search_conversation(pool: &SqlitePool, chat_id: i64, arguments: &str) -> String {
    let args: SearchConversationArgs = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => {
            return json!({ "error": format!("parsing search_conversation args: {e}") })
                .to_string();
        }
    };
    match store::search_turns(pool, chat_id, &args.query, 10).await {
        Ok(hits) => {
            let matches: Vec<_> = hits
                .into_iter()
                .map(|(id, role, created_at, snippet)| {
                    json!({
                        "turn_id": id,
                        "role": role,
                        "at": chrono::DateTime::<chrono::Utc>::from_timestamp(created_at, 0)
                            .map(|d| d.to_rfc3339())
                            .unwrap_or_default(),
                        "snippet": snippet,
                    })
                })
                .collect();
            json!({ "matches": matches }).to_string()
        }
        Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
    }
}

async fn recall_turns(pool: &SqlitePool, chat_id: i64, arguments: &str) -> String {
    let args: RecallTurnsArgs = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => return json!({ "error": format!("parsing recall_turns args: {e}") }).to_string(),
    };
    let rows = match store::load_turn_contents(pool, chat_id, &args.turn_ids).await {
        Ok(r) => r,
        Err(e) => return json!({ "error": format!("{e:#}") }).to_string(),
    };

    let mut used = 0usize;
    let mut truncated = false;
    let mut turns = Vec::new();
    for row in rows {
        if used >= RECALL_CHAR_CAP {
            truncated = true;
            break;
        }
        let content = row.content.unwrap_or_default();
        // Full fidelity (unlike `snippet`, which collapses newlines) — just
        // bounded by the per-turn and running caps.
        let room = (RECALL_CHAR_CAP - used).min(RECALL_PER_TURN_CAP);
        let (text, cut) = cap_text(&content, room);
        truncated |= cut;
        used += text.chars().count();
        turns.push(json!({
            "turn_id": row.id,
            "role": row.role,
            "tool_name": row.tool_name,
            "content": text,
        }));
    }
    json!({ "turns": turns, "truncated": truncated }).to_string()
}

/// Write a draft for the user to review and send: prefilled like one they
/// start themselves, with the agent's text above the quoted original.
async fn draft_email(pool: &SqlitePool, chat_id: i64, arguments: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    use mnemis_types::DraftKind;

    let args: DraftEmailArgs =
        serde_json::from_str(arguments).context("parsing draft_email args")?;
    let kind =
        DraftKind::parse(&args.kind).with_context(|| format!("unknown kind {:?}", args.kind))?;
    let message_id = match (kind, args.message_external_id.as_deref()) {
        (DraftKind::New, _) => None,
        (_, None) => anyhow::bail!("message_external_id is required for a {}", args.kind),
        (_, Some(external_id)) => {
            let row: Option<(i64,)> = sqlx::query_as(
                "SELECT id FROM messages WHERE external_id = ? ORDER BY posted_at DESC LIMIT 1",
            )
            .bind(external_id)
            .fetch_optional(pool)
            .await?;
            Some(row.with_context(|| format!("no message {external_id}"))?.0)
        }
    };
    let postbox = crate::mail::Network { pool };
    let id =
        crate::mail::start_draft(pool, &postbox, kind, message_id, Some(chat_id), true).await?;
    let mut draft = crate::mail::get_draft(pool, id).await?;
    if let Some(to) = args.to.filter(|t| !t.trim().is_empty()) {
        draft.to = to;
    }
    if let Some(cc) = args.cc.filter(|c| !c.trim().is_empty()) {
        draft.cc = cc;
    }
    if let Some(subject) = args.subject.filter(|s| !s.trim().is_empty()) {
        draft.subject = subject;
    }
    draft.body = format!("{}{}", args.body.trim_end(), draft.body);
    crate::mail::save_draft(pool, &draft).await?;
    Ok(json!({
        "draft_id": id,
        "from": draft.from_address,
        "to": draft.to,
        "cc": draft.cc,
        "subject": draft.subject,
        "status": "shown to the user to review and send; it has not been sent",
    })
    .to_string())
}

/// Truncate to `max` chars, reporting whether anything was cut. Preserves
/// newlines (recall needs fidelity, not a one-line preview).
fn cap_text(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        (s.to_string(), false)
    } else {
        (s.chars().take(max).collect(), true)
    }
}
