use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::path::Path;
use std::sync::Once;

static INIT_VEC: Once = Once::new();

fn register_sqlite_vec() {
    INIT_VEC.call_once(|| unsafe {
        libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute::<
            unsafe extern "C" fn(),
            unsafe extern "C" fn(
                *mut libsqlite3_sys::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const libsqlite3_sys::sqlite3_api_routines,
            ) -> std::os::raw::c_int,
        >(sqlite_vec::sqlite3_vec_init)));
    });
}

/// Open the database at `path`, with the mail cache next to it
/// (`<stem>-cache.db`).
pub async fn open(path: &Path) -> Result<SqlitePool> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("mnemis");
    open_with_cache(path, &path.with_file_name(format!("{stem}-cache.db"))).await
}

/// Start a transaction that writes, holding the write lock from the start.
/// A deferred one that reads first fails at its first write, without
/// waiting (SQLITE_BUSY_SNAPSHOT, 517), when another connection wrote in
/// between; this one waits its turn instead (the busy timeout).
pub async fn begin_write(
    pool: &SqlitePool,
) -> sqlx::Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
    pool.begin_with("BEGIN IMMEDIATE").await
}

/// Open the database at `path` and attach the mail cache at `cache_path` as
/// schema `cache`. The cache holds only what can be fetched again (full
/// message bodies), so it belongs in the OS cache directory, where the
/// system may delete it: it is recreated empty.
pub async fn open_with_cache(path: &Path, cache_path: &Path) -> Result<SqlitePool> {
    register_sqlite_vec();

    if let Some(dir) = cache_path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating cache dir {}", dir.display()))?;
    }
    let cache = cache_path
        .to_str()
        .context("cache path is not UTF-8")?
        .to_string();

    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _meta| {
            let cache = cache.clone();
            Box::pin(async move {
                sqlx::query("ATTACH DATABASE ? AS cache")
                    .bind(cache)
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("PRAGMA cache.journal_mode = WAL")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query(
                    "CREATE TABLE IF NOT EXISTS cache.mail_raw (\
                         source_id INTEGER NOT NULL, \
                         external_id TEXT NOT NULL, \
                         raw BLOB NOT NULL, \
                         size INTEGER NOT NULL, \
                         fetched_at INTEGER NOT NULL, \
                         viewed_at INTEGER, \
                         PRIMARY KEY (source_id, external_id))",
                )
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .connect_with(opts)
        .await
        .with_context(|| format!("failed to open SQLite at {}", path.display()))?;

    Ok(pool)
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("migration failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn opens_and_migrates_clean_db() -> Result<()> {
        let tmp = TempDir::new()?;
        let path = tmp.path().join("mnemis.db");
        let pool = open(&path).await?;
        migrate(&pool).await?;

        // Sanity: a few tables exist.
        let names: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&pool)
                .await?;
        let names: Vec<String> = names.into_iter().map(|(n,)| n).collect();

        for required in [
            "sources",
            "channels",
            "messages",
            "actions",
            "contacts",
            "embed_queue",
            "chats",
            "memory_notes",
        ] {
            assert!(
                names.contains(&required.to_string()),
                "missing table {required}"
            );
        }

        // Sanity: vec0 virtual table works (proves sqlite-vec auto-extension fired).
        sqlx::query("INSERT INTO messages_vec(rowid, embedding) VALUES (1, vec_f32(?))")
            .bind(serde_json::to_string(&vec![0.5_f32; 768])?)
            .execute(&pool)
            .await?;

        Ok(())
    }
}
