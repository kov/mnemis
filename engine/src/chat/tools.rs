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
        "draft_email" => DispatchOutput {
            output: match draft_email(pool, chat_id, arguments).await {
                Ok(json) => json,
                Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
            },
            recorded_action: false,
        },
        _ => tools::dispatch(pool, ToolScope::Global, fetch_budget, name, arguments).await,
    }
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
        "from": draft.from,
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
