#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use anyhow::Context;
use mnemis_engine::{db, queries};
use mnemis_types::{ActionDto, MessageDto};
use sqlx::SqlitePool;
use tauri::{Manager, State};
use tracing::info;

/// Wraps shared mutable engine state held by the Tauri runtime.
struct AppState {
    pool: SqlitePool,
}

#[tauri::command(rename_all = "snake_case")]
async fn list_actions(
    state: State<'_, AppState>,
    include_resolved: bool,
) -> Result<Vec<ActionDto>, String> {
    queries::list_actions(&state.pool, queries::ActionFilter { include_resolved })
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command(rename_all = "snake_case")]
async fn list_messages(
    state: State<'_, AppState>,
    limit: Option<i64>,
) -> Result<Vec<MessageDto>, String> {
    let filter = match limit {
        Some(n) => queries::MessageFilter { limit: n },
        None => queries::MessageFilter::default(),
    };
    queries::list_messages(&state.pool, filter)
        .await
        .map_err(|e| e.to_string())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,mnemis_engine=info")),
        )
        .init();

    // Tauri 2's async_runtime wraps tokio but doesn't bootstrap one by
    // default. Install a multi-threaded tokio runtime so engine code (sqlx,
    // reqwest, IMAP) can use it from commands and setup hooks.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("building tokio runtime");
    tauri::async_runtime::set(rt.handle().clone());
    // Leak the runtime so it lives for the whole process. Otherwise dropping
    // it at end of main would race with Tauri's shutdown.
    Box::leak(Box::new(rt));

    tauri::Builder::default()
        .setup(|app| {
            let app_data = resolve_db_path(app)?;
            if let Some(parent) = app_data.parent() {
                std::fs::create_dir_all(parent).context("creating app data dir")?;
            }
            info!(path = %app_data.display(), "opening sqlite");

            let pool = tauri::async_runtime::block_on(async {
                let pool = db::open(&app_data).await?;
                db::migrate(&pool).await?;
                anyhow::Ok(pool)
            })?;

            app.manage(AppState { pool });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![list_actions, list_messages])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Resolve the SQLite path. Prefers `MNEMIS_DB_PATH` (useful for pointing the
/// dev build at the existing CLI database); otherwise uses Tauri's per-OS
/// app data dir.
fn resolve_db_path(app: &tauri::App) -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("MNEMIS_DB_PATH") {
        return Ok(PathBuf::from(p));
    }
    let dir = app
        .path()
        .app_data_dir()
        .context("resolving app data dir")?;
    Ok(dir.join("mnemis.db"))
}
