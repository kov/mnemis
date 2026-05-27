use leptos::prelude::*;
use mnemis_types::{ActionDto, ActionStatus, Confidence};
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

async fn fetch_actions(include_resolved: bool) -> Result<Vec<ActionDto>, String> {
    let args = serde_wasm_bindgen::to_value(&ListActionsArgs { include_resolved })
        .map_err(|e| e.to_string())?;
    let raw = invoke("list_actions", args)
        .await
        .map_err(|e| format!("invoke failed: {:?}", e))?;
    serde_wasm_bindgen::from_value::<Vec<ActionDto>>(raw).map_err(|e| e.to_string())
}

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let actions = LocalResource::new(|| async move { fetch_actions(false).await });
    view! {
        <div class="app">
            <h1>"Actions"</h1>
            <Suspense fallback=|| view! { <div class="loading">"Loading…"</div> }>
                {move || actions.get().map(|res| match res {
                    Ok(rows) => view! { <components::ActionsList rows=rows /> }.into_any(),
                    Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
                })}
            </Suspense>
        </div>
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
