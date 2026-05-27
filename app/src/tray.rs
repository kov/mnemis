//! macOS-only tray icon. Linux tray support is deferred to Phase 7 (needs
//! runtime D-Bus detection of `org.kde.StatusNotifierWatcher`); on Linux
//! the app currently runs as a regular window.
//!
//! The menu is intentionally tiny — "Show window / Sync now / Quit". Tray
//! is convenience; settings, status, and the actions list all live in the
//! main window.
#![cfg(target_os = "macos")]

use std::sync::Arc;

use anyhow::Result;
use mnemis_engine::orchestrator;
use tauri::{
    AppHandle, Manager,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tracing::{error, info, warn};

use super::AppState;

pub fn install(app: &AppHandle) -> Result<()> {
    let show = MenuItem::with_id(app, "show", "Show window", true, None::<&str>)?;
    let sync = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit mnemis", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &sync, &quit])?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no default window icon configured"))?;

    let _tray = TrayIconBuilder::with_id("main")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => focus_main(app),
            "sync" => spawn_sync(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn focus_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

fn spawn_sync(app: &AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = match handle.try_state::<AppState>() {
            Some(s) => s,
            None => {
                warn!("tray sync: AppState not yet managed");
                return;
            }
        };
        let Some(stack) = state.llm_stack.as_ref() else {
            warn!("tray sync: no LLM configured");
            return;
        };
        match orchestrator::sync_now(
            &state.pool,
            &stack.llm,
            Arc::clone(&stack.embedder),
            &stack.chat_model,
        )
        .await
        {
            Ok(o) => info!(
                sources_synced = o.sources_synced,
                actions_created = o.actions_created,
                "tray sync done"
            ),
            Err(e) => error!(error = %e, "tray sync failed"),
        }
    });
}
