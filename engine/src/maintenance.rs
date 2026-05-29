//! Maintenance helpers: data wipes, vacuums.

use anyhow::Result;
use sqlx::SqlitePool;

/// Tables holding user data — wiped by [`reset_data`]. Settings, sources,
/// channels, contacts, user_profile, and extraction_directives are preserved
/// (sources/channels get their health and cursor columns rewound).
const CLEAR_TABLES: &[&str] = &[
    // action graph
    "action_events",
    "action_evidence",
    "actions_vec",
    "actions",
    // ingest pipeline
    "embed_queue",
    "extraction_runs",
    "dismissal_feedback",
    "messages_vec",
    "messages", // FTS cleared by trigger
    // memory notes
    "memory_notes_vec",
    "memory_notes", // FTS cleared by trigger
    // assistant artefacts
    "reports",
    "chat_turn_reasoning",
    "chat_turns",
    "chats",
    // people are extracted from messages — rebuilt on next sync
    "people",
];

pub struct ResetCounts {
    pub before: Vec<(&'static str, i64)>,
    pub after: Vec<(&'static str, i64)>,
}

pub async fn count_user_data(pool: &SqlitePool) -> Result<Vec<(&'static str, i64)>> {
    let mut out = Vec::with_capacity(CLEAR_TABLES.len());
    for t in CLEAR_TABLES {
        let sql = format!("SELECT COUNT(*) FROM {t}");
        let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_one(pool)
            .await?;
        out.push((*t, n));
    }
    Ok(out)
}

/// Wipe all message/action/etc data; keep sources, channels, contacts,
/// settings, user_profile, extraction_directives. Source and channel health
/// columns are rewound so the next sync re-bootstraps.
pub async fn reset_data(pool: &SqlitePool) -> Result<ResetCounts> {
    let before = count_user_data(pool).await?;
    let mut tx = pool.begin().await?;
    for t in CLEAR_TABLES {
        let sql = format!("DELETE FROM {t}");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "UPDATE sources SET last_synced_at = NULL, last_error = NULL, \
         consecutive_failures = 0, status = 'ok'",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE channels SET cursor = NULL, last_synced_at = NULL")
        .execute(&mut *tx)
        .await?;
    // sqlite_sequence is auto-created when an AUTOINCREMENT column exists;
    // safe to no-op if absent.
    let _ = sqlx::query("DELETE FROM sqlite_sequence")
        .execute(&mut *tx)
        .await;
    tx.commit().await?;
    // VACUUM must run outside a transaction.
    sqlx::query("VACUUM").execute(pool).await?;
    let after = count_user_data(pool).await?;
    Ok(ResetCounts { before, after })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::ingest::ingest_batch;
    use crate::source::{Cursor, ImportedAuthor, ImportedMessage, PollBatch, SourceId};
    use chrono::{DateTime, Utc};
    use tempfile::TempDir;

    async fn seeded() -> Result<(TempDir, SqlitePool, i64, i64)> {
        let tmp = TempDir::new()?;
        let path = tmp.path().join("test.db");
        let pool = db::open(&path).await?;
        db::migrate(&pool).await?;

        let now = Utc::now().timestamp();
        let (source_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, last_synced_at, created_at) \
             VALUES ('imap', 'test', 'kc/test', ?, ?) RETURNING id",
        )
        .bind(now)
        .bind(now)
        .fetch_one(&pool)
        .await?;
        let (channel_id,): (i64,) = sqlx::query_as(
            "INSERT INTO channels (source_id, external_id, name, kind, cursor, last_synced_at) \
             VALUES (?, 'INBOX', 'INBOX', 'mailbox', 'pos:42', ?) RETURNING id",
        )
        .bind(source_id)
        .bind(now)
        .fetch_one(&pool)
        .await?;
        sqlx::query("INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, 'Me', ?)")
            .bind(now)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO contacts (display_name, created_at, updated_at) VALUES ('Boss', ?, ?)",
        )
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await?;

        let msg = ImportedMessage {
            external_id: "m1".to_string(),
            parent_external_id: None,
            author: Some(ImportedAuthor {
                external_id: "a@b.com".to_string(),
                display_name: None,
                handle: None,
            }),
            posted_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            subject: Some("S".to_string()),
            body: "hello world".to_string(),
            body_format: "text".to_string(),
            recipients: Vec::new(),
            raw_json: None,
            flags: 0,
        };
        ingest_batch(
            &pool,
            SourceId(source_id),
            channel_id,
            &PollBatch {
                messages: vec![msg],
                next_cursor: Cursor("99:100".to_string()),
                more_available: false,
            },
        )
        .await?;
        sqlx::query(
            "INSERT INTO actions (title, confidence, status, extracted_at) \
             VALUES ('a', 'high', 'auto_claimed', ?)",
        )
        .bind(now)
        .execute(&pool)
        .await?;
        Ok((tmp, pool, source_id, channel_id))
    }

    #[tokio::test]
    async fn reset_data_wipes_data_keeps_settings_rewinds_cursors() -> Result<()> {
        // Pin the user-visible contract of the reset path:
        // - messages, actions, people gone
        // - sources, channels, contacts, user_profile intact
        // - channel cursor + source health rewound so sync re-bootstraps
        let (_tmp, pool, source_id, channel_id) = seeded().await?;

        let pre_msgs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages")
            .fetch_one(&pool)
            .await?;
        assert!(pre_msgs.0 > 0, "test setup should have seeded messages");

        let _ = reset_data(&pool).await?;

        let (msgs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages")
            .fetch_one(&pool)
            .await?;
        let (actions,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM actions")
            .fetch_one(&pool)
            .await?;
        let (people,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM people")
            .fetch_one(&pool)
            .await?;
        assert_eq!(msgs, 0);
        assert_eq!(actions, 0);
        assert_eq!(people, 0);

        // Settings kept.
        let (sources,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sources WHERE id = ?")
            .bind(source_id)
            .fetch_one(&pool)
            .await?;
        let (channels,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM channels WHERE id = ?")
            .bind(channel_id)
            .fetch_one(&pool)
            .await?;
        let (profile,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_profile")
            .fetch_one(&pool)
            .await?;
        let (contacts,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM contacts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(sources, 1);
        assert_eq!(channels, 1);
        assert_eq!(profile, 1);
        assert_eq!(contacts, 1);

        // Cursors/health rewound.
        let (cursor, last_synced): (Option<String>, Option<i64>) =
            sqlx::query_as("SELECT cursor, last_synced_at FROM channels WHERE id = ?")
                .bind(channel_id)
                .fetch_one(&pool)
                .await?;
        assert!(cursor.is_none(), "channel cursor should be rewound");
        assert!(
            last_synced.is_none(),
            "channel last_synced_at should be cleared"
        );

        let (status, last_error, failures, src_last_synced): (
            String,
            Option<String>,
            i64,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT status, last_error, consecutive_failures, last_synced_at \
                 FROM sources WHERE id = ?",
        )
        .bind(source_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(status, "ok");
        assert!(last_error.is_none());
        assert_eq!(failures, 0);
        assert!(src_last_synced.is_none());

        Ok(())
    }
}
