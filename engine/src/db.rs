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

pub async fn open(path: &Path) -> Result<SqlitePool> {
    register_sqlite_vec();

    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
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
