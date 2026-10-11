//! Changes the user makes to mail (read/unread), applied locally at once
//! and queued in `mail_ops` until the server has them, so they survive
//! being offline. The latest change to a message wins; a queued change also
//! wins over the server's older flags during sync.

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use tracing::warn;

use super::sync::MailServer;

const SEEN: i64 = 0x01;
/// Attempts before a change that keeps failing is dropped.
const MAX_ATTEMPTS: i64 = 10;

/// Mark messages read or unread: every copy of each email in the account
/// (the same mail in Inbox and Archive, say).
pub async fn mark_seen(pool: &SqlitePool, message_ids: &[i64], seen: bool) -> Result<()> {
    let mut tx = pool.begin().await?;
    let copies: Vec<(i64, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT DISTINCT m2.id, m2.channel_id, m2.uid, m2.uidvalidity \
         FROM messages m \
         JOIN channels c ON c.id = m.channel_id \
         JOIN channels c2 ON c2.source_id = c.source_id \
         JOIN messages m2 ON m2.channel_id = c2.id AND m2.external_id = m.external_id \
         WHERE m.id IN (SELECT value FROM json_each(?)) AND m2.removed_at IS NULL",
    )
    .bind(serde_json::to_string(message_ids)?)
    .fetch_all(&mut *tx)
    .await
    .context("finding the messages")?;
    let now = Utc::now().timestamp();
    for (id, channel_id, uid, uidvalidity) in copies {
        sqlx::query(
            "UPDATE messages SET flags = CASE WHEN ? THEN flags | ? ELSE flags & ~? END WHERE id = ?",
        )
        .bind(seen)
        .bind(SEEN)
        .bind(SEEN)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        let (Some(uid), Some(uidvalidity)) = (uid, uidvalidity) else {
            continue;
        };
        sqlx::query("DELETE FROM mail_ops WHERE channel_id = ? AND uid = ?")
            .bind(channel_id)
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO mail_ops (channel_id, uid, uidvalidity, op, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(channel_id)
        .bind(uid)
        .bind(uidvalidity)
        .bind(if seen { "seen" } else { "unseen" })
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Send the account's queued changes to the server; returns how many it
/// took. A folder that fails keeps its changes for the next try.
pub async fn flush(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
) -> Result<usize> {
    let ops: Vec<(i64, i64, String, i64, i64, String)> = sqlx::query_as(
        "SELECT o.id, o.channel_id, c.external_id, o.uid, o.uidvalidity, o.op \
         FROM mail_ops o JOIN channels c ON c.id = o.channel_id \
         WHERE c.source_id = ? ORDER BY o.channel_id, o.id",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await
    .context("loading queued mail changes")?;
    let mut done = 0;
    for folder_ops in ops.chunk_by(|a, b| a.1 == b.1) {
        let (_, channel_id, folder, ..) = &folder_ops[0];
        let ids: Vec<i64> = folder_ops.iter().map(|o| o.0).collect();
        let result = async {
            let status = server.open(folder).await?;
            // Changes for UIDs of an earlier numbering of the folder no
            // longer point at the same mail: drop them.
            let current =
                |o: &&(i64, i64, String, i64, i64, String)| o.4 == status.uidvalidity as i64;
            for (op, seen) in [("seen", true), ("unseen", false)] {
                let uids: Vec<u32> = folder_ops
                    .iter()
                    .filter(current)
                    .filter(|o| o.5 == op)
                    .map(|o| o.3 as u32)
                    .collect();
                if !uids.is_empty() {
                    server.store_seen(&uids, seen).await?;
                }
            }
            anyhow::Ok(())
        }
        .await;
        let ids_json = serde_json::to_string(&ids)?;
        match result {
            Ok(()) => {
                sqlx::query("DELETE FROM mail_ops WHERE id IN (SELECT value FROM json_each(?))")
                    .bind(&ids_json)
                    .execute(pool)
                    .await?;
                done += ids.len();
            }
            Err(e) => {
                let error = format!("{e:#}");
                warn!(error, channel_id, "mail changes: folder failed");
                sqlx::query(
                    "UPDATE mail_ops SET attempts = attempts + 1, last_error = ? \
                     WHERE id IN (SELECT value FROM json_each(?))",
                )
                .bind(&error)
                .bind(&ids_json)
                .execute(pool)
                .await?;
                sqlx::query("DELETE FROM mail_ops WHERE attempts >= ?")
                    .bind(MAX_ATTEMPTS)
                    .execute(pool)
                    .await?;
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::sync::tests::{FakeServer, folder, setup};
    use crate::mail::sync::{sync_folder, sync_folders};

    async fn flags(pool: &SqlitePool, channel_id: i64) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT flags FROM messages WHERE channel_id = ?")
                .bind(channel_id)
                .fetch_one(pool)
                .await?,
        )
    }

    #[tokio::test]
    async fn reading_marks_every_copy_and_survives_being_offline() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        let inbox = folder(&pool, source_id, "INBOX", true).await?;
        let archive = folder(&pool, source_id, "Archive", true).await?;
        let mut server = FakeServer {
            condstore: true,
            ..Default::default()
        };
        server.add("INBOX", 4, "a@x", "Lunch", 1);
        server.add("Archive", 9, "a@x", "Lunch", 1);
        sync_folders(&pool, &mut server, source_id).await?;
        let id: i64 = sqlx::query_scalar("SELECT id FROM messages WHERE channel_id = ?")
            .bind(inbox)
            .fetch_one(&pool)
            .await?;

        mark_seen(&pool, &[id], true).await?;
        assert_eq!(flags(&pool, inbox).await? & SEEN, SEEN, "read here at once");
        assert_eq!(
            flags(&pool, archive).await? & SEEN,
            SEEN,
            "and its copy too"
        );

        // Offline: the change stays queued, and the server's stale flags
        // don't undo it.
        server.offline = true;
        sync_folder(&pool, &mut server, source_id, inbox, "INBOX", Utc::now()).await?;
        assert_eq!(flags(&pool, inbox).await? & SEEN, SEEN);
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_ops")
            .fetch_one(&pool)
            .await?;
        assert_eq!(queued, 2);

        server.offline = false;
        sync_folders(&pool, &mut server, source_id).await?;
        let mut stored = server.stored.clone();
        stored.sort();
        assert_eq!(
            stored,
            [
                ("Archive".to_string(), vec![9], true),
                ("INBOX".to_string(), vec![4], true)
            ]
        );
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_ops")
            .fetch_one(&pool)
            .await?;
        assert_eq!(queued, 0);

        // Marked unread again: the latest change is the one sent.
        mark_seen(&pool, &[id], false).await?;
        mark_seen(&pool, &[id], true).await?;
        mark_seen(&pool, &[id], false).await?;
        server.stored.clear();
        flush(&pool, &mut server, source_id).await?;
        assert!(server.stored.iter().all(|(_, _, seen)| !seen));
        assert_eq!(server.folder("INBOX").messages[&4].flags & 1, 0);
        Ok(())
    }
}
