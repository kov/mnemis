//! Syncing on its own: a full sync (mail, then to-dos) at startup and every
//! [`EVERY`] while the app runs, window open or hidden in the tray. Without an
//! LLM it syncs mail only. Sync now (button or tray) and this loop share one
//! lock, so syncs never overlap, and every sync tells the UI when it starts
//! and ends, so it shows "Syncing…" and reloads whatever started it.

use std::sync::Arc;
use std::time::Duration;

use mnemis_engine::{config, orchestrator};
use mnemis_types::SyncOutcome;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tracing::{info, warn};

use crate::AppState;

/// How often the full sync runs.
const EVERY: Duration = Duration::from_secs(15 * 60);

/// What the UI hears about syncs, whoever started them.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncEvent {
    Started,
    Finished,
}

/// A full sync now; errors without an LLM (Sync now promises to-dos).
pub async fn full_sync(state: &AppState) -> Result<SyncOutcome, String> {
    let _serialized = state.sync_lock.lock().await;
    run(state, false).await
}

/// Start the loop. `MNEMIS_NO_BACKGROUND_SYNC` (the UI tests) turns it off.
pub fn spawn(app: AppHandle) {
    if std::env::var("MNEMIS_NO_BACKGROUND_SYNC").is_ok() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        loop {
            if let Some(state) = app.try_state::<AppState>() {
                // A sync already running (Sync now) covers this turn.
                if let Ok(_serialized) = state.sync_lock.try_lock() {
                    match run(&state, true).await {
                        Ok(o) => info!(
                            messages = o.messages_ingested,
                            actions = o.actions_created,
                            errors = o.errors.len(),
                            "background sync done"
                        ),
                        Err(e) => warn!(error = %e, "background sync failed"),
                    }
                }
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// The UI subscribes to sync events.
#[tauri::command]
pub fn watch_sync(state: State<'_, AppState>, on_event: Channel<SyncEvent>) {
    if let Ok(mut watchers) = state.sync_watchers.lock() {
        watchers.push(on_event);
    }
}

async fn run(state: &AppState, mail_only_without_llm: bool) -> Result<SyncOutcome, String> {
    let stack = state.current_llm();
    if stack.is_none() && !mail_only_without_llm {
        return Err("No LLM configured. Set it in Settings → LLM.".to_string());
    }
    notify(state, SyncEvent::Started);
    let result = match stack {
        Some(stack) => {
            let traces = config::traces_dir_for(&state.db_path);
            orchestrator::sync_now(
                &state.pool,
                &stack.llm,
                Arc::clone(&stack.embedder),
                &stack.chat_model,
                stack.window_char_budget,
                Duration::from_secs(stack.chat_idle_timeout_secs),
                Some(&traces),
            )
            .await
        }
        None => orchestrator::sync_mail(&state.pool)
            .await
            .map(|errors| SyncOutcome {
                errors,
                ..Default::default()
            }),
    }
    .map_err(|e| format!("{e:#}"));
    notify(state, SyncEvent::Finished);
    result
}

fn notify(state: &AppState, event: SyncEvent) {
    if let Ok(mut watchers) = state.sync_watchers.lock() {
        // A send error means that webview is gone.
        watchers.retain(|w| w.send(event.clone()).is_ok());
    }
}
