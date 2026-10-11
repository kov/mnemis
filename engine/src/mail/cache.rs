//! The mail cache: full RFC 822 messages, in the `cache` schema (a separate
//! file in the OS cache directory, see [`crate::db::open_with_cache`]). Keyed
//! by account and Message-ID, so one email filed in two folders is stored
//! once. Everything the reader shows is derived from these bytes; a message
//! missing here is fetched from the server again.

use anyhow::{Context, Result};
use sqlx::SqliteConnection;

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
