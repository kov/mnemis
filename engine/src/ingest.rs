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
    let inserted = ingest_messages(&mut tx, source_id, channel_id, &batch.messages, false).await?;

    sqlx::query("UPDATE channels SET cursor = ?, last_synced_at = ? WHERE id = ?")
        .bind(&batch.next_cursor.0)
        .bind(Utc::now().timestamp())
        .bind(channel_id)
        .execute(&mut *tx)
        .await
        .context("updating channel cursor")?;

    tx.commit().await.context("committing ingest transaction")?;
    Ok(inserted)
}

/// Persist mail fetched only for reading (older history, muted folders):
/// shown in the mail views, never extracted for to-dos or embedded. A message
/// already stored just gains what it was missing (its placement, threading
/// headers, body). Returns the count of new rows.
pub async fn ingest_for_reading(
    pool: &SqlitePool,
    source_id: SourceId,
    channel_id: i64,
    messages: &[ImportedMessage],
) -> Result<usize> {
    let mut tx = pool.begin().await.context("starting ingest transaction")?;
    let inserted = ingest_messages(&mut tx, source_id, channel_id, messages, true).await?;
    tx.commit().await.context("committing ingest transaction")?;
    Ok(inserted)
}

/// Returns how many messages became extraction candidates (or, for
/// `reader_only`, how many rows are new).
async fn ingest_messages(
    tx: &mut sqlx::SqliteTransaction<'_>,
    source_id: SourceId,
    channel_id: i64,
    messages: &[ImportedMessage],
    reader_only: bool,
) -> Result<usize> {
    let now = Utc::now().timestamp();
    let mut counted = 0usize;
    for msg in messages {
        let author_id = match &msg.author {
            Some(author) => Some(upsert_person(tx, source_id, author).await?),
            None => None,
        };
        let stored = store_message(tx, channel_id, msg, author_id, now, reader_only).await?;
        if let Some(mail) = &msg.mail
            && stored.needs_thread
        {
            let mut linked = mail.references.clone();
            linked.extend(mail.in_reply_to.clone());
            let thread = crate::mail::thread::assign(
                tx,
                source_id.0,
                &msg.external_id,
                &linked,
                msg.subject.as_deref(),
                msg.posted_at.timestamp(),
            )
            .await?;
            sqlx::query("UPDATE messages SET thread_id = ? WHERE id = ?")
                .bind(thread)
                .bind(stored.id)
                .execute(&mut **tx)
                .await
                .context("setting thread")?;
        }
        if let Some(raw) = &msg.raw {
            crate::mail::cache::store(tx, source_id.0, &msg.external_id, raw, now).await?;
        }
        if stored.candidate {
            counted += 1;
            enqueue_embed(tx, "message", stored.id, &message_text(msg), now).await?;
        } else if reader_only && stored.new {
            counted += 1;
        }
    }
    Ok(counted)
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

struct Stored {
    id: i64,
    /// A row inserted by this call.
    new: bool,
    /// Newly eligible for extraction: inserted, or a reader-only row the
    /// extraction poll has now brought in.
    candidate: bool,
    /// Mail without a thread yet.
    needs_thread: bool,
}

/// Insert a message, or complete the row already stored for it in this
/// channel: its placement on the server, threading headers, and the body
/// when only the headers were known.
async fn store_message(
    tx: &mut sqlx::SqliteTransaction<'_>,
    channel_id: i64,
    msg: &ImportedMessage,
    author_id: Option<i64>,
    ingested_at: i64,
    reader_only: bool,
) -> Result<Stored> {
    let mail = msg.mail.clone().unwrap_or_default();
    let has_body = !mail.headers_only;
    let references_json = (!mail.references.is_empty())
        .then(|| serde_json::to_string(&mail.references))
        .transpose()?;

    // NULL when there are no recipients (chat sources, or mail without to/cc)
    // so empty rows stay clean and the projection can skip the section.
    let recipients_json = if msg.recipients.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&msg.recipients).context("serializing recipients")?)
    };

    let links_json = msg
        .links
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("serializing links")?;

    let existing: Option<(i64, bool, bool, Option<i64>)> = sqlx::query_as(
        "SELECT id, reader_only, has_body, thread_id FROM messages \
         WHERE channel_id = ? AND external_id = ?",
    )
    .bind(channel_id)
    .bind(&msg.external_id)
    .fetch_optional(&mut **tx)
    .await
    .context("checking for existing message")?;

    if let Some((id, was_reader_only, had_body, thread_id)) = existing {
        let fill_body = has_body && !had_body;
        sqlx::query(
            "UPDATE messages SET \
                 uid = COALESCE(?, uid), \
                 uidvalidity = COALESCE(?, uidvalidity), \
                 removed_at = NULL, \
                 in_reply_to = COALESCE(in_reply_to, ?), \
                 references_json = COALESCE(references_json, ?), \
                 size = COALESCE(?, size), \
                 flags = CASE WHEN ? THEN ? ELSE flags END, \
                 body = CASE WHEN ? THEN ? ELSE body END, \
                 links_json = CASE WHEN ? THEN ? ELSE links_json END, \
                 has_attachments = CASE WHEN ? THEN ? ELSE has_attachments END, \
                 has_body = MAX(has_body, ?), \
                 reader_only = MIN(reader_only, ?) \
             WHERE id = ?",
        )
        .bind(mail.uid.map(i64::from))
        .bind(mail.uidvalidity.map(i64::from))
        .bind(mail.in_reply_to.as_deref())
        .bind(references_json.as_deref())
        .bind(mail.size.map(i64::from))
        .bind(msg.mail.is_some())
        .bind(msg.flags as i64)
        .bind(fill_body)
        .bind(&msg.body)
        .bind(fill_body)
        .bind(links_json.as_deref())
        .bind(fill_body)
        .bind(mail.has_attachments)
        .bind(has_body)
        .bind(reader_only)
        .bind(id)
        .execute(&mut **tx)
        .await
        .context("completing stored message")?;
        return Ok(Stored {
            id,
            new: false,
            candidate: was_reader_only && !reader_only,
            needs_thread: thread_id.is_none(),
        });
    }

    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO messages \
         (channel_id, external_id, parent_external_id, author_id, posted_at, \
          subject, body, body_format, recipients_json, links_json, raw_json, flags, \
          ingested_at, uid, uidvalidity, in_reply_to, references_json, size, \
          has_attachments, has_body, reader_only) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(channel_id)
    .bind(&msg.external_id)
    .bind(msg.parent_external_id.as_deref())
    .bind(author_id)
    .bind(msg.posted_at.timestamp())
    .bind(msg.subject.as_deref())
    .bind(&msg.body)
    .bind(&msg.body_format)
    .bind(recipients_json.as_deref())
    .bind(links_json.as_deref())
    .bind(msg.raw_json.as_deref())
    .bind(msg.flags as i64)
    .bind(ingested_at)
    .bind(mail.uid.map(i64::from))
    .bind(mail.uidvalidity.map(i64::from))
    .bind(mail.in_reply_to.as_deref())
    .bind(references_json.as_deref())
    .bind(mail.size.map(i64::from))
    .bind(mail.has_attachments)
    .bind(has_body)
    .bind(reader_only)
    .fetch_one(&mut **tx)
    .await
    .context("inserting message")?;
    Ok(Stored {
        id,
        new: true,
        candidate: !reader_only,
        needs_thread: true,
    })
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
            recipients: Vec::new(),
            links: None,
            raw_json: None,
            flags: 0,
            mail: None,
            raw: None,
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

    /// A mail message as the IMAP source imports it: whole, or just headers.
    fn mail(
        id: &str,
        subject: &str,
        linked: &[&str],
        uid: u32,
        headers_only: bool,
    ) -> ImportedMessage {
        let mut m = sample_message(id, subject, if headers_only { "" } else { "the body" });
        m.mail = Some(crate::source::MailMeta {
            uid: Some(uid),
            uidvalidity: Some(7),
            in_reply_to: linked.last().map(|s| s.to_string()),
            references: linked.iter().map(|s| s.to_string()).collect(),
            size: None,
            has_attachments: false,
            headers_only,
        });
        m.raw = (!headers_only).then(|| format!("Message-ID: <{id}>\r\n\r\nthe body").into_bytes());
        m
    }

    async fn channel(pool: &SqlitePool, source_id: SourceId, name: &str) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, ?, ?, 'mailbox') RETURNING id",
        )
        .bind(source_id.0)
        .bind(name)
        .bind(name)
        .fetch_one(pool)
        .await?)
    }

    async fn thread_of(pool: &SqlitePool, external_id: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT thread_id FROM messages WHERE external_id = ? ORDER BY id LIMIT 1",
        )
        .bind(external_id)
        .fetch_one(pool)
        .await?)
    }

    #[tokio::test]
    async fn threads_mail_across_folders_whatever_order_it_arrives_in() -> Result<()> {
        let (_tmp, pool, source_id, inbox) = setup().await?;
        let sent = channel(&pool, source_id, "Sent").await?;

        // The user's reply (in Sent) is seen before the mail it answers.
        ingest_for_reading(
            &pool,
            source_id,
            sent,
            &[mail("b@x", "Re: Lunch", &["a@x"], 1, true)],
        )
        .await?;
        ingest_for_reading(
            &pool,
            source_id,
            inbox,
            &[mail("a@x", "Lunch", &[], 1, true)],
        )
        .await?;
        // An unrelated mail with the same subject but no headers stays apart:
        // only replies join by subject.
        ingest_for_reading(
            &pool,
            source_id,
            inbox,
            &[mail("c@x", "Lunch", &[], 2, true)],
        )
        .await?;
        // A header-less reply from a broken client joins by subject.
        ingest_for_reading(
            &pool,
            source_id,
            inbox,
            &[mail("d@x", "RE: lunch", &[], 3, true)],
        )
        .await?;

        let a = thread_of(&pool, "a@x").await?;
        assert!(a.is_some());
        assert_eq!(
            thread_of(&pool, "b@x").await?,
            a,
            "the reply joins the original"
        );
        assert_ne!(thread_of(&pool, "c@x").await?, a);
        assert!(thread_of(&pool, "d@x").await?.is_some());

        // A message referencing both threads merges them.
        let c = thread_of(&pool, "c@x").await?;
        ingest_for_reading(
            &pool,
            source_id,
            inbox,
            &[mail("e@x", "Re: Lunch", &["a@x", "c@x"], 4, true)],
        )
        .await?;
        let merged = thread_of(&pool, "e@x").await?;
        assert_eq!(thread_of(&pool, "a@x").await?, merged);
        assert_eq!(thread_of(&pool, "c@x").await?, merged);
        assert!(merged == a || merged == c);
        let threads: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM mail_threads WHERE id IN (?, ?)")
                .bind(a)
                .bind(c)
                .fetch_one(&pool)
                .await?;
        assert_eq!(threads, 1, "the merged-away thread is gone");
        Ok(())
    }

    #[tokio::test]
    async fn reading_only_mail_is_never_extracted_until_the_poll_brings_it_in() -> Result<()> {
        let (_tmp, pool, source_id, inbox) = setup().await?;

        // The reader sees the headers first: stored, but not for to-dos.
        let new =
            ingest_for_reading(&pool, source_id, inbox, &[mail("a@x", "Hi", &[], 5, true)]).await?;
        assert_eq!(new, 1);
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM embed_queue")
            .fetch_one(&pool)
            .await?;
        assert_eq!(queued, 0, "reading-only mail isn't embedded");

        // The extraction poll then fetches it whole.
        let batch = PollBatch {
            messages: vec![mail("a@x", "Hi", &[], 5, false)],
            next_cursor: Cursor("7:6".to_string()),
            more_available: false,
        };
        let candidates = ingest_batch(&pool, source_id, inbox, &batch).await?;
        assert_eq!(candidates, 1, "the poll makes it an extraction candidate");

        let (rows, body, reader_only, has_body): (i64, String, bool, bool) = sqlx::query_as(
            "SELECT COUNT(*), body, reader_only, has_body FROM messages WHERE external_id = 'a@x'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(rows, 1);
        assert_eq!(body, "the body");
        assert!(!reader_only && has_body);
        let cached: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM cache.mail_raw WHERE external_id = 'a@x'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(cached, 1, "the whole message is kept in the cache");
        Ok(())
    }
}
