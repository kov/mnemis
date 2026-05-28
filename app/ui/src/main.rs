use leptos::prelude::*;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::path;
use mnemis_types::{
    ActionDto, ActionStatus, Confidence, FeedbackKind, LlmConfigDto, MessageDto,
    PendingResolutionDto, SourceRowDto, StatusSnapshot, SyncOutcome, UserProfileDto,
};
use serde::Serialize;
use wasm_bindgen::prelude::*;

mod components;

#[wasm_bindgen]
extern "C" {
    // Tauri 2 always exposes `__TAURI_INTERNALS__.invoke`. The friendlier
    // `__TAURI__.core.invoke` is only present when `app.withGlobalTauri` is
    // enabled in `tauri.conf.json` — we don't bother with that flag.
    #[wasm_bindgen(js_namespace = ["window", "__TAURI_INTERNALS__"], catch)]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;
}

#[derive(Serialize)]
struct ListActionsArgs {
    include_resolved: bool,
}

#[derive(Serialize)]
struct ListMessagesArgs {
    limit: Option<i64>,
}

async fn fetch_actions(include_resolved: bool) -> Result<Vec<ActionDto>, String> {
    let args = serde_wasm_bindgen::to_value(&ListActionsArgs { include_resolved })
        .map_err(|e| e.to_string())?;
    let raw = invoke("list_actions", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Vec<ActionDto>>(raw).map_err(|e| e.to_string())
}

async fn fetch_messages(limit: Option<i64>) -> Result<Vec<MessageDto>, String> {
    let args =
        serde_wasm_bindgen::to_value(&ListMessagesArgs { limit }).map_err(|e| e.to_string())?;
    let raw = invoke("list_messages", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Vec<MessageDto>>(raw).map_err(|e| e.to_string())
}

async fn fetch_status() -> Result<StatusSnapshot, String> {
    let raw = invoke("get_status", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<StatusSnapshot>(raw).map_err(|e| e.to_string())
}

pub async fn run_sync_now() -> Result<SyncOutcome, String> {
    let raw = invoke("sync_now", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<SyncOutcome>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct UpdateActionArgs {
    action_id: i64,
    new_status: ActionStatus,
    dismissed_reason: Option<String>,
}

pub async fn update_action(
    action_id: i64,
    new_status: ActionStatus,
    dismissed_reason: Option<String>,
) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&UpdateActionArgs {
        action_id,
        new_status,
        dismissed_reason,
    })
    .map_err(|e| e.to_string())?;
    invoke("update_action", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

pub async fn fetch_user_profile() -> Result<UserProfileDto, String> {
    let raw = invoke("get_user_profile", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<UserProfileDto>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct SaveProfileArgs {
    profile: UserProfileDto,
}

pub async fn save_user_profile(profile: UserProfileDto) -> Result<(), String> {
    let args =
        serde_wasm_bindgen::to_value(&SaveProfileArgs { profile }).map_err(|e| e.to_string())?;
    invoke("save_user_profile", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

pub async fn fetch_is_first_run() -> Result<bool, String> {
    let raw = invoke("is_first_run", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<bool>(raw).map_err(|e| e.to_string())
}

pub async fn fetch_settings_sources() -> Result<Vec<SourceRowDto>, String> {
    let raw = invoke("list_settings_sources", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Vec<SourceRowDto>>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct SourceIdArgs {
    source_id: i64,
}

#[derive(Serialize)]
struct SetSourceMutedArgs {
    source_id: i64,
    muted: bool,
}

pub async fn set_source_muted(source_id: i64, muted: bool) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&SetSourceMutedArgs { source_id, muted })
        .map_err(|e| e.to_string())?;
    invoke("set_source_muted", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

#[derive(Serialize)]
struct AddImapArgs {
    name: String,
    server: String,
    port: u16,
    username: String,
    password: String,
}

pub async fn add_imap_source(
    name: String,
    server: String,
    port: u16,
    username: String,
    password: String,
) -> Result<i64, String> {
    let args = serde_wasm_bindgen::to_value(&AddImapArgs {
        name,
        server,
        port,
        username,
        password,
    })
    .map_err(|e| e.to_string())?;
    let raw = invoke("add_imap_source", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<i64>(raw).map_err(|e| e.to_string())
}

pub async fn delete_source(source_id: i64) -> Result<(), String> {
    let args =
        serde_wasm_bindgen::to_value(&SourceIdArgs { source_id }).map_err(|e| e.to_string())?;
    invoke("delete_source", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

pub async fn fetch_llm_config() -> Result<LlmConfigDto, String> {
    let raw = invoke("get_llm_config", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<LlmConfigDto>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct SaveLlmArgs {
    cfg: LlmConfigDto,
}

pub async fn save_llm_config(cfg: LlmConfigDto) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&SaveLlmArgs { cfg }).map_err(|e| e.to_string())?;
    invoke("save_llm_config", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

pub async fn fetch_pending_resolutions() -> Result<Vec<PendingResolutionDto>, String> {
    let raw = invoke("list_pending_resolutions", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Vec<PendingResolutionDto>>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct ConfirmResolutionArgs {
    action_id: i64,
    new_status: ActionStatus,
}

pub async fn confirm_resolution(action_id: i64, new_status: ActionStatus) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&ConfirmResolutionArgs {
        action_id,
        new_status,
    })
    .map_err(|e| e.to_string())?;
    invoke("confirm_resolution", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

#[derive(Serialize)]
struct RejectResolutionArgs {
    action_id: i64,
}

pub async fn reject_resolution(action_id: i64) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&RejectResolutionArgs { action_id })
        .map_err(|e| e.to_string())?;
    invoke("reject_resolution", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

#[derive(Serialize)]
struct SubmitFeedbackArgs {
    action_id: i64,
    kind: FeedbackKind,
    comment: Option<String>,
}

pub async fn submit_dismissal_feedback(
    action_id: i64,
    kind: FeedbackKind,
    comment: Option<String>,
) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&SubmitFeedbackArgs {
        action_id,
        kind,
        comment,
    })
    .map_err(|e| e.to_string())?;
    invoke("submit_dismissal_feedback", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

/// Newtype around the sync-tick signal so contexts don't collide with the
/// first-run-tick (both are `RwSignal<u32>`).
#[derive(Clone, Copy)]
pub struct SyncTick(pub RwSignal<u32>);

/// Bumped only when the profile is saved; the first-run banner refetches
/// from this so the banner disappears without remounting the profile form
/// (which would lose the "Saved." toast mid-render).
#[derive(Clone, Copy)]
pub struct FirstRunTick(pub RwSignal<u32>);

#[component]
fn App() -> impl IntoView {
    provide_context(SyncTick(RwSignal::new(0u32)));
    provide_context(FirstRunTick(RwSignal::new(0u32)));

    view! {
        <Router>
            <div class="app">
                <components::FirstRunBanner />
                <components::StatusPanel />
                <nav class="nav">
                    <A href="/">"Actions"</A>
                    <A href="/inbox">"Inbox"</A>
                    <A href="/settings">"Settings"</A>
                </nav>
                <Routes fallback=|| view! { <div class="empty">"Not found"</div> }>
                    <Route path=path!("/") view=ActionsPage />
                    <Route path=path!("/inbox") view=InboxPage />
                    <Route path=path!("/settings") view=SettingsPage />
                    <Route path=path!("/settings/profile") view=SettingsProfilePage />
                    <Route path=path!("/settings/llm") view=SettingsLlmPage />
                    <Route path=path!("/settings/sources") view=SettingsSourcesPage />
                </Routes>
            </div>
        </Router>
    }
}

#[component]
fn SettingsPage() -> impl IntoView {
    view! { <components::SettingsHome /> }
}

#[component]
fn SettingsProfilePage() -> impl IntoView {
    view! { <components::SettingsProfile /> }
}

#[component]
fn SettingsLlmPage() -> impl IntoView {
    view! { <components::SettingsLlm /> }
}

#[component]
fn SettingsSourcesPage() -> impl IntoView {
    view! { <components::SettingsSources /> }
}

#[component]
fn ActionsPage() -> impl IntoView {
    view! { <components::ActionsPage /> }
}

#[component]
fn InboxPage() -> impl IntoView {
    let SyncTick(sync_tick) = use_context::<SyncTick>().expect("sync tick context");
    let messages = LocalResource::new(move || {
        // Subscribing to sync_tick here makes the resource re-fetch whenever
        // StatusPanel bumps it after a successful sync.
        let _ = sync_tick.get();
        async move { fetch_messages(None).await }
    });
    view! {
        <h1>"Inbox"</h1>
        <Suspense fallback=|| view! { <div class="loading">"Loading…"</div> }>
            {move || messages.get().map(|res| match res {
                Ok(rows) => view! { <components::InboxList rows=rows /> }.into_any(),
                Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
            })}
        </Suspense>
    }
}

/// Pure-frontend helpers for action presentation. Lives here for now so we
/// don't pull serde or chrono into the wasm crate just for two strings.
pub(crate) fn confidence_class(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "badge badge-high",
        Confidence::Medium => "badge badge-medium",
        Confidence::Low => "badge badge-low",
    }
}

pub(crate) fn status_label(s: ActionStatus) -> &'static str {
    match s {
        ActionStatus::Pending => "pending",
        ActionStatus::AutoClaimed => "auto-claimed",
        ActionStatus::Claimed => "claimed",
        ActionStatus::Done => "done",
        ActionStatus::Cancelled => "cancelled",
        ActionStatus::Dismissed => "dismissed",
    }
}
