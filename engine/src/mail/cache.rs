//! The mail cache: full RFC 822 messages, in the `cache` schema (a separate
//! file in the OS cache directory, see [`crate::db::open_with_cache`]). Keyed
//! by account and Message-ID, so one email filed in two folders is stored
//! once. Everything the reader shows is derived from these bytes; a message
//! missing here is fetched from the server again.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use sqlx::{SqliteConnection, SqlitePool};

use super::sync::MailServer;
use crate::source::SourceId;

/// How long a message stays cached after it arrived or was last read.
#[cfg(not(target_os = "ios"))]
pub const KEEP_DAYS: i64 = 7;
#[cfg(target_os = "ios")]
pub const KEEP_DAYS: i64 = 3;
/// The cache's size cap; the least recently used go first.
#[cfg(not(target_os = "ios"))]
pub const MAX_BYTES: i64 = 1 << 30;
#[cfg(target_os = "ios")]
pub const MAX_BYTES: i64 = 300 << 20;
/// Bigger messages aren't fetched ahead, only when opened.
const PREFETCH_MAX_SIZE: i64 = 2 << 20;
/// Messages fetched ahead per sync, so one sync never runs long.
const PREFETCH_PER_SYNC: i64 = 200;
const CHUNK: usize = 20;

/// Whether syncs fetch ahead. The iOS app turns it off on metered or
/// low-data networks; opening a thread still downloads it.
static PREFETCH_ALLOWED: AtomicBool = AtomicBool::new(true);

pub fn set_prefetch_allowed(allowed: bool) {
    PREFETCH_ALLOWED.store(allowed, Ordering::Relaxed);
}

pub fn prefetch_allowed() -> bool {
    PREFETCH_ALLOWED.load(Ordering::Relaxed)
}

/// Mail kept whatever its age: every message of a thread behind an open
/// to-do, or answered by an unsent draft. `m` is the messages row.
const PINNED: &str = "m.thread_id IN ( \
        SELECT m2.thread_id FROM messages m2 \
        JOIN action_evidence ae ON ae.message_id = m2.id \
        JOIN actions a ON a.id = ae.action_id \
        WHERE a.status IN ('pending', 'auto_claimed', 'claimed') AND m2.thread_id IS NOT NULL \
        UNION \
        SELECT m3.thread_id FROM messages m3 JOIN drafts d ON d.message_id = m3.id \
        WHERE d.status = 'draft' AND m3.thread_id IS NOT NULL)";

pub(crate) async fn store(
    conn: &mut SqliteConnection,
    source_id: i64,
    external_id: &str,
    raw: &[u8],
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO cache.mail_raw (source_id, external_id, raw, size, fetched_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (source_id, external_id) DO UPDATE SET \
             raw = excluded.raw, size = excluded.size, fetched_at = excluded.fetched_at",
    )
    .bind(source_id)
    .bind(external_id)
    .bind(raw)
    .bind(raw.len() as i64)
    .bind(now)
    .execute(conn)
    .await
    .context("caching message")?;
    Ok(())
}

pub(crate) async fn get(
    conn: &mut SqliteConnection,
    source_id: i64,
    external_id: &str,
) -> Result<Option<Vec<u8>>> {
    sqlx::query_scalar("SELECT raw FROM cache.mail_raw WHERE source_id = ? AND external_id = ?")
        .bind(source_id)
        .bind(external_id)
        .fetch_optional(conn)
        .await
        .context("reading the mail cache")
}

/// Fetch ahead what the user is likely to open, so it reads instantly:
/// threads behind open to-dos, then everything from the last [`KEEP_DAYS`],
/// newest first. Returns how many messages were fetched.
pub async fn prefetch(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
    now: i64,
) -> Result<usize> {
    let cutoff = now - KEEP_DAYS * 86_400;
    let wanted: Vec<(i64, String, i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT m.channel_id, c.external_id, m.uid, m.uidvalidity \
         FROM messages m JOIN channels c ON c.id = m.channel_id \
         WHERE c.source_id = ?1 AND m.uid IS NOT NULL AND m.removed_at IS NULL \
           AND COALESCE(m.size, 0) <= ?2 \
           AND (m.posted_at >= ?3 OR {PINNED}) \
           AND NOT EXISTS (SELECT 1 FROM cache.mail_raw r \
                           WHERE r.source_id = ?1 AND r.external_id = m.external_id) \
         GROUP BY m.external_id \
         ORDER BY ({PINNED}) DESC, m.posted_at DESC LIMIT ?4"
    )))
    .bind(source_id)
    .bind(PREFETCH_MAX_SIZE)
    .bind(cutoff)
    .bind(PREFETCH_PER_SYNC)
    .fetch_all(pool)
    .await
    .context("choosing mail to fetch ahead")?;

    fetch_wanted(pool, server, source_id, wanted).await
}

/// Fetch a thread's messages that aren't cached, whatever their size or
/// age: the user is opening it. Returns how many were fetched.
pub async fn fetch_thread(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
    thread_id: i64,
) -> Result<usize> {
    let wanted: Vec<(i64, String, i64, i64)> = sqlx::query_as(
        "SELECT m.channel_id, c.external_id, m.uid, m.uidvalidity \
         FROM messages m JOIN channels c ON c.id = m.channel_id \
         WHERE m.thread_id = ?1 AND c.source_id = ?2 AND m.uid IS NOT NULL \
           AND m.removed_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM cache.mail_raw r \
                           WHERE r.source_id = ?2 AND r.external_id = m.external_id) \
         GROUP BY m.external_id ORDER BY m.posted_at",
    )
    .bind(thread_id)
    .bind(source_id)
    .fetch_all(pool)
    .await
    .context("finding the thread's mail")?;
    fetch_wanted(pool, server, source_id, wanted).await
}

/// Fetch `(channel, folder, uid, uidvalidity)` rows, folder by folder.
async fn fetch_wanted(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
    wanted: Vec<(i64, String, i64, i64)>,
) -> Result<usize> {
    let mut by_folder: Vec<(i64, String, i64, Vec<u32>)> = Vec::new();
    for (channel_id, folder, uid, uidvalidity) in wanted {
        match by_folder.iter_mut().find(|f| f.0 == channel_id) {
            Some(f) => f.3.push(uid as u32),
            None => by_folder.push((channel_id, folder, uidvalidity, vec![uid as u32])),
        }
    }
    let mut fetched = 0;
    for (channel_id, folder, uidvalidity, uids) in by_folder {
        let status = server.open(&folder).await?;
        if status.uidvalidity as i64 != uidvalidity {
            continue;
        }
        for chunk in uids.chunks(CHUNK) {
            fetched += fetch_into_cache(
                pool,
                server,
                source_id,
                channel_id,
                uidvalidity as u32,
                chunk,
            )
            .await?;
        }
    }
    Ok(fetched)
}

/// Fetch whole messages from the open folder: they fill in their rows'
/// bodies and land in the cache.
pub(crate) async fn fetch_into_cache(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
    channel_id: i64,
    uidvalidity: u32,
    uids: &[u32],
) -> Result<usize> {
    let messages: Vec<_> = server
        .messages(uids)
        .await?
        .into_iter()
        .filter_map(|m| crate::source::imap::imported(&m.raw, m.uid, uidvalidity, m.flags, false))
        .collect();
    crate::ingest::ingest_for_reading(pool, SourceId(source_id), channel_id, &messages).await?;
    Ok(messages.len())
}

/// Drop cached messages neither recent nor read lately (unless pinned, see
/// [`PINNED`]), those no folder holds any more, then the least recently
/// used until the cache fits in [`MAX_BYTES`]. Returns how many went.
pub async fn evict(pool: &SqlitePool, now: i64) -> Result<u64> {
    let cutoff = now - KEEP_DAYS * 86_400;
    let mut dropped = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM cache.mail_raw AS r \
         WHERE NOT EXISTS (SELECT 1 FROM messages m JOIN channels c ON c.id = m.channel_id \
                           WHERE c.source_id = r.source_id AND m.external_id = r.external_id \
                             AND m.removed_at IS NULL) \
            OR (MAX(r.fetched_at, COALESCE(r.viewed_at, 0)) < ? \
                AND NOT EXISTS (SELECT 1 FROM messages m JOIN channels c ON c.id = m.channel_id \
                                WHERE c.source_id = r.source_id AND m.external_id = r.external_id \
                                  AND {PINNED}))"
    )))
    .bind(cutoff)
    .execute(pool)
    .await
    .context("clearing out the mail cache")?
    .rows_affected();

    let rows: Vec<(i64, String, i64)> = sqlx::query_as(
        "SELECT source_id, external_id, size FROM cache.mail_raw \
         ORDER BY MAX(fetched_at, COALESCE(viewed_at, 0)) DESC",
    )
    .fetch_all(pool)
    .await?;
    let mut total = 0i64;
    let mut over = Vec::new();
    for (source_id, external_id, size) in rows {
        total += size;
        if total > MAX_BYTES {
            over.push((source_id, external_id));
        }
    }
    for (source_id, external_id) in over {
        dropped +=
            sqlx::query("DELETE FROM cache.mail_raw WHERE source_id = ? AND external_id = ?")
                .bind(source_id)
                .bind(external_id)
                .execute(pool)
                .await?
                .rows_affected();
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::sync::sync_folders;
    use crate::mail::sync::tests::{FakeServer, folder, setup};

    async fn cached(pool: &SqlitePool) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar("SELECT external_id FROM cache.mail_raw ORDER BY external_id")
                .fetch_all(pool)
                .await?,
        )
    }

    /// An open to-do whose evidence is `external_id`.
    async fn todo_on(pool: &SqlitePool, external_id: &str) -> Result<i64> {
        let action: i64 = sqlx::query_scalar(
            "INSERT INTO actions (title, confidence, status, extracted_at) \
             VALUES ('Reply to Ana', 'high', 'pending', 0) RETURNING id",
        )
        .fetch_one(pool)
        .await?;
        sqlx::query(
            "INSERT INTO action_evidence (action_id, message_id, is_primary) \
             SELECT ?, id, 1 FROM messages WHERE external_id = ?",
        )
        .bind(action)
        .bind(external_id)
        .execute(pool)
        .await?;
        Ok(action)
    }

    #[tokio::test]
    async fn fetches_recent_mail_and_threads_behind_to_dos_ahead() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        folder(&pool, source_id, "Archive", true).await?;
        let mut server = FakeServer::default();
        server.add("Archive", 1, "recent@x", "Lunch", 1);
        server.add("Archive", 2, "old@x", "Last month", 20);
        server.add("Archive", 3, "todo@x", "Contract", 30);
        sync_folders(&pool, &mut server, source_id).await?;
        todo_on(&pool, "todo@x").await?;

        let now = chrono::Utc::now().timestamp();
        let fetched = prefetch(&pool, &mut server, source_id, now).await?;
        assert_eq!(fetched, 2);
        assert_eq!(cached(&pool).await?, ["recent@x", "todo@x"]);
        let (body, has_body): (String, bool) =
            sqlx::query_as("SELECT body, has_body FROM messages WHERE external_id = 'recent@x'")
                .fetch_one(&pool)
                .await?;
        assert!(has_body && body.contains("The body of recent@x"));

        // Nothing left to fetch: the next sync asks for nothing.
        server.fetched.clear();
        prefetch(&pool, &mut server, source_id, now).await?;
        assert!(server.fetched.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn clears_out_old_mail_but_keeps_what_was_read_lately_or_is_pinned() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        folder(&pool, source_id, "Archive", true).await?;
        let mut server = FakeServer::default();
        for (uid, id) in [(1, "stale@x"), (2, "read@x"), (3, "todo@x"), (4, "fresh@x")] {
            server.add("Archive", uid, id, id, 1);
        }
        sync_folders(&pool, &mut server, source_id).await?;
        let now = chrono::Utc::now().timestamp();
        prefetch(&pool, &mut server, source_id, now).await?;
        todo_on(&pool, "todo@x").await?;

        let long_ago = now - (KEEP_DAYS + 1) * 86_400;
        sqlx::query("UPDATE cache.mail_raw SET fetched_at = ? WHERE external_id != 'fresh@x'")
            .bind(long_ago)
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE cache.mail_raw SET viewed_at = ? WHERE external_id = 'read@x'")
            .bind(now - 3600)
            .execute(&pool)
            .await?;

        assert_eq!(evict(&pool, now).await?, 1);
        assert_eq!(cached(&pool).await?, ["fresh@x", "read@x", "todo@x"]);
        let body: String =
            sqlx::query_scalar("SELECT body FROM messages WHERE external_id = 'stale@x'")
                .fetch_one(&pool)
                .await?;
        assert!(
            body.contains("The body of stale@x"),
            "the stored text stays"
        );

        // Gone from the server: no reason to keep it.
        server.folder("Archive").messages.remove(&4);
        sync_folders(&pool, &mut server, source_id).await?;
        evict(&pool, now).await?;
        assert_eq!(cached(&pool).await?, ["read@x", "todo@x"]);
        Ok(())
    }
}
