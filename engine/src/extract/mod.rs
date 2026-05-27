use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use tracing::{debug, warn};

use crate::llm::{InputItem, LlmTransport, OutputItem, Role};

pub mod prompt;
pub mod tools;

use prompt::{ExistingAction, PromptInputs, WindowMessage};
use tools::ExtractionScope;

pub const PROMPT_VERSION: i64 = 1;
const WINDOW_LIMIT: i64 = 100;
const MAX_AGENT_TURNS: usize = 20;

#[derive(Debug)]
pub struct ExtractionOutcome {
    pub result: &'static str,
    pub actions_created: usize,
    pub up_to_message_id: Option<i64>,
    pub summary: Option<String>,
}

pub async fn extract_for_channel(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    channel_id: i64,
    model_name: &str,
) -> Result<ExtractionOutcome> {
    let channel = load_channel(pool, channel_id).await?;
    let watermark = load_watermark(pool, channel_id).await?;
    let window = load_window(pool, channel_id, watermark).await?;

    if window.is_empty() {
        record_run(
            pool,
            channel_id,
            None,
            model_name,
            "no_activity",
            0,
            0,
            None,
        )
        .await?;
        return Ok(ExtractionOutcome {
            result: "no_activity",
            actions_created: 0,
            up_to_message_id: watermark,
            summary: None,
        });
    }

    let up_to = window.last().map(|m| m.id);
    let existing = load_existing_actions(pool, channel_id).await?;
    let profile = load_user_profile_for(pool, &channel.source_kind).await?;

    let window_for_prompt: Vec<WindowMessage> = window
        .iter()
        .map(|m| WindowMessage {
            external_id: m.external_id.clone(),
            posted_at: DateTime::<Utc>::from_timestamp(m.posted_at, 0).unwrap_or_else(Utc::now),
            author: m.author_display.clone().unwrap_or_else(|| "?".to_string()),
            subject: m.subject.clone(),
            body: m.body.clone(),
        })
        .collect();

    let inputs = PromptInputs {
        source_kind: &channel.source_kind,
        channel_name: &channel.name,
        user_display_name: profile.display_name.as_deref().unwrap_or("(unknown)"),
        user_identifiers: &profile.identifiers,
        custom_prompt: profile.custom_prompt.as_deref(),
        current_time: Utc::now(),
        existing_actions: &existing,
        window: &window_for_prompt,
    };
    let system_prompt = prompt::build(&inputs);

    let scope = ExtractionScope {
        source_id: channel.source_id,
        channel_id,
    };
    let outcome = run_agent_loop(pool, llm, scope, system_prompt).await;

    let (result, actions_created, summary) = match outcome {
        Ok((created, summary)) => ("ok", created, summary),
        Err(e) => {
            warn!(error = %e, channel_id, "extraction agent failed");
            ("error", 0, Some(e.to_string()))
        }
    };

    record_run(
        pool,
        channel_id,
        up_to,
        model_name,
        result,
        actions_created,
        0,
        summary.clone(),
    )
    .await?;

    Ok(ExtractionOutcome {
        result,
        actions_created,
        up_to_message_id: up_to,
        summary,
    })
}

struct ChannelInfo {
    source_id: i64,
    source_kind: String,
    name: String,
}

async fn load_channel(pool: &SqlitePool, channel_id: i64) -> Result<ChannelInfo> {
    let (source_id, source_kind, name): (i64, String, String) = sqlx::query_as(
        "SELECT c.source_id, s.kind, c.name \
         FROM channels c JOIN sources s ON s.id = c.source_id \
         WHERE c.id = ?",
    )
    .bind(channel_id)
    .fetch_one(pool)
    .await
    .context("loading channel")?;
    Ok(ChannelInfo {
        source_id,
        source_kind,
        name,
    })
}

async fn load_watermark(pool: &SqlitePool, channel_id: i64) -> Result<Option<i64>> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        "SELECT up_to_message_id FROM extraction_runs \
         WHERE channel_id = ? AND result IN ('ok', 'no_activity') \
         ORDER BY ran_at DESC LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(opt,)| opt))
}

struct WindowRow {
    id: i64,
    external_id: String,
    posted_at: i64,
    subject: Option<String>,
    body: String,
    author_display: Option<String>,
}

#[allow(clippy::type_complexity)]
async fn load_window(
    pool: &SqlitePool,
    channel_id: i64,
    watermark: Option<i64>,
) -> Result<Vec<WindowRow>> {
    let rows: Vec<(i64, String, i64, Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT m.id, m.external_id, m.posted_at, m.subject, m.body, p.display_name \
         FROM messages m \
         LEFT JOIN people p ON p.id = m.author_id \
         WHERE m.channel_id = ? AND m.id > ? \
         ORDER BY m.id ASC LIMIT ?",
    )
    .bind(channel_id)
    .bind(watermark.unwrap_or(0))
    .bind(WINDOW_LIMIT)
    .fetch_all(pool)
    .await
    .context("loading window")?;
    Ok(rows
        .into_iter()
        .map(
            |(id, external_id, posted_at, subject, body, author_display)| WindowRow {
                id,
                external_id,
                posted_at,
                subject,
                body,
                author_display,
            },
        )
        .collect())
}

async fn load_existing_actions(pool: &SqlitePool, channel_id: i64) -> Result<Vec<ExistingAction>> {
    let rows: Vec<(i64, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT DISTINCT a.id, a.title, a.details, a.due_at \
         FROM actions a \
         JOIN action_evidence ae ON ae.action_id = a.id \
         JOIN messages m ON m.id = ae.message_id \
         WHERE m.channel_id = ? AND a.status IN ('pending', 'auto_claimed', 'claimed') \
         ORDER BY a.extracted_at DESC LIMIT 50",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await
    .context("loading existing actions")?;
    Ok(rows
        .into_iter()
        .map(|(id, title, details, due_at)| ExistingAction {
            id,
            title,
            details,
            due_at: due_at.and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0)),
        })
        .collect())
}

struct UserProfile {
    display_name: Option<String>,
    custom_prompt: Option<String>,
    identifiers: Vec<String>,
}

async fn load_user_profile_for(pool: &SqlitePool, source_kind: &str) -> Result<UserProfile> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT display_name, custom_prompt FROM user_profile WHERE id = 1")
            .fetch_optional(pool)
            .await?;
    let (display_name, custom_prompt) = match row {
        Some((n, c)) => (Some(n), c),
        None => (None, None),
    };

    // Map source kind to identifier kinds the user might have on it.
    let identifier_kinds: &[&str] = match source_kind {
        "imap" => &["email"],
        "mattermost" => &["mattermost_handle", "email"],
        "discord" => &["discord_id"],
        _ => &[],
    };

    let mut identifiers = Vec::new();
    if !identifier_kinds.is_empty() {
        for kind in identifier_kinds {
            let rows: Vec<(String,)> = sqlx::query_as(
                "SELECT ci.value FROM contact_identifiers ci \
                 JOIN contacts c ON c.id = ci.contact_id \
                 WHERE c.relationship = 'self' AND ci.kind = ?",
            )
            .bind(*kind)
            .fetch_all(pool)
            .await?;
            identifiers.extend(rows.into_iter().map(|(v,)| v));
        }
    }

    Ok(UserProfile {
        display_name,
        custom_prompt,
        identifiers,
    })
}

async fn run_agent_loop(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    scope: ExtractionScope,
    system_prompt: String,
) -> Result<(usize, Option<String>)> {
    let tool_defs = tools::definitions();
    let mut input = vec![InputItem::Message {
        role: Role::User,
        content: "Process the messages in the window. Record any actions you find.".to_string(),
    }];
    let mut last_response_id: Option<String> = None;
    let mut actions_created = 0usize;

    for turn in 0..MAX_AGENT_TURNS {
        let response = llm
            .send(
                &system_prompt,
                std::mem::take(&mut input),
                &tool_defs,
                last_response_id.as_deref(),
            )
            .await
            .with_context(|| format!("LLM send failed on turn {turn}"))?;
        last_response_id = Some(response.id);

        let mut function_calls = Vec::new();
        let mut text_parts = Vec::new();
        for item in response.output {
            match item {
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => function_calls.push((call_id, name, arguments)),
                OutputItem::Message { content } => {
                    for c in content {
                        if let crate::llm::ContentItem::OutputText { text } = c {
                            text_parts.push(text);
                        }
                    }
                }
                OutputItem::Reasoning { .. } | OutputItem::Unknown => {}
            }
        }

        if function_calls.is_empty() {
            let last_text = text_parts.join("");
            let summary = if last_text.is_empty() {
                None
            } else {
                Some(last_text)
            };
            return Ok((actions_created, summary));
        }

        for (call_id, name, arguments) in function_calls {
            debug!(name = %name, call_id = %call_id, "dispatching tool");
            let out = tools::dispatch(pool, scope, &name, &arguments).await;
            if out.recorded_action {
                actions_created += 1;
            }
            input.push(InputItem::FunctionCallOutput {
                call_id,
                output: out.output,
            });
        }
    }

    anyhow::bail!("extraction agent exceeded {MAX_AGENT_TURNS} turns")
}

#[allow(clippy::too_many_arguments)]
async fn record_run(
    pool: &SqlitePool,
    channel_id: i64,
    up_to_message_id: Option<i64>,
    model: &str,
    result: &str,
    _actions_created: usize,
    messages_pending_embed: i64,
    summary: Option<String>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO extraction_runs \
         (channel_id, ran_at, up_to_message_id, model, prompt_version, result, \
          embeddings_partial, messages_pending_embed, summary) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(channel_id)
    .bind(Utc::now().timestamp())
    .bind(up_to_message_id)
    .bind(model)
    .bind(PROMPT_VERSION)
    .bind(result)
    .bind(if messages_pending_embed > 0 { 1 } else { 0 })
    .bind(messages_pending_embed)
    .bind(summary)
    .execute(pool)
    .await
    .context("inserting extraction_runs")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::ingest::ingest_batch;
    use crate::source::{Cursor, ImportedAuthor, ImportedMessage, PollBatch, SourceId};
    use chrono::Utc;
    use tempfile::TempDir;

    async fn setup() -> Result<(TempDir, SqlitePool, i64, i64)> {
        let tmp = TempDir::new()?;
        let path = tmp.path().join("test.db");
        let pool = db::open(&path).await?;
        db::migrate(&pool).await?;

        let (source_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'test', 'kc/test', ?) RETURNING id",
        )
        .bind(Utc::now().timestamp())
        .fetch_one(&pool)
        .await?;
        let (channel_id,): (i64,) = sqlx::query_as(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, 'INBOX', 'INBOX', 'mailbox') RETURNING id",
        )
        .bind(source_id)
        .fetch_one(&pool)
        .await?;

        let msg = ImportedMessage {
            external_id: "msg-1".to_string(),
            parent_external_id: None,
            author: Some(ImportedAuthor {
                external_id: "ana@example.com".to_string(),
                display_name: Some("Ana".to_string()),
                handle: None,
            }),
            posted_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            subject: Some("Hello".to_string()),
            body: "Please take a look".to_string(),
            body_format: "text".to_string(),
            raw_json: None,
            flags: 0,
        };
        ingest_batch(
            &pool,
            SourceId(source_id),
            channel_id,
            &PollBatch {
                messages: vec![msg],
                next_cursor: Cursor("1:2".to_string()),
                more_available: false,
            },
        )
        .await?;

        Ok((tmp, pool, source_id, channel_id))
    }

    #[tokio::test]
    async fn record_action_persists_action_evidence_and_event() -> Result<()> {
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        let args = serde_json::json!({
            "title": "Take a look at the doc",
            "details": "Ana asked you to review.",
            "confidence": "high",
            "rationale": "Direct ask from Ana",
            "evidence_external_ids": ["msg-1"]
        })
        .to_string();

        let scope = ExtractionScope {
            source_id,
            channel_id,
        };
        let out = tools::dispatch(&pool, scope, "record_action", &args).await;
        assert!(out.recorded_action);

        let actions: (i64, String, String, String) =
            sqlx::query_as("SELECT id, title, confidence, status FROM actions LIMIT 1")
                .fetch_one(&pool)
                .await?;
        assert_eq!(actions.1, "Take a look at the doc");
        assert_eq!(actions.2, "high");
        assert_eq!(actions.3, "auto_claimed");

        let (evidence_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM action_evidence WHERE action_id = ?")
                .bind(actions.0)
                .fetch_one(&pool)
                .await?;
        assert_eq!(evidence_count, 1);

        let (event_kind, actor): (String, String) =
            sqlx::query_as("SELECT event_kind, actor FROM action_events WHERE action_id = ?")
                .bind(actions.0)
                .fetch_one(&pool)
                .await?;
        assert_eq!(event_kind, "created");
        assert_eq!(actor, "agent_auto");

        let (queued,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embed_queue WHERE target_kind = 'action'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(queued, 1);

        Ok(())
    }

    #[tokio::test]
    async fn record_action_rolls_back_when_no_evidence_resolves() -> Result<()> {
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        let args = serde_json::json!({
            "title": "Imaginary",
            "details": "Refers to a message that doesn't exist.",
            "confidence": "medium",
            "rationale": "Test",
            "evidence_external_ids": ["does-not-exist"]
        })
        .to_string();

        let scope = ExtractionScope {
            source_id,
            channel_id,
        };
        let out = tools::dispatch(&pool, scope, "record_action", &args).await;
        assert!(!out.recorded_action);
        assert!(out.output.contains("error"));

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM actions")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);

        Ok(())
    }

    #[tokio::test]
    async fn medium_confidence_stays_pending() -> Result<()> {
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        let args = serde_json::json!({
            "title": "Maybe do this",
            "details": "Not sure if for you.",
            "confidence": "medium",
            "rationale": "Implied",
            "evidence_external_ids": ["msg-1"]
        })
        .to_string();

        let scope = ExtractionScope {
            source_id,
            channel_id,
        };
        tools::dispatch(&pool, scope, "record_action", &args).await;

        let (status, actor): (String, String) = sqlx::query_as(
            "SELECT a.status, ae.actor FROM actions a \
             JOIN action_events ae ON ae.action_id = a.id LIMIT 1",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(status, "pending");
        assert_eq!(actor, "agent_queued");

        Ok(())
    }
}
