#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
mod tray;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use mnemis_engine::config::Config;
use mnemis_engine::embed::{Embedder, OmlxEmbedder};
use mnemis_engine::llm::LlmClient;
use mnemis_engine::{chat, config, db, mutations, orchestrator, queries, settings};
use mnemis_types::{
    ActionDto, ActionStatus, ChannelRowDto, ChatDto, ChatEvent, ChatTurnDto, FeedbackKind,
    LlmConfigDto, MessageDto, PendingResolutionDto, SourceRowDto, StatusSnapshot, SyncOutcome,
    UserProfileDto,
};
use sqlx::SqlitePool;
use tauri::ipc::Channel;
use tauri::{Manager, State};
use tracing::{info, warn};

/// Wraps shared mutable engine state held by the Tauri runtime.
pub(crate) struct AppState {
    pub pool: SqlitePool,
    /// LLM + embedder, present when the user has a usable config.toml. If
    /// missing, sync_now is the only command that surfaces a clear error;
    /// read-only views continue to work.
    pub llm_stack: Option<LlmStack>,
    /// Resolved path to the SQLite file, kept so commands can derive the
    /// traces directory (`<db_parent>/traces/`) without recomputing it.
    pub db_path: PathBuf,
}

pub(crate) struct LlmStack {
    pub llm: LlmClient,
    pub embedder: Arc<dyn Embedder>,
    pub chat_model: String,
    pub window_char_budget: usize,
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

#[tauri::command(rename_all = "snake_case")]
async fn update_action(
    state: State<'_, AppState>,
    action_id: i64,
    new_status: ActionStatus,
    dismissed_reason: Option<String>,
) -> Result<(), String> {
    mutations::update_action_status(&state.pool, action_id, new_status, dismissed_reason)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn get_user_profile(state: State<'_, AppState>) -> Result<UserProfileDto, String> {
    settings::get_user_profile(&state.pool)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn save_user_profile(
    state: State<'_, AppState>,
    profile: UserProfileDto,
) -> Result<(), String> {
    settings::save_user_profile(&state.pool, &profile)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn is_first_run(state: State<'_, AppState>) -> Result<bool, String> {
    settings::is_first_run(&state.pool)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn list_settings_sources(state: State<'_, AppState>) -> Result<Vec<SourceRowDto>, String> {
    settings::list_sources(&state.pool)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn list_source_channels(
    state: State<'_, AppState>,
    source_id: i64,
) -> Result<Vec<ChannelRowDto>, String> {
    settings::list_source_channels(&state.pool, source_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn set_channel_muted(
    state: State<'_, AppState>,
    channel_id: i64,
    muted: bool,
) -> Result<(), String> {
    settings::set_channel_muted(&state.pool, channel_id, muted)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn set_channels_muted_bulk(
    state: State<'_, AppState>,
    channel_ids: Vec<i64>,
    muted: bool,
) -> Result<(), String> {
    settings::set_channels_muted_bulk(&state.pool, &channel_ids, muted)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn set_source_muted(
    state: State<'_, AppState>,
    source_id: i64,
    muted: bool,
) -> Result<(), String> {
    settings::set_source_muted(&state.pool, source_id, muted)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn add_imap_source(
    state: State<'_, AppState>,
    name: String,
    server: String,
    port: u16,
    username: String,
    password: String,
) -> Result<i64, String> {
    settings::add_imap_source(&state.pool, &name, &server, port, &username, &password)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn delete_source(state: State<'_, AppState>, source_id: i64) -> Result<(), String> {
    settings::delete_source(&state.pool, source_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn get_llm_config(_state: State<'_, AppState>) -> Result<LlmConfigDto, String> {
    let cfg_path = config::default_config_path();
    match config::load(None) {
        Ok(cfg) => Ok(LlmConfigDto {
            base_url: cfg.llm.base_url,
            chat_model: cfg.llm.chat_model,
            embedding_model: cfg.llm.embedding_model,
            bearer_token: cfg.llm.bearer_token,
            config_path: cfg_path.display().to_string(),
        }),
        Err(_) => Ok(LlmConfigDto {
            config_path: cfg_path.display().to_string(),
            ..Default::default()
        }),
    }
}

#[tauri::command(rename_all = "snake_case")]
async fn save_llm_config(cfg: LlmConfigDto) -> Result<(), String> {
    config::save_llm(&config::LlmSection {
        base_url: cfg.base_url,
        chat_model: cfg.chat_model,
        embedding_model: cfg.embedding_model,
        bearer_token: cfg.bearer_token.filter(|s| !s.trim().is_empty()),
        // None → save_llm keeps whatever's already in the file. This form
        // edits neither max_context_tokens nor request_timeout_secs
        // (config.toml-only knobs for now).
        max_context_tokens: None,
        request_timeout_secs: None,
    })
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn list_pending_resolutions(
    state: State<'_, AppState>,
) -> Result<Vec<PendingResolutionDto>, String> {
    queries::list_pending_resolutions(&state.pool)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn confirm_resolution(
    state: State<'_, AppState>,
    action_id: i64,
    new_status: ActionStatus,
) -> Result<(), String> {
    // Routes through the same status-update path the user buttons use; that
    // already writes a user-driven 'resolved' event which suppresses the
    // suggestion via the NOT EXISTS clause in list_pending_resolutions.
    mutations::update_action_status(&state.pool, action_id, new_status, None)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn reject_resolution(state: State<'_, AppState>, action_id: i64) -> Result<(), String> {
    mutations::reject_resolution_suggestion(&state.pool, action_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn submit_dismissal_feedback(
    state: State<'_, AppState>,
    action_id: i64,
    kind: FeedbackKind,
    comment: Option<String>,
) -> Result<(), String> {
    mutations::record_dismissal_feedback(&state.pool, action_id, kind, comment)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn sync_now(state: State<'_, AppState>) -> Result<SyncOutcome, String> {
    let stack = state
        .llm_stack
        .as_ref()
        .ok_or_else(|| "No LLM configured. Edit ~/.config/mnemis/config.toml.".to_string())?;
    let traces = config::traces_dir_for(&state.db_path);
    orchestrator::sync_now(
        &state.pool,
        &stack.llm,
        Arc::clone(&stack.embedder),
        &stack.chat_model,
        stack.window_char_budget,
        Some(&traces),
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn list_chats(
    state: State<'_, AppState>,
    include_archived: bool,
) -> Result<Vec<ChatDto>, String> {
    chat::store::list_chats(&state.pool, include_archived)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn set_chat_archived(
    state: State<'_, AppState>,
    chat_id: i64,
    archived: bool,
) -> Result<(), String> {
    chat::store::set_archived(&state.pool, chat_id, archived)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn delete_chat(state: State<'_, AppState>, chat_id: i64) -> Result<(), String> {
    chat::store::delete_chat(&state.pool, chat_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn create_chat(
    state: State<'_, AppState>,
    seeded_from_kind: Option<String>,
    seeded_from_id: Option<i64>,
) -> Result<i64, String> {
    chat::create_chat(&state.pool, seeded_from_kind.as_deref(), seeded_from_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn get_chat_turns(
    state: State<'_, AppState>,
    chat_id: i64,
) -> Result<Vec<ChatTurnDto>, String> {
    chat::store::load_turns(&state.pool, chat_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// A short label of what a seeded chat is about (or null for a blank chat), so
/// the UI can show the "Talk about this" context.
#[tauri::command(rename_all = "snake_case")]
async fn get_chat_seed(state: State<'_, AppState>, chat_id: i64) -> Result<Option<String>, String> {
    chat::prompt::seed_label(&state.pool, chat_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Run one chat turn, streaming each loop event to the frontend over `on_event`
/// (a Tauri typed Channel). Every event is persisted to SQLite *before* it's
/// sent, so the transcript survives even if the window closes mid-answer.
#[tauri::command(rename_all = "snake_case")]
async fn send_chat_message(
    state: State<'_, AppState>,
    chat_id: i64,
    text: String,
    on_event: Channel<ChatEvent>,
) -> Result<(), String> {
    let stack = state
        .llm_stack
        .as_ref()
        .ok_or_else(|| "No LLM configured. Edit ~/.config/mnemis/config.toml.".to_string())?;
    let system_prompt = chat::prompt::build_system_prompt(&state.pool, chat_id)
        .await
        .map_err(|e| format!("{e:#}"))?;
    // Forward each engine event to the frontend channel. run_chat_turn already
    // emits a terminal Error event before returning Err, so the UI sees the
    // failure either way; we still surface it as a command error for logging.
    let sink = move |e: ChatEvent| {
        let _ = on_event.send(e);
    };
    let traces = config::traces_dir_for(&state.db_path);
    chat::run_chat_turn(
        &state.pool,
        &stack.llm,
        &system_prompt,
        chat_id,
        &text,
        stack.window_char_budget,
        &sink,
        Some(&traces),
    )
    .await
    .map_err(|e| format!("{e:#}"))?;

    // Best-effort: give an unseeded chat a real title from its first message.
    // Detached so it doesn't hold the channel open — the composer re-enables as
    // soon as this command returns and the `Channel` drops; the title lands in
    // the list on its next view.
    let pool = state.pool.clone();
    let llm = stack.llm.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = chat::maybe_generate_title(&pool, &llm, chat_id, &text).await {
            tracing::debug!(error = %format!("{e:#}"), "chat title generation failed");
        }
    });
    Ok(())
}

/// Chat UI preference: show the model's reasoning. Defaults on; persisted in
/// the `settings` table so it's remembered across runs.
#[tauri::command]
async fn get_chat_show_reasoning(state: State<'_, AppState>) -> Result<bool, String> {
    settings::get_chat_show_reasoning(&state.pool)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command(rename_all = "snake_case")]
async fn set_chat_show_reasoning(state: State<'_, AppState>, value: bool) -> Result<(), String> {
    settings::set_chat_show_reasoning(&state.pool, value)
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

    let mut builder = tauri::Builder::default();
    // Single-instance routing is the right UX for end users, but it would
    // serialize concurrent UI tests (each spawned app would defer to the
    // first one's window). The harness sets MNEMIS_NO_SINGLE_INSTANCE so
    // each test gets its own fresh app process.
    if std::env::var_os("MNEMIS_NO_SINGLE_INSTANCE").is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Second launch: surface and focus the existing window instead.
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }));
    }
    // Lets the chat view hand external links (rendered from the assistant's
    // markdown) to the OS default browser instead of navigating the webview.
    builder = builder.plugin(tauri_plugin_opener::init());
    builder
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

            app.manage(AppState {
                pool,
                llm_stack,
                db_path: app_data,
            });

            // macOS: tray-resident. Linux/Windows: window-only for now.
            // Linux runtime tray detection is Phase 7.
            #[cfg(target_os = "macos")]
            tray::install(app.handle())?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // macOS hide-to-tray: intercept close and hide the window.
            // Cmd+Q / the tray Quit menu item exits normally.
            #[cfg(target_os = "macos")]
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
            {
                api.prevent_close();
                let _ = window.hide();
            }
            // Silence unused-var warnings on non-macOS.
            #[cfg(not(target_os = "macos"))]
            {
                let _ = window;
                let _ = event;
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_actions,
            list_messages,
            get_status,
            sync_now,
            update_action,
            submit_dismissal_feedback,
            list_pending_resolutions,
            confirm_resolution,
            reject_resolution,
            get_user_profile,
            save_user_profile,
            is_first_run,
            list_settings_sources,
            set_source_muted,
            delete_source,
            add_imap_source,
            list_source_channels,
            set_channel_muted,
            set_channels_muted_bulk,
            get_llm_config,
            save_llm_config,
            list_chats,
            create_chat,
            set_chat_archived,
            delete_chat,
            get_chat_turns,
            get_chat_seed,
            send_chat_message,
            get_chat_show_reasoning,
            set_chat_show_reasoning
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn build_llm_stack(cfg: &Config) -> LlmStack {
    let mut llm = LlmClient::new(cfg.llm.base_url.clone(), cfg.llm.chat_model.clone())
        .with_timeout(cfg.llm.resolved_request_timeout_secs());
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
        window_char_budget: mnemis_engine::extract::window_char_budget_for(
            cfg.llm.resolved_max_context_tokens(),
        ),
    }
}

/// Resolve the SQLite path.
///
/// Order:
/// 1. `MNEMIS_DB_PATH` env override (handy for tests + pointing dev builds
///    at a specific database).
/// 2. The CLI config's `paths.db` (or its default of
///    `~/.local/share/mnemis/mnemis.db` on Linux,
///    `~/Library/Application Support/mnemis/mnemis.db` on macOS).
///
/// Falling back to `config::default_db_path()` rather than Tauri's
/// `app_data_dir()` is deliberate: the CLI and the desktop app must share
/// one database, and the CLI's path predates the Tauri bundle identifier.
fn resolve_db_path(_app: &tauri::App) -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("MNEMIS_DB_PATH") {
        return Ok(PathBuf::from(p));
    }
    // Respect an explicit config override; otherwise fall through to the
    // built-in default. Config loading is best-effort; a missing file is
    // not a failure here.
    if let Ok(cfg) = config::load(None) {
        return Ok(cfg.db_path());
    }
    Ok(config::default_db_path())
}
