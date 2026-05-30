//! Persistence for the chat view: `chats`, `chat_turns`, and (separately, so
//! it can't be replayed) `chat_turn_reasoning`. Runtime sqlx throughout, the
//! same style as `queries.rs`/`mutations.rs`.
//!
//! The agent loop persists every turn here *before* streaming it to the UI, so
//! the DB — not the channel — is the source of truth. `build_history` is the
//! other side of that: it reconstructs the model's input from the persisted
//! turns, deliberately joining only `chat_turns` so reasoning can never leak
//! back into a prompt.

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::SqlitePool;

use mnemis_types::{ChatDto, ChatTurnDto};

use crate::llm::{InputItem, Role};

/// Create a new chat, optionally seeded from an entity ("Talk about this").
/// Returns the new chat id. Title starts null and is filled from the first
/// user message (see [`ensure_title`]).
pub async fn create_chat(
    pool: &SqlitePool,
    seeded_from_kind: Option<&str>,
    seeded_from_id: Option<i64>,
) -> Result<i64> {
    let now = Utc::now().timestamp();
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO chats (title, seeded_from_kind, seeded_from_id, created_at, updated_at) \
         VALUES (NULL, ?, ?, ?, ?) RETURNING id",
    )
    .bind(seeded_from_kind)
    .bind(seeded_from_id)
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await
    .context("creating chat")?;
    Ok(id)
}

/// List non-archived chats, most-recently-active first.
pub async fn list_chats(pool: &SqlitePool) -> Result<Vec<ChatDto>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, Option<String>, Option<String>, Option<i64>, i64, i64)> = sqlx::query_as(
        "SELECT id, title, seeded_from_kind, seeded_from_id, created_at, updated_at \
         FROM chats WHERE archived = 0 ORDER BY updated_at DESC",
    )
    .fetch_all(pool)
    .await
    .context("listing chats")?;
    Ok(rows
        .into_iter()
        .map(
            |(id, title, seeded_from_kind, seeded_from_id, created_at, updated_at)| ChatDto {
                id,
                title,
                seeded_from_kind,
                seeded_from_id,
                created_at,
                updated_at,
            },
        )
        .collect())
}

/// One chat's transcript in display order, each turn carrying its reasoning
/// (left-joined) when present.
pub async fn load_turns(pool: &SqlitePool, chat_id: i64) -> Result<Vec<ChatTurnDto>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
    )> = sqlx::query_as(
        "SELECT t.id, t.role, t.content, t.tool_name, t.tool_call_id, r.content, t.created_at \
         FROM chat_turns t \
         LEFT JOIN chat_turn_reasoning r ON r.turn_id = t.id \
         WHERE t.chat_id = ? \
         ORDER BY t.created_at, t.id",
    )
    .bind(chat_id)
    .fetch_all(pool)
    .await
    .context("loading chat turns")?;
    Ok(rows
        .into_iter()
        .map(
            |(id, role, content, tool_name, tool_call_id, reasoning, created_at)| ChatTurnDto {
                id,
                role,
                content,
                tool_name,
                tool_call_id,
                reasoning,
                created_at,
            },
        )
        .collect())
}

/// Append one turn and return its id. `response_id` is the omlx response id
/// when known (informational; we thread client-side, not via it).
#[allow(clippy::too_many_arguments)]
pub async fn append_turn(
    pool: &SqlitePool,
    chat_id: i64,
    role: &str,
    content: Option<&str>,
    tool_name: Option<&str>,
    tool_call_id: Option<&str>,
    response_id: Option<&str>,
) -> Result<i64> {
    let now = Utc::now().timestamp();
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO chat_turns \
         (chat_id, role, content, tool_name, tool_call_id, response_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(chat_id)
    .bind(role)
    .bind(content)
    .bind(tool_name)
    .bind(tool_call_id)
    .bind(response_id)
    .bind(now)
    .fetch_one(pool)
    .await
    .context("appending chat turn")?;
    Ok(id)
}

/// Attach (or replace) the captured reasoning for a turn. Lives in its own
/// table so [`build_history`] — which reads only `chat_turns` — can never feed
/// it back to the model.
pub async fn append_reasoning(pool: &SqlitePool, turn_id: i64, content: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO chat_turn_reasoning (turn_id, content) VALUES (?, ?) \
         ON CONFLICT(turn_id) DO UPDATE SET content = excluded.content",
    )
    .bind(turn_id)
    .bind(content)
    .execute(pool)
    .await
    .context("appending chat reasoning")?;
    Ok(())
}

/// Bump a chat's `updated_at` so it sorts to the top of the list.
pub async fn touch_chat(pool: &SqlitePool, chat_id: i64) -> Result<()> {
    let now = Utc::now().timestamp();
    sqlx::query("UPDATE chats SET updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(chat_id)
        .execute(pool)
        .await
        .context("touching chat")?;
    Ok(())
}

/// Give the chat a title derived from its first user message — only if it
/// doesn't have one yet (idempotent via the `title IS NULL` guard).
pub async fn ensure_title(pool: &SqlitePool, chat_id: i64, from_text: &str) -> Result<()> {
    let title = title_from(from_text);
    sqlx::query("UPDATE chats SET title = ? WHERE id = ? AND title IS NULL")
        .bind(&title)
        .bind(chat_id)
        .execute(pool)
        .await
        .context("setting chat title")?;
    Ok(())
}

/// Set a chat's title unconditionally (used to upgrade the provisional
/// first-message title to a seed label or a model-generated one).
pub async fn set_title(pool: &SqlitePool, chat_id: i64, title: &str) -> Result<()> {
    sqlx::query("UPDATE chats SET title = ? WHERE id = ?")
        .bind(title)
        .bind(chat_id)
        .execute(pool)
        .await
        .context("updating chat title")?;
    Ok(())
}

/// The chat's current title, if any.
pub async fn current_title(pool: &SqlitePool, chat_id: i64) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT title FROM chats WHERE id = ?")
        .bind(chat_id)
        .fetch_optional(pool)
        .await
        .context("loading chat title")?;
    Ok(row.and_then(|(t,)| t))
}

/// True when the chat was seeded from an entity ("Talk about this"). Seeded
/// chats are titled from their seed label, so they skip generated titling.
pub async fn is_seeded(pool: &SqlitePool, chat_id: i64) -> Result<bool> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT seeded_from_kind FROM chats WHERE id = ?")
            .bind(chat_id)
            .fetch_optional(pool)
            .await
            .context("checking chat seed")?;
    Ok(matches!(row, Some((Some(_),))))
}

pub(crate) fn title_from(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 60;
    if one_line.chars().count() <= MAX {
        one_line
    } else {
        format!("{}…", one_line.chars().take(MAX).collect::<String>())
    }
}

/// Rebuild the model's input from the persisted turns. Joins **only**
/// `chat_turns` (never the reasoning table), mapping each row to the right
/// `InputItem`:
/// - user/assistant text → `Message`
/// - assistant tool-call rows (`tool_name` set) → `FunctionCall`
/// - tool rows → `FunctionCallOutput`
///
/// Rows that carry neither text nor a tool reference (e.g. a reasoning-only
/// placeholder) are skipped.
pub async fn build_history(pool: &SqlitePool, chat_id: i64) -> Result<Vec<InputItem>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT role, content, tool_name, tool_call_id \
         FROM chat_turns WHERE chat_id = ? ORDER BY created_at, id",
    )
    .bind(chat_id)
    .fetch_all(pool)
    .await
    .context("building chat history")?;

    let mut history = Vec::with_capacity(rows.len());
    for (role, content, tool_name, tool_call_id) in rows {
        match (role.as_str(), tool_name, tool_call_id) {
            // Assistant tool call: arguments live in `content`.
            ("assistant", Some(name), Some(call_id)) => {
                history.push(InputItem::FunctionCall {
                    call_id,
                    name,
                    arguments: content.unwrap_or_default(),
                });
            }
            // Tool result.
            ("tool", _, Some(call_id)) => {
                history.push(InputItem::FunctionCallOutput {
                    call_id,
                    output: content.unwrap_or_default(),
                });
            }
            // Plain user/assistant text.
            ("user", _, _) => {
                if let Some(text) = content {
                    history.push(InputItem::Message {
                        role: Role::User,
                        content: text,
                    });
                }
            }
            ("assistant", _, _) => {
                if let Some(text) = content {
                    history.push(InputItem::Message {
                        role: Role::Assistant,
                        content: text,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use tempfile::TempDir;

    async fn empty_db() -> (TempDir, SqlitePool) {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        (tmp, pool)
    }

    #[tokio::test]
    async fn turns_persist_and_history_excludes_reasoning() {
        let (_tmp, pool) = empty_db().await;
        let chat = create_chat(&pool, Some("action"), Some(42)).await.unwrap();

        // user → assistant tool-call (+reasoning) → tool result → assistant text
        append_turn(&pool, chat, "user", Some("why flagged?"), None, None, None)
            .await
            .unwrap();
        ensure_title(&pool, chat, "why flagged?").await.unwrap();
        let call_turn = append_turn(
            &pool,
            chat,
            "assistant",
            Some(r#"{"action_id":"A-42"}"#),
            Some("get_action"),
            Some("call_1"),
            None,
        )
        .await
        .unwrap();
        append_reasoning(&pool, call_turn, "let me look it up")
            .await
            .unwrap();
        append_turn(
            &pool,
            chat,
            "tool",
            Some(r#"{"status":"auto_claimed"}"#),
            Some("get_action"),
            Some("call_1"),
            None,
        )
        .await
        .unwrap();
        append_turn(
            &pool,
            chat,
            "assistant",
            Some("It was an explicit ask from Ana."),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // Transcript carries the reasoning on the tool-call turn.
        let turns = load_turns(&pool, chat).await.unwrap();
        assert_eq!(turns.len(), 4);
        assert_eq!(turns[0].role, "user");
        let call = turns.iter().find(|t| t.tool_name.is_some()).unwrap();
        assert_eq!(call.reasoning.as_deref(), Some("let me look it up"));

        // Title was derived from the first user message.
        let chats = list_chats(&pool).await.unwrap();
        assert_eq!(chats[0].title.as_deref(), Some("why flagged?"));
        assert_eq!(chats[0].seeded_from_kind.as_deref(), Some("action"));

        // Reconstructed history maps roles and OMITS reasoning entirely.
        let history = build_history(&pool, chat).await.unwrap();
        assert_eq!(history.len(), 4, "user + call + output + assistant text");
        assert!(
            matches!(&history[0], InputItem::Message { role: Role::User, content } if content == "why flagged?")
        );
        assert!(matches!(
            &history[1],
            InputItem::FunctionCall { name, call_id, .. } if name == "get_action" && call_id == "call_1"
        ));
        assert!(matches!(
            &history[2],
            InputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
        ));
        assert!(
            matches!(&history[3], InputItem::Message { role: Role::Assistant, content } if content.contains("Ana"))
        );
        // No history item should carry the reasoning string.
        let serialized = serde_json::to_string(
            &history
                .iter()
                .map(|i| serde_json::to_value(i).unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(
            !serialized.contains("let me look it up"),
            "reasoning must never appear in reconstructed history: {serialized}"
        );
    }
}
