use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::hooks::use_location;
use leptos_router::path;
use mnemis_types::{
    ActionDto, ActionStatus, AppearanceDto, CaldavAccountDto, CaldavCollectionDto, CaldavSyncDto,
    ChannelRowDto, ChatDto, ChatEvent, ChatTurnDto, ColorScheme, Confidence, FeedbackKind,
    LlmConfigDto, MessageDetailDto, MessageDto, PendingResolutionDto, SourceRowDto, StatusSnapshot,
    SyncOutcome, UserProfileDto,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

mod components;
mod markdown;

#[wasm_bindgen]
extern "C" {
    // Tauri 2 always exposes `__TAURI_INTERNALS__.invoke`. The friendlier
    // `__TAURI__.core.invoke` is only present when `app.withGlobalTauri` is
    // enabled in `tauri.conf.json` — we don't bother with that flag.
    #[wasm_bindgen(js_namespace = ["window", "__TAURI_INTERNALS__"], catch)]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    // Registers a JS callback and returns its numeric id. The Tauri `Channel`
    // wire protocol: pass `"__CHANNEL__:<id>"` as a command arg, and each
    // `channel.send(x)` (and an `{end:true}` on drop) is delivered to this
    // callback. Lets us stream without pulling in `@tauri-apps/api`.
    #[wasm_bindgen(js_namespace = ["window", "__TAURI_INTERNALS__"])]
    fn transformCallback(callback: &Closure<dyn FnMut(JsValue)>, once: bool) -> f64;
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

#[derive(Serialize)]
struct MessageIdArgs {
    message_id: i64,
}

/// Full detail for one message (the reading pane), loaded on selection.
pub async fn fetch_message(message_id: i64) -> Result<MessageDetailDto, String> {
    let args =
        serde_wasm_bindgen::to_value(&MessageIdArgs { message_id }).map_err(|e| e.to_string())?;
    let raw = invoke("get_message", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<MessageDetailDto>(raw).map_err(|e| e.to_string())
}

async fn fetch_status() -> Result<StatusSnapshot, String> {
    let raw = invoke("get_status", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<StatusSnapshot>(raw).map_err(|e| e.to_string())
}

/// Read the host desktop's accent + color scheme from the backend.
pub async fn fetch_appearance() -> Result<AppearanceDto, String> {
    let raw = invoke("get_appearance", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<AppearanceDto>(raw).map_err(|e| e.to_string())
}

/// Legible text color (dark vs white) for an accent fill, picked by relative
/// luminance — so a light accent (e.g. yellow) gets dark text on selected rows
/// and buttons rather than invisible white.
fn on_accent_for(hex: &str) -> &'static str {
    if hex.len() < 7 {
        return "#ffffff";
    }
    let ch = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f64 / 255.0;
    let lum = 0.2126 * ch(1) + 0.7152 * ch(3) + 0.0722 * ch(5);
    if lum > 0.6 { "#1a1a18" } else { "#ffffff" }
}

/// Apply the OS appearance to the document root: inject `--accent` (and the
/// derived `--on-accent`), and drive the theme from the OS color scheme as an
/// explicit `data-theme` override — cleared when the OS has no preference so we
/// fall back to the `prefers-color-scheme` media query. webkit2gtk doesn't
/// always track the GTK setting via that media query, so on Linux this explicit
/// read is what actually makes the app open in the right theme.
fn apply_appearance(a: &AppearanceDto) {
    let Some(root) = leptos::web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .and_then(|e| e.dyn_into::<leptos::web_sys::HtmlElement>().ok())
    else {
        return;
    };
    let style = root.style();
    if let Some(hex) = a.accent.as_deref() {
        let _ = style.set_property("--accent", hex);
        let _ = style.set_property("--on-accent", on_accent_for(hex));
    }
    match a.color_scheme {
        ColorScheme::Dark => {
            let _ = root.set_attribute("data-theme", "dark");
        }
        ColorScheme::Light => {
            let _ = root.set_attribute("data-theme", "light");
        }
        ColorScheme::NoPreference => {
            let _ = root.remove_attribute("data-theme");
        }
    }
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

/// Refresh folders from the server, then return the list. Used when the source
/// settings view opens so newly created folders appear without a sync.
pub async fn discover_source_channels(source_id: i64) -> Result<Vec<ChannelRowDto>, String> {
    let args =
        serde_wasm_bindgen::to_value(&SourceIdArgs { source_id }).map_err(|e| e.to_string())?;
    let raw = invoke("discover_source_channels", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Vec<ChannelRowDto>>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct SetChannelMutedArgs {
    channel_id: i64,
    muted: bool,
}

pub async fn set_channel_muted(channel_id: i64, muted: bool) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&SetChannelMutedArgs { channel_id, muted })
        .map_err(|e| e.to_string())?;
    invoke("set_channel_muted", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

#[derive(Serialize)]
struct SetChannelsMutedBulkArgs {
    channel_ids: Vec<i64>,
    muted: bool,
}

pub async fn set_channels_muted_bulk(channel_ids: Vec<i64>, muted: bool) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&SetChannelsMutedBulkArgs { channel_ids, muted })
        .map_err(|e| e.to_string())?;
    invoke("set_channels_muted_bulk", args)
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

// ---- CalDAV reminders ----------------------------------------------------

pub async fn fetch_caldav_account() -> Result<CaldavAccountDto, String> {
    let raw = invoke("get_caldav_account", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<CaldavAccountDto>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct AddCaldavArgs {
    base_url: String,
    username: String,
    password: String,
}

pub async fn add_caldav_account(
    base_url: String,
    username: String,
    password: String,
) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&AddCaldavArgs {
        base_url,
        username,
        password,
    })
    .map_err(|e| e.to_string())?;
    invoke("add_caldav_account", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

pub async fn delete_caldav_account() -> Result<(), String> {
    invoke("delete_caldav_account", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

pub async fn list_caldav_collections() -> Result<Vec<CaldavCollectionDto>, String> {
    let raw = invoke("list_caldav_collections", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Vec<CaldavCollectionDto>>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct SetCollectionArgs {
    url: String,
    display_name: Option<String>,
}

pub async fn set_caldav_collection(
    url: String,
    display_name: Option<String>,
) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&SetCollectionArgs { url, display_name })
        .map_err(|e| e.to_string())?;
    invoke("set_caldav_collection", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

pub async fn run_sync_caldav() -> Result<CaldavSyncDto, String> {
    let raw = invoke("sync_caldav", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<CaldavSyncDto>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct PromoteReminderArgs {
    action_id: i64,
    due_at: i64,
}

pub async fn promote_to_reminder(action_id: i64, due_at: i64) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&PromoteReminderArgs { action_id, due_at })
        .map_err(|e| e.to_string())?;
    invoke("promote_to_reminder", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
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

#[derive(Serialize)]
struct ListChatsArgs {
    include_archived: bool,
}

pub async fn fetch_chats(include_archived: bool) -> Result<Vec<ChatDto>, String> {
    let args = serde_wasm_bindgen::to_value(&ListChatsArgs { include_archived })
        .map_err(|e| e.to_string())?;
    let raw = invoke("list_chats", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Vec<ChatDto>>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct SetChatArchivedArgs {
    chat_id: i64,
    archived: bool,
}

pub async fn set_chat_archived(chat_id: i64, archived: bool) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&SetChatArchivedArgs { chat_id, archived })
        .map_err(|e| e.to_string())?;
    invoke("set_chat_archived", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

pub async fn delete_chat(chat_id: i64) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&ChatIdArgs { chat_id }).map_err(|e| e.to_string())?;
    invoke("delete_chat", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

/// Stop the in-flight chat send for `chat_id` (the Stop button). Best-effort:
/// the streaming send returns its partial answer and the turn ends cleanly.
pub async fn cancel_chat_message(chat_id: i64) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&ChatIdArgs { chat_id }).map_err(|e| e.to_string())?;
    invoke("cancel_chat_message", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

#[derive(Serialize)]
struct CreateChatArgs {
    seeded_from_kind: Option<String>,
    seeded_from_id: Option<i64>,
}

pub async fn create_chat(
    seeded_from_kind: Option<String>,
    seeded_from_id: Option<i64>,
) -> Result<i64, String> {
    let args = serde_wasm_bindgen::to_value(&CreateChatArgs {
        seeded_from_kind,
        seeded_from_id,
    })
    .map_err(|e| e.to_string())?;
    let raw = invoke("create_chat", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<i64>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct FindSeededChatArgs {
    seeded_from_kind: String,
    seeded_from_id: i64,
}

/// The most recent chat seeded from an entity (e.g. an action), if any.
pub async fn fetch_seeded_chat(kind: &str, id: i64) -> Result<Option<i64>, String> {
    let args = serde_wasm_bindgen::to_value(&FindSeededChatArgs {
        seeded_from_kind: kind.to_string(),
        seeded_from_id: id,
    })
    .map_err(|e| e.to_string())?;
    let raw = invoke("find_seeded_chat", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Option<i64>>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct ChatIdArgs {
    chat_id: i64,
}

pub async fn fetch_chat_turns(chat_id: i64) -> Result<Vec<ChatTurnDto>, String> {
    let args = serde_wasm_bindgen::to_value(&ChatIdArgs { chat_id }).map_err(|e| e.to_string())?;
    let raw = invoke("get_chat_turns", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Vec<ChatTurnDto>>(raw).map_err(|e| e.to_string())
}

pub async fn fetch_chat_seed(chat_id: i64) -> Result<Option<String>, String> {
    let args = serde_wasm_bindgen::to_value(&ChatIdArgs { chat_id }).map_err(|e| e.to_string())?;
    let raw = invoke("get_chat_seed", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Option<String>>(raw).map_err(|e| e.to_string())
}

pub async fn fetch_chat_show_reasoning() -> Result<bool, String> {
    let raw = invoke("get_chat_show_reasoning", JsValue::NULL)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<bool>(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct SetShowReasoningArgs {
    value: bool,
}

pub async fn set_chat_show_reasoning(value: bool) -> Result<(), String> {
    let args =
        serde_wasm_bindgen::to_value(&SetShowReasoningArgs { value }).map_err(|e| e.to_string())?;
    invoke("set_chat_show_reasoning", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

#[derive(Serialize)]
struct OpenUrlArgs {
    url: String,
    with: Option<String>,
}

/// Hand a URL to the OS default handler via `tauri-plugin-opener`, rather than
/// letting the click navigate the webview away from the app.
pub async fn open_external(url: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&OpenUrlArgs { url, with: None })
        .map_err(|e| e.to_string())?;
    invoke("plugin:opener|open_url", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    Ok(())
}

/// One delivery from a streamed chat turn: an engine event, or the channel
/// closing (after the command returns and the Tauri `Channel` is dropped).
pub enum ChatStream {
    Event(ChatEvent),
    End,
}

/// The envelope the Tauri `Channel` callback receives: either `{message, index}`
/// for a sent value or `{end: true, index}` when the channel drops.
#[derive(Deserialize)]
struct ChannelEnvelope {
    #[serde(default)]
    end: bool,
    message: Option<ChatEvent>,
}

#[derive(Serialize)]
struct SendChatArgs {
    chat_id: i64,
    text: String,
    on_event: String,
}

/// Start a chat turn, delivering each streamed `ChatEvent` (then `End`) to
/// `on_msg`. We register a JS callback via `transformCallback` and hand its id
/// to the `send_chat_message` command as `"__CHANNEL__:<id>"`; the engine
/// persists every turn before emitting, so the `End` handler can safely refetch
/// the authoritative transcript. Payloads are parsed with `serde_json` (robust
/// for internally-tagged enums) rather than `serde_wasm_bindgen`.
pub fn send_chat_message<F>(chat_id: i64, text: String, mut on_msg: F)
where
    F: FnMut(ChatStream) + 'static,
{
    let closure = Closure::wrap(Box::new(move |payload: JsValue| {
        let Some(s) = js_sys::JSON::stringify(&payload)
            .ok()
            .and_then(|v| v.as_string())
        else {
            return;
        };
        match serde_json::from_str::<ChannelEnvelope>(&s) {
            Ok(env) if env.end => on_msg(ChatStream::End),
            Ok(env) => {
                if let Some(ev) = env.message {
                    on_msg(ChatStream::Event(ev));
                }
            }
            Err(_) => {}
        }
    }) as Box<dyn FnMut(JsValue)>);

    let id = transformCallback(&closure, false);
    // The callback fires for every streamed event until the channel closes, so
    // it must outlive this function. Leak it — one per user message (user-paced,
    // so the bound is trivial).
    closure.forget();

    let on_event = format!("__CHANNEL__:{}", id as u64);
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(args) = serde_wasm_bindgen::to_value(&SendChatArgs {
            chat_id,
            text,
            on_event,
        }) {
            let _ = invoke("send_chat_message", args).await;
        }
    });
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

/// Chat state that has to outlive `ChatDetail` remounts. The component is torn
/// down and rebuilt on every navigation, so anything kept in component-local
/// signals (the in-flight stream, the reasoning toggle) is lost when the user
/// flips to the chats list and back. Hoisting it to app scope means a send
/// started in one visit keeps streaming into the same buffers, and returning to
/// the chat shows it continuing rather than a frozen, half-finished transcript.
///
/// `active_id` records which chat the in-flight send belongs to, so a different
/// chat's `ChatDetail` doesn't render someone else's stream.
#[derive(Clone, Copy)]
pub struct ChatUiState {
    /// "Show reasoning" — persisted so toggling it survives navigation.
    pub show_reasoning: RwSignal<bool>,
    /// The chat id of the in-flight send (None when idle).
    pub active_id: RwSignal<Option<i64>>,
    /// The user message being answered (shown optimistically until its
    /// persisted copy appears in the transcript).
    pub pending_user: RwSignal<Option<String>>,
    /// Assistant text accumulated from streamed `Delta` events for the
    /// in-flight turn — rendered live, then cleared the moment the completed
    /// (persisted) message arrives so nothing shows twice.
    pub streaming_text: RwSignal<String>,
    /// True while a send is in flight (one at a time).
    pub sending: RwSignal<bool>,
    /// Bumped on every streamed event (and on completion) so any mounted
    /// `ChatDetail` refetches the authoritative, persisted transcript. The
    /// engine persists each turn *before* emitting its event, so a refetch
    /// always reflects what was just streamed — no parallel live buffer to
    /// double up against the DB after a navigate-away-and-back.
    pub refresh: RwSignal<u32>,
    /// A turn-level failure `(chat_id, message)`. Errors aren't persisted as
    /// turns (they'd pollute the transcript + history), so they ride this
    /// signal instead; scoped by chat id so one chat's failure doesn't surface
    /// while viewing another.
    pub error: RwSignal<Option<(i64, String)>>,
}

#[component]
fn App() -> impl IntoView {
    provide_context(SyncTick(RwSignal::new(0u32)));
    provide_context(FirstRunTick(RwSignal::new(0u32)));
    // Default the reasoning toggle ON; the persisted preference (below)
    // overrides it once loaded.
    let chat_ui = ChatUiState {
        show_reasoning: RwSignal::new(true),
        active_id: RwSignal::new(None),
        pending_user: RwSignal::new(None),
        streaming_text: RwSignal::new(String::new()),
        sending: RwSignal::new(false),
        refresh: RwSignal::new(0u32),
        error: RwSignal::new(None),
    };
    provide_context(chat_ui);
    // Remember "show reasoning" across runs (stored in the DB settings table).
    let show_reasoning = chat_ui.show_reasoning;
    spawn_local(async move {
        if let Ok(v) = fetch_chat_show_reasoning().await {
            show_reasoning.set(v);
        }
    });
    // Theme the app from the OS: inject the system accent and color scheme on
    // load. Best-effort — a failure leaves the CSS fallback in place.
    spawn_local(async move {
        if let Ok(appearance) = fetch_appearance().await {
            apply_appearance(&appearance);
        }
    });

    view! {
        <Router>
            <div class="app">
                <Sidebar />
                <main class="main">
                    <Toolbar />
                    <div class="outlet">
                        <components::FirstRunBanner />
                        <Routes fallback=|| view! { <div class="empty">"Not found"</div> }>
                    <Route path=path!("/") view=ActionsPage />
                    <Route path=path!("/inbox") view=InboxPage />
                    <Route path=path!("/chats") view=ChatsListPage />
                    <Route path=path!("/chats/:id") view=ChatDetailPage />
                    <Route path=path!("/settings") view=SettingsPage />
                    <Route path=path!("/settings/profile") view=SettingsProfilePage />
                    <Route path=path!("/settings/llm") view=SettingsLlmPage />
                    <Route path=path!("/settings/sources") view=SettingsSourcesPage />
                    <Route path=path!("/settings/reminders") view=SettingsRemindersPage />
                        </Routes>
                    </div>
                </main>
            </div>
        </Router>
    }
}

/// The full-height translucent sidebar: brand, smart-view nav (keeps `nav.nav`
/// for routing + tests), the per-account list, and the status/sync footer.
#[component]
fn Sidebar() -> impl IntoView {
    view! {
        <aside class="sidebar">
            <div class="brand"><span class="glyph"></span>"mnemis"</div>
            <div class="side-scroll">
                <nav class="nav side-nav">
                    <div class="side-label">"Smart views"</div>
                    <A href="/" attr:class="side-item"><span class="ico">"\u{25CE}"</span>"Actions"</A>
                    <A href="/inbox" attr:class="side-item"><span class="ico">"\u{2709}"</span>"Inbox"</A>
                    <A href="/chats" attr:class="side-item"><span class="ico">"\u{25C8}"</span>"Chats"</A>
                    <A href="/settings" attr:class="side-item"><span class="ico">"\u{2699}"</span>"Settings"</A>
                </nav>
                <AccountsNav />
            </div>
            <div class="side-footer-status">
                <components::StatusPanel />
            </div>
        </aside>
    }
}

/// Lists configured sources under an "Accounts" header. Reacts to `SyncTick` so
/// a newly added source appears after a sync without a reload. Each entry links
/// to the inbox for now (per-source filtering is a later step).
#[component]
fn AccountsNav() -> impl IntoView {
    let SyncTick(sync_tick) = use_context::<SyncTick>().expect("sync tick context");
    let sources = LocalResource::new(move || {
        let _ = sync_tick.get();
        async move { fetch_settings_sources().await }
    });
    view! {
        <Suspense fallback=|| view! { <></> }>
            {move || sources.get().and_then(|res| match res {
                Ok(rows) if rows.is_empty() => None,
                Ok(rows) => Some(view! {
                    <div class="side-section">
                        <div class="side-label">"Accounts"</div>
                        <For
                            each=move || rows.clone()
                            key=|s| s.id
                            children=move |s: SourceRowDto| view! {
                                <A href="/inbox" attr:class="side-item account">
                                    <span class="acc-dot"></span>{s.name.clone()}
                                </A>
                            }
                        />
                    </div>
                }.into_any()),
                Err(_) => None,
            })}
        </Suspense>
    }
}

/// The main-pane toolbar. Derives its title from the current route so the shell
/// reads like a native window title bar above the content.
#[component]
fn Toolbar() -> impl IntoView {
    let location = use_location();
    let title = move || {
        let p = location.pathname.get();
        if p == "/" {
            "Actions"
        } else if p.starts_with("/inbox") {
            "Inbox"
        } else if p.starts_with("/chats") {
            "Chats"
        } else if p.starts_with("/settings") {
            "Settings"
        } else {
            "mnemis"
        }
    };
    view! {
        <header class="toolbar">
            <span class="title">{title}</span>
            <span class="spacer"></span>
        </header>
    }
}

#[component]
fn SettingsPage() -> impl IntoView {
    view! { <div class="doc"><components::SettingsHome /></div> }
}

#[component]
fn SettingsProfilePage() -> impl IntoView {
    view! { <div class="doc"><components::SettingsProfile /></div> }
}

#[component]
fn SettingsLlmPage() -> impl IntoView {
    view! { <div class="doc"><components::SettingsLlm /></div> }
}

#[component]
fn SettingsSourcesPage() -> impl IntoView {
    view! { <div class="doc"><components::SettingsSources /></div> }
}

#[component]
fn SettingsRemindersPage() -> impl IntoView {
    view! { <div class="doc"><components::SettingsReminders /></div> }
}

#[component]
fn ActionsPage() -> impl IntoView {
    view! { <components::ActionsPage /> }
}

#[component]
fn ChatsListPage() -> impl IntoView {
    view! { <div class="doc"><components::ChatsPage /></div> }
}

#[component]
fn ChatDetailPage() -> impl IntoView {
    view! { <div class="doc"><components::ChatDetail /></div> }
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
        <Suspense fallback=|| view! {
            <div class="body">
                <div class="list"></div>
                <div class="reading"><div class="reading-empty">"Loading…"</div></div>
            </div>
        }>
            {move || messages.get().map(|res| match res {
                Ok(rows) => view! { <components::InboxPane rows=rows /> }.into_any(),
                Err(e) => view! {
                    <div class="reading-empty">{format!("Error: {e}")}</div>
                }.into_any(),
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
