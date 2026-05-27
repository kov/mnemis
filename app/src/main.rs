#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use mnemis_engine::config::Config;
use mnemis_engine::embed::{Embedder, OmlxEmbedder};
use mnemis_engine::llm::LlmClient;
use mnemis_engine::{config, db, orchestrator, queries};
use mnemis_types::{ActionDto, MessageDto, StatusSnapshot, SyncOutcome};
use sqlx::SqlitePool;
use tauri::{Manager, State};
use tracing::{info, warn};

/// Wraps shared mutable engine state held by the Tauri runtime.
struct AppState {
    pool: SqlitePool,
    /// LLM + embedder, present when the user has a usable config.toml. If
    /// missing, sync_now is the only command that surfaces a clear error;
    /// read-only views continue to work.
    llm_stack: Option<LlmStack>,
}

struct LlmStack {
    llm: LlmClient,
    embedder: Arc<dyn Embedder>,
    chat_model: String,
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

#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<StatusSnapshot, String> {
    queries::get_status(&state.pool)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn sync_now(state: State<'_, AppState>) -> Result<SyncOutcome, String> {
    let stack = state
        .llm_stack
        .as_ref()
        .ok_or_else(|| "No LLM configured. Edit ~/.config/mnemis/config.toml.".to_string())?;
    orchestrator::sync_now(
        &state.pool,
        &stack.llm,
        Arc::clone(&stack.embedder),
        &stack.chat_model,
    )
    .await
    .map_err(|e| format!("{e:#}"))
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

            // Best-effort config load. The app remains usable read-only when
            // no config is present — only sync_now will fail. The
            // MNEMIS_DISABLE_LLM env (used by ui_smoke) forces this path so
            // tests can exercise sync_now without depending on whatever
            // config.toml the dev machine has.
            let llm_stack = if std::env::var("MNEMIS_DISABLE_LLM").is_ok() {
                info!("MNEMIS_DISABLE_LLM set; sync_now disabled");
                None
            } else {
                match config::load(None) {
                    Ok(cfg) => Some(build_llm_stack(&cfg)),
                    Err(e) => {
                        warn!(error = %e, "no mnemis config; sync_now disabled");
                        None
                    }
                }
            };

            app.manage(AppState { pool, llm_stack });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_actions,
            list_messages,
            get_status,
            sync_now
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn build_llm_stack(cfg: &Config) -> LlmStack {
    let mut llm = LlmClient::new(cfg.llm.base_url.clone(), cfg.llm.chat_model.clone());
    if let Some(t) = &cfg.llm.bearer_token {
        llm = llm.with_bearer_token(t.clone());
    }
    let mut embedder = OmlxEmbedder::new(cfg.llm.base_url.clone(), cfg.llm.embedding_model.clone());
    if let Some(t) = &cfg.llm.bearer_token {
        embedder = embedder.with_bearer_token(t.clone());
    }
    LlmStack {
        llm,
        embedder: Arc::new(embedder),
        chat_model: cfg.llm.chat_model.clone(),
    }
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
