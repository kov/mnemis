use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::source::{ImportedAuthor, ImportedMessage, PollBatch, SourceId};

/// Persist a poll batch atomically: insert new messages, upsert authors,
/// enqueue embed tasks, advance the channel cursor.
///
/// Returns the count of newly-inserted messages.
pub async fn ingest_batch(
    pool: &SqlitePool,
    source_id: SourceId,
    channel_id: i64,
    batch: &PollBatch,
) -> Result<usize> {
    let mut tx = pool.begin().await.context("starting ingest transaction")?;
    let now = Utc::now().timestamp();
    let mut inserted = 0usize;

    for msg in &batch.messages {
        let author_id = match &msg.author {
            Some(author) => Some(upsert_person(&mut tx, source_id, author).await?),
            None => None,
        };

        let message_id = insert_message_if_new(&mut tx, channel_id, msg, author_id, now).await?;
        if let Some(message_id) = message_id {
            inserted += 1;
            enqueue_embed(&mut tx, "message", message_id, &message_text(msg), now).await?;
        }
    }

    sqlx::query("UPDATE channels SET cursor = ?, last_synced_at = ? WHERE id = ?")
        .bind(&batch.next_cursor.0)
        .bind(now)
        .bind(channel_id)
        .execute(&mut *tx)
        .await
        .context("updating channel cursor")?;

    tx.commit().await.context("committing ingest transaction")?;
    Ok(inserted)
}

async fn upsert_person(
    tx: &mut sqlx::SqliteTransaction<'_>,
    source_id: SourceId,
    author: &ImportedAuthor,
) -> Result<i64> {
    let existing: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM people WHERE source_id = ? AND external_id = ?")
            .bind(source_id.0)
            .bind(&author.external_id)
            .fetch_optional(&mut **tx)
            .await
            .context("looking up person")?;

    if let Some((id,)) = existing {
        // Update display_name/handle in case they changed.
        sqlx::query("UPDATE people SET display_name = ?, handle = ? WHERE id = ?")
            .bind(author.display_name.as_deref())
            .bind(author.handle.as_deref())
            .bind(id)
            .execute(&mut **tx)
            .await
            .context("updating person")?;
        return Ok(id);
    }

    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO people (source_id, external_id, display_name, handle) \
         VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(source_id.0)
    .bind(&author.external_id)
    .bind(author.display_name.as_deref())
    .bind(author.handle.as_deref())
    .fetch_one(&mut **tx)
    .await
    .context("inserting person")?;
    Ok(id)
}

async fn insert_message_if_new(
    tx: &mut sqlx::SqliteTransaction<'_>,
    channel_id: i64,
    msg: &ImportedMessage,
    author_id: Option<i64>,
    ingested_at: i64,
) -> Result<Option<i64>> {
    let existing: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM messages WHERE channel_id = ? AND external_id = ?")
            .bind(channel_id)
            .bind(&msg.external_id)
            .fetch_optional(&mut **tx)
            .await
            .context("checking for existing message")?;

    if existing.is_some() {
        return Ok(None);
    }

    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO messages \
         (channel_id, external_id, parent_external_id, author_id, posted_at, \
          subject, body, body_format, raw_json, flags, ingested_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(channel_id)
    .bind(&msg.external_id)
    .bind(msg.parent_external_id.as_deref())
    .bind(author_id)
    .bind(msg.posted_at.timestamp())
    .bind(msg.subject.as_deref())
    .bind(&msg.body)
    .bind(&msg.body_format)
    .bind(msg.raw_json.as_deref())
    .bind(msg.flags as i64)
    .bind(ingested_at)
    .fetch_one(&mut **tx)
    .await
    .context("inserting message")?;
    Ok(Some(id))
}

async fn enqueue_embed(
    tx: &mut sqlx::SqliteTransaction<'_>,
    target_kind: &str,
    target_id: i64,
    text: &str,
    enqueued_at: i64,
) -> Result<()> {
    let hash = content_hash(text);
    sqlx::query(
        "INSERT INTO embed_queue (target_kind, target_id, text_hash, enqueued_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(target_kind, target_id) DO UPDATE SET \
             text_hash = excluded.text_hash, \
             enqueued_at = excluded.enqueued_at, \
             attempts = 0, \
             last_error = NULL \
         WHERE embed_queue.text_hash != excluded.text_hash",
    )
    .bind(target_kind)
    .bind(target_id)
    .bind(&hash)
    .bind(enqueued_at)
    .execute(&mut **tx)
    .await
    .context("enqueueing embed task")?;
    Ok(())
}

fn message_text(msg: &ImportedMessage) -> String {
    match &msg.subject {
        Some(s) if !s.is_empty() => format!("{s}\n\n{}", msg.body),
        _ => msg.body.clone(),
    }
}

fn content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::source::{Cursor, ImportedMessage};
    use chrono::{DateTime, Utc};
    use tempfile::TempDir;

    async fn setup() -> Result<(TempDir, SqlitePool, SourceId, i64)> {
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

        Ok((tmp, pool, SourceId(source_id), channel_id))
    }

    fn sample_message(id: &str, subject: &str, body: &str) -> ImportedMessage {
        ImportedMessage {
            external_id: id.to_string(),
            parent_external_id: None,
            author: Some(ImportedAuthor {
                external_id: "ana@example.com".to_string(),
                display_name: Some("Ana".to_string()),
                handle: None,
            }),
            posted_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            subject: Some(subject.to_string()),
            body: body.to_string(),
            body_format: "text".to_string(),
            raw_json: None,
            flags: 0,
        }
    }

    #[tokio::test]
    async fn ingests_new_messages_and_enqueues_embeds() -> Result<()> {
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        let batch = PollBatch {
            messages: vec![
                sample_message("msg-1", "Hello", "body one"),
                sample_message("msg-2", "Howdy", "body two"),
            ],
            next_cursor: Cursor("1:100".to_string()),
            more_available: false,
        };

        let inserted = ingest_batch(&pool, source_id, channel_id, &batch).await?;
        assert_eq!(inserted, 2);

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count.0, 2);

        let queue: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM embed_queue")
            .fetch_one(&pool)
            .await?;
        assert_eq!(queue.0, 2);

        let (cursor,): (String,) = sqlx::query_as("SELECT cursor FROM channels WHERE id = ?")
            .bind(channel_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(cursor, "1:100");

        // FTS triggers must have fired.
        let fts: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM messages_fts WHERE body MATCH 'body'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(fts.0, 2);

        Ok(())
    }

    #[tokio::test]
    async fn dedups_messages_by_external_id() -> Result<()> {
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        let batch = PollBatch {
            messages: vec![sample_message("msg-1", "Hello", "body")],
            next_cursor: Cursor("1:100".to_string()),
            more_available: false,
        };
        ingest_batch(&pool, source_id, channel_id, &batch).await?;

        // Same message id, ingested again — should not double-insert.
        let inserted = ingest_batch(&pool, source_id, channel_id, &batch).await?;
        assert_eq!(inserted, 0);

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count.0, 1);

        Ok(())
    }

    #[tokio::test]
    async fn upserts_authors() -> Result<()> {
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        let batch = PollBatch {
            messages: vec![
                sample_message("msg-1", "S1", "b1"),
                sample_message("msg-2", "S2", "b2"),
            ],
            next_cursor: Cursor("1:100".to_string()),
            more_available: false,
        };
        ingest_batch(&pool, source_id, channel_id, &batch).await?;

        // One person row for both messages from the same author.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM people")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count.0, 1);

        Ok(())
    }
}
