//! Syncing on its own while the app runs, window open or hidden in the
//! tray: a full sync at startup and every [`EVERY`], and push between them,
//! where the mail servers can ([`mnemis_engine::mail::watch`]); where none
//! can, a mail-only sync every [`MAIL_EVERY`] instead. Every sync, this or
//! Sync now (button or tray), fetches mail first and tells the UI, so new
//! mail shows at once; then it looks for to-dos (skipped without an LLM).
//! Mail phases share one lock, so they never overlap; the engine runs one
//! to-do search at a time. The UI hears when a sync starts, when its mail
//! is in and when it ends, and when pushed mail changed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mnemis_engine::mail::watch::{MailChange, MailWatch, OnChange};
use mnemis_engine::{config, orchestrator};
use mnemis_types::SyncOutcome;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tracing::{info, warn};

use crate::AppState;

/// How often the full sync runs.
const EVERY: Duration = Duration::from_secs(15 * 60);
/// How often mail syncs when no server pushes.
const MAIL_EVERY: Duration = Duration::from_secs(5 * 60);

/// What the UI hears about syncs, whoever started them.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncEvent {
    Started,
    /// The mail is in; to-dos are being looked for.
    MailSynced,
    Finished,
    /// Mail changed between syncs (pushed, or a mail-only sync), or to-dos
    /// were found in it.
    Changed,
}

/// A full sync now; errors without an LLM (Sync now promises to-dos).
pub async fn full_sync(state: &AppState) -> Result<SyncOutcome, String> {
    if state.current_llm().is_none() {
        return Err("No LLM configured. Set it in Settings → LLM.".to_string());
    }
    let serialized = state.sync_lock.lock().await;
    run(state, serialized).await
}

/// Start the loop. `MNEMIS_NO_BACKGROUND_SYNC` (the UI tests) turns it off.
pub fn spawn(app: AppHandle) {
    if std::env::var("MNEMIS_NO_BACKGROUND_SYNC").is_ok() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let mut watch: Option<(Vec<i64>, MailWatch)> = None;
        let mut last_full: Option<Instant> = None;
        loop {
            if let Some(state) = app.try_state::<AppState>() {
                if last_full.is_none_or(|t| t.elapsed() >= EVERY) {
                    // A sync already running (Sync now) covers this turn.
                    if let Ok(serialized) = state.sync_lock.try_lock() {
                        match run(&state, serialized).await {
                            Ok(o) => info!(
                                messages = o.messages_ingested,
                                actions = o.actions_created,
                                errors = o.errors.len(),
                                "background sync done"
                            ),
                            Err(e) => warn!(error = %e, "background sync failed"),
                        }
                    }
                    last_full = Some(Instant::now());
                } else if !watch.as_ref().is_some_and(|(_, w)| w.active())
                    && let Ok(_serialized) = state.sync_lock.try_lock()
                {
                    if let Err(e) = orchestrator::sync_mail(&state.pool).await {
                        warn!(error = %format!("{e:#}"), "mail sync failed");
                    }
                    notify(&state, SyncEvent::Changed);
                }
                watch = keep_watching(&app, &state, watch).await;
            }
            tokio::time::sleep(MAIL_EVERY).await;
        }
    });
}

/// The watch over every account, started anew when the accounts changed.
async fn keep_watching(
    app: &AppHandle,
    state: &AppState,
    current: Option<(Vec<i64>, MailWatch)>,
) -> Option<(Vec<i64>, MailWatch)> {
    let accounts: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM sources WHERE status != 'disabled' AND kind = 'imap' ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    if current.as_ref().is_some_and(|(ids, _)| *ids == accounts) {
        return current;
    }
    drop(current);
    let app = app.clone();
    let on_change: OnChange = Arc::new(move |change: MailChange| {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            notify(&state, SyncEvent::Changed);
            if change.to_dos && state.current_llm().is_some() {
                match find_to_dos(&state).await {
                    Ok(found) if found.actions_created > 0 => notify(&state, SyncEvent::Changed),
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "finding to-dos in pushed mail failed"),
                }
            }
        });
    });
    match mnemis_engine::mail::watch::start(state.pool.clone(), on_change).await {
        Ok(watch) => Some((accounts, watch)),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "mail watch failed to start");
            None
        }
    }
}

/// Look for to-dos in mail not yet extracted (needs an LLM).
async fn find_to_dos(state: &AppState) -> Result<SyncOutcome, String> {
    let stack = state
        .current_llm()
        .ok_or_else(|| "No LLM configured.".to_string())?;
    let traces = config::traces_dir_for(&state.db_path);
    orchestrator::find_to_dos(
        &state.pool,
        &stack.llm,
        Arc::clone(&stack.embedder),
        &stack.chat_model,
        stack.window_char_budget,
        Duration::from_secs(stack.chat_idle_timeout_secs),
        Some(&traces),
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

/// The UI subscribes to sync events.
#[tauri::command]
pub fn watch_sync(state: State<'_, AppState>, on_event: Channel<SyncEvent>) {
    if let Ok(mut watchers) = state.sync_watchers.lock() {
        watchers.push(on_event);
    }
}

/// Mail (holding `serialized`, the mail-phase lock), then to-dos when an
/// LLM is configured.
async fn run(
    state: &AppState,
    serialized: tokio::sync::MutexGuard<'_, ()>,
) -> Result<SyncOutcome, String> {
    notify(state, SyncEvent::Started);
    let mail = orchestrator::sync_mail_outcome(&state.pool)
        .await
        .map_err(|e| format!("{e:#}"));
    drop(serialized);
    notify(state, SyncEvent::MailSynced);
    let result = match (mail, state.current_llm()) {
        (Err(e), _) => Err(e),
        (Ok(mail), None) => Ok(mail),
        (Ok(mut out), Some(_)) => match find_to_dos(state).await {
            Ok(found) => {
                out.embeddings_drained += found.embeddings_drained;
                out.actions_created += found.actions_created;
                out.errors.extend(found.errors);
                Ok(out)
            }
            Err(e) => Err(e),
        },
    };
    notify(state, SyncEvent::Finished);
    result
}

fn notify(state: &AppState, event: SyncEvent) {
    if let Ok(mut watchers) = state.sync_watchers.lock() {
        // A send error means that webview is gone.
        watchers.retain(|w| w.send(event.clone()).is_ok());
    }
}
