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

/// The id of the most recent non-archived chat seeded from a specific entity
/// (e.g. an action), or `None`. Lets the inline action chat resume an existing
/// conversation instead of starting a fresh one each time the action is opened.
pub async fn find_seeded_chat(
    pool: &SqlitePool,
    seeded_from_kind: &str,
    seeded_from_id: i64,
) -> Result<Option<i64>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM chats \
         WHERE seeded_from_kind = ? AND seeded_from_id = ? AND archived = 0 \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(seeded_from_kind)
    .bind(seeded_from_id)
    .fetch_optional(pool)
    .await
    .context("finding seeded chat")?;
    Ok(row.map(|(id,)| id))
}

/// List chats most-recently-active first. Archived chats are excluded unless
/// `include_archived` is set, in which case they sort after the active ones.
pub async fn list_chats(pool: &SqlitePool, include_archived: bool) -> Result<Vec<ChatDto>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        Option<String>,
        Option<String>,
        Option<i64>,
        i64,
        i64,
        i64,
    )> = sqlx::query_as(
        "SELECT id, title, seeded_from_kind, seeded_from_id, created_at, updated_at, archived \
             FROM chats WHERE (? OR archived = 0) ORDER BY archived ASC, updated_at DESC",
    )
    .bind(include_archived)
    .fetch_all(pool)
    .await
    .context("listing chats")?;
    Ok(rows
        .into_iter()
        .map(
            |(id, title, seeded_from_kind, seeded_from_id, created_at, updated_at, archived)| {
                ChatDto {
                    id,
                    title,
                    seeded_from_kind,
                    seeded_from_id,
                    created_at,
                    updated_at,
                    archived: archived != 0,
                }
            },
        )
        .collect())
}

/// Archive or unarchive a chat (toggles whether it shows in the default list).
pub async fn set_archived(pool: &SqlitePool, chat_id: i64, archived: bool) -> Result<()> {
    sqlx::query("UPDATE chats SET archived = ? WHERE id = ?")
        .bind(archived as i64)
        .bind(chat_id)
        .execute(pool)
        .await
        .context("archiving chat")?;
    Ok(())
}

/// Permanently delete a chat. Its turns and reasoning rows cascade away via the
/// `ON DELETE CASCADE` foreign keys (foreign_keys is enabled on the pool).
pub async fn delete_chat(pool: &SqlitePool, chat_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM chats WHERE id = ?")
        .bind(chat_id)
        .execute(pool)
        .await
        .context("deleting chat")?;
    Ok(())
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

/// A persisted chat turn with its id. Carries enough to (a) reconstruct the
/// model `InputItem` it replays as and (b) let the compaction planner pick a
/// safe boundary by turn id without re-querying.
#[derive(Debug, Clone)]
pub struct TurnRow {
    pub id: i64,
    pub role: String,
    pub content: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
}

/// The active compaction checkpoint for a chat: the summary that stands in for
/// every turn with `id <= up_to_turn_id`. `None` when the chat has never been
/// compacted.
#[derive(Debug, Clone)]
pub struct SummaryCheckpoint {
    pub up_to_turn_id: i64,
    pub summary: String,
}

/// Map one persisted turn to the model `InputItem` it replays as, or `None` for
/// a row that carries neither text nor a tool reference (e.g. a reasoning-only
/// placeholder). Never consults the reasoning table — that's what keeps
/// reasoning out of the prompt. Shared by [`build_history`] and the compaction
/// planner so they agree exactly on what the model sees.
pub(crate) fn row_to_input(row: &TurnRow) -> Option<InputItem> {
    match (row.role.as_str(), &row.tool_name, &row.tool_call_id) {
        // Assistant tool call: arguments live in `content`.
        ("assistant", Some(name), Some(call_id)) => Some(InputItem::FunctionCall {
            call_id: call_id.clone(),
            name: name.clone(),
            arguments: row.content.clone().unwrap_or_default(),
        }),
        // Tool result.
        ("tool", _, Some(call_id)) => Some(InputItem::FunctionCallOutput {
            call_id: call_id.clone(),
            output: row.content.clone().unwrap_or_default(),
        }),
        // Plain user/assistant text.
        ("user", _, _) => row.content.clone().map(|content| InputItem::Message {
            role: Role::User,
            content,
        }),
        ("assistant", _, _) => row.content.clone().map(|content| InputItem::Message {
            role: Role::Assistant,
            content,
        }),
        _ => None,
    }
}

/// Load the chat's turns with `id > after_turn_id`, in replay order. `after = 0`
/// loads them all. Filtering by id (not `created_at`) is what lets a compaction
/// checkpoint cleanly exclude everything it folded in. Used by both history
/// reconstruction and the compaction planner.
pub async fn load_turn_rows(
    pool: &SqlitePool,
    chat_id: i64,
    after_turn_id: i64,
) -> Result<Vec<TurnRow>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, role, content, tool_name, tool_call_id \
         FROM chat_turns WHERE chat_id = ? AND id > ? ORDER BY created_at, id",
    )
    .bind(chat_id)
    .bind(after_turn_id)
    .fetch_all(pool)
    .await
    .context("loading chat turn rows")?;
    Ok(rows
        .into_iter()
        .map(|(id, role, content, tool_name, tool_call_id)| TurnRow {
            id,
            role,
            content,
            tool_name,
            tool_call_id,
        })
        .collect())
}

/// The active compaction checkpoint (the row with the greatest `up_to_turn_id`),
/// or `None` if the chat was never compacted.
pub async fn latest_summary(pool: &SqlitePool, chat_id: i64) -> Result<Option<SummaryCheckpoint>> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT up_to_turn_id, summary FROM chat_summaries \
         WHERE chat_id = ? ORDER BY up_to_turn_id DESC LIMIT 1",
    )
    .bind(chat_id)
    .fetch_optional(pool)
    .await
    .context("loading latest chat summary")?;
    Ok(row.map(|(up_to_turn_id, summary)| SummaryCheckpoint {
        up_to_turn_id,
        summary,
    }))
}

/// Record a new compaction checkpoint. Append-only — the greatest
/// `up_to_turn_id` wins, so re-compaction just inserts a fresh row.
pub async fn insert_summary(
    pool: &SqlitePool,
    chat_id: i64,
    up_to_turn_id: i64,
    summary: &str,
) -> Result<()> {
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO chat_summaries (chat_id, up_to_turn_id, summary, created_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(chat_id)
    .bind(up_to_turn_id)
    .bind(summary)
    .bind(now)
    .execute(pool)
    .await
    .context("inserting chat summary")?;
    Ok(())
}

/// Keyword search over a single chat's turns — what the model uses to find
/// earlier content the summary dropped. Case-insensitive `LIKE`: a chat
/// transcript is small (one conversation), so there's no FTS index for it the
/// way there is for ingested messages. Returns `(turn_id, role, created_at,
/// snippet)`, most-recent first.
pub async fn search_turns(
    pool: &SqlitePool,
    chat_id: i64,
    query: &str,
    limit: i64,
) -> Result<Vec<(i64, String, i64, String)>> {
    // Escape LIKE metacharacters so a literal query can't act as a wildcard.
    let like = format!(
        "%{}%",
        query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let rows: Vec<(i64, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT id, role, content, created_at FROM chat_turns \
         WHERE chat_id = ? AND content LIKE ? ESCAPE '\\' \
         ORDER BY created_at DESC, id DESC LIMIT ?",
    )
    .bind(chat_id)
    .bind(&like)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("searching chat turns")?;
    Ok(rows
        .into_iter()
        .map(|(id, role, content, created_at)| {
            (
                id,
                role,
                created_at,
                crate::extract::tools::snippet(content.as_deref().unwrap_or(""), 200),
            )
        })
        .collect())
}

/// Load full turns by id within a chat (the model recalls these after
/// [`search_turns`]). Scoped to `chat_id` so a chat can never read another's
/// turns.
pub async fn load_turn_contents(
    pool: &SqlitePool,
    chat_id: i64,
    ids: &[i64],
) -> Result<Vec<TurnRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // sqlite has no array binding — build an IN list of placeholders. Only the
    // count of `?`s is interpolated (never any data), so this is injection-safe.
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT id, role, content, tool_name, tool_call_id FROM chat_turns \
         WHERE chat_id = ? AND id IN ({placeholders}) ORDER BY created_at, id"
    );
    let mut q = sqlx::query_as::<_, (i64, String, Option<String>, Option<String>, Option<String>)>(
        sqlx::AssertSqlSafe(sql),
    )
    .bind(chat_id);
    for id in ids {
        q = q.bind(id);
    }
    let rows = q
        .fetch_all(pool)
        .await
        .context("loading chat turn contents")?;
    Ok(rows
        .into_iter()
        .map(|(id, role, content, tool_name, tool_call_id)| TurnRow {
            id,
            role,
            content,
            tool_name,
            tool_call_id,
        })
        .collect())
}

/// The leading message that stands in for a compacted prefix. Framed as recap
/// context so the model treats it as background and knows it can pull exact
/// earlier wording back with the recall tools.
pub(crate) fn summary_input_item(summary: &str) -> InputItem {
    InputItem::Message {
        role: Role::User,
        content: format!(
            "[Summary of the earlier part of this conversation, condensed to fit the context \
             window. If you need exact earlier wording, use search_conversation then \
             recall_turns.]\n\n{summary}"
        ),
    }
}

/// Rebuild the model's input from the persisted turns. Joins **only**
/// `chat_turns` (never the reasoning table), mapping each row to the right
/// `InputItem` via [`row_to_input`]:
/// - user/assistant text → `Message`
/// - assistant tool-call rows (`tool_name` set) → `FunctionCall`
/// - tool rows → `FunctionCallOutput`
///
/// When the chat has a compaction checkpoint, the folded prefix
/// (`id <= up_to_turn_id`) is replaced by a single leading summary message and
/// only the verbatim tail is replayed. `chat_turns` itself is untouched, so the
/// UI's [`load_turns`] still shows the whole conversation.
pub async fn build_history(pool: &SqlitePool, chat_id: i64) -> Result<Vec<InputItem>> {
    let checkpoint = latest_summary(pool, chat_id).await?;
    let after = checkpoint.as_ref().map(|c| c.up_to_turn_id).unwrap_or(0);
    let rows = load_turn_rows(pool, chat_id, after).await?;

    let mut history = Vec::with_capacity(rows.len() + 1);
    if let Some(c) = &checkpoint {
        history.push(summary_input_item(&c.summary));
    }
    history.extend(rows.iter().filter_map(row_to_input));
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
    async fn find_seeded_chat_resumes_matching_action_only() {
        let (_tmp, pool) = empty_db().await;
        // Nothing seeded yet.
        assert_eq!(find_seeded_chat(&pool, "action", 7).await.unwrap(), None);

        let a7 = create_chat(&pool, Some("action"), Some(7)).await.unwrap();
        let _a8 = create_chat(&pool, Some("action"), Some(8)).await.unwrap();
        let _m7 = create_chat(&pool, Some("message"), Some(7)).await.unwrap();

        // Matches exactly (kind, id) — not a different id, nor a different kind.
        assert_eq!(
            find_seeded_chat(&pool, "action", 7).await.unwrap(),
            Some(a7)
        );
        assert_eq!(find_seeded_chat(&pool, "action", 99).await.unwrap(), None);

        // Archived chats are skipped so a resume never lands on a hidden one.
        set_archived(&pool, a7, true).await.unwrap();
        assert_eq!(find_seeded_chat(&pool, "action", 7).await.unwrap(), None);
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
        let chats = list_chats(&pool, false).await.unwrap();
        assert_eq!(chats[0].title.as_deref(), Some("why flagged?"));
        assert_eq!(chats[0].seeded_from_kind.as_deref(), Some("action"));
        assert!(!chats[0].archived);

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

    #[tokio::test]
    async fn build_history_substitutes_the_summary_for_the_folded_prefix() {
        let (_tmp, pool) = empty_db().await;
        let chat = create_chat(&pool, None, None).await.unwrap();
        append_turn(
            &pool,
            chat,
            "user",
            Some("first question"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let t2 = append_turn(
            &pool,
            chat,
            "assistant",
            Some("first answer"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        append_turn(
            &pool,
            chat,
            "user",
            Some("second question"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // Fold everything up to and including the first answer into a summary.
        insert_summary(&pool, chat, t2, "CONDENSED RECAP")
            .await
            .unwrap();

        // The model now sees the recap message + only the post-watermark turn.
        let history = build_history(&pool, chat).await.unwrap();
        assert_eq!(history.len(), 2);
        assert!(
            matches!(&history[0], InputItem::Message { role: Role::User, content } if content.contains("CONDENSED RECAP")),
            "first item should be the summary message"
        );
        assert!(
            matches!(&history[1], InputItem::Message { role: Role::User, content } if content == "second question"),
            "only the verbatim tail should follow the summary"
        );

        // The UI transcript is untouched — it still shows the whole conversation.
        assert_eq!(load_turns(&pool, chat).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn search_and_recall_reach_a_chat_s_own_turns() {
        let (_tmp, pool) = empty_db().await;
        let chat = create_chat(&pool, None, None).await.unwrap();
        append_turn(
            &pool,
            chat,
            "user",
            Some("Let's discuss the quarterly budget plan"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let target = append_turn(
            &pool,
            chat,
            "assistant",
            Some("The budget is approved for Q3"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        append_turn(
            &pool,
            chat,
            "user",
            Some("unrelated chatter"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let hits = search_turns(&pool, chat, "budget", 10).await.unwrap();
        assert!(hits.iter().any(|(id, ..)| *id == target));
        assert!(hits.len() >= 2, "both budget turns should match: {hits:?}");

        let rows = load_turn_contents(&pool, chat, &[target]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0]
                .content
                .as_deref()
                .unwrap()
                .contains("approved for Q3")
        );
    }

    #[tokio::test]
    async fn archive_hides_from_default_list_and_delete_cascades() {
        let (_tmp, pool) = empty_db().await;

        let keep = create_chat(&pool, None, None).await.unwrap();
        let gone = create_chat(&pool, None, None).await.unwrap();
        // Give `gone` a turn + reasoning so we can prove the cascade deletes them.
        let turn = append_turn(&pool, gone, "user", Some("hi"), None, None, None)
            .await
            .unwrap();
        append_reasoning(&pool, turn, "thinking").await.unwrap();

        // Archive `keep`: it drops out of the default list but shows when asked.
        set_archived(&pool, keep, true).await.unwrap();
        let default = list_chats(&pool, false).await.unwrap();
        assert!(
            default.iter().all(|c| c.id != keep),
            "archived chat must not appear in the default list"
        );
        let all = list_chats(&pool, true).await.unwrap();
        let archived = all.iter().find(|c| c.id == keep).unwrap();
        assert!(
            archived.archived,
            "include_archived should surface it as archived"
        );

        // Unarchive brings it back to the default list.
        set_archived(&pool, keep, false).await.unwrap();
        assert!(
            list_chats(&pool, false)
                .await
                .unwrap()
                .iter()
                .any(|c| c.id == keep)
        );

        // Delete `gone`: the chat and its turns + reasoning all disappear.
        delete_chat(&pool, gone).await.unwrap();
        assert!(
            list_chats(&pool, true)
                .await
                .unwrap()
                .iter()
                .all(|c| c.id != gone)
        );
        let (turns,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM chat_turns WHERE chat_id = ?")
            .bind(gone)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(turns, 0, "turns should cascade-delete with the chat");
        let (reasoning,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM chat_turn_reasoning WHERE turn_id = ?")
                .bind(turn)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            reasoning, 0,
            "reasoning should cascade-delete with its turn"
        );
    }
}
