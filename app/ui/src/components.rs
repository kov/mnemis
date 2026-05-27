use std::sync::Arc;

use leptos::prelude::*;
use leptos::task::spawn_local;
use mnemis_types::{
    ActionDto, ActionStatus, Confidence, MessageDto, SourceHealth, StatusSnapshot, SyncOutcome,
};

use crate::{confidence_class, fetch_actions, fetch_status, run_sync_now, status_label, update_action};

#[component]
pub fn ActionsPage() -> impl IntoView {
    let actions = LocalResource::new(|| async move { fetch_actions(false).await });
    let refetch: Arc<dyn Fn() + Send + Sync> = Arc::new(move || actions.refetch());

    view! {
        <h1>"Actions"</h1>
        <Suspense fallback=|| view! { <div class="loading">"Loading…"</div> }>
            {move || {
                let refetch = refetch.clone();
                actions.get().map(move |res| match res {
                    Ok(rows) => view! { <ActionsList rows=rows refetch=refetch /> }.into_any(),
                    Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
                })
            }}
        </Suspense>
    }
}

#[component]
fn ActionsList(rows: Vec<ActionDto>, refetch: Arc<dyn Fn() + Send + Sync>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty">"No active actions."</div> }.into_any();
    }

    let (low, rest): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|a| matches!(a.confidence, Confidence::Low));

    let show_low = RwSignal::new(false);
    let low_count = low.len();
    let low_for_view = StoredValue::new(low);
    let refetch_for_rest = refetch.clone();
    let refetch_for_low = refetch.clone();

    view! {
        <div>
            <For
                each=move || rest.clone()
                key=|a| a.id
                children=move |a: ActionDto| view! {
                    <ActionCard action=a refetch=refetch_for_rest.clone() />
                }
            />

            {move || (low_count > 0).then(|| {
                let refetch_for_low = refetch_for_low.clone();
                view! {
                    <div
                        class="revealer"
                        on:click=move |_| show_low.update(|v| *v = !*v)
                    >
                        {move || if show_low.get() {
                            format!("▾ hide {} low-confidence", low_count)
                        } else {
                            format!("▸ show {} low-confidence", low_count)
                        }}
                    </div>
                    <Show when=move || show_low.get() fallback=|| view! { <></> }>
                        <For
                            each=move || low_for_view.get_value()
                            key=|a| a.id
                            children={
                                let refetch_for_low = refetch_for_low.clone();
                                move |a: ActionDto| view! {
                                    <ActionCard action=a refetch=refetch_for_low.clone() />
                                }
                            }
                        />
                    </Show>
                }
            })}
        </div>
    }
    .into_any()
}

#[component]
fn ActionCard(action: ActionDto, refetch: Arc<dyn Fn() + Send + Sync>) -> impl IntoView {
    let conf_class = confidence_class(action.confidence);
    let conf_label = match action.confidence {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    };
    let status = status_label(action.status);
    let evidence = action.evidence_count;
    let source = action
        .source_name
        .clone()
        .unwrap_or_else(|| "?".to_string());
    let channel = action
        .channel_name
        .clone()
        .unwrap_or_else(|| "?".to_string());
    let id = action.id;
    let current_status = action.status;

    let trigger = move |target: ActionStatus, reason: Option<String>| {
        let refetch = refetch.clone();
        spawn_local(async move {
            let _ = update_action(id, target, reason).await;
            refetch();
        });
    };

    let trigger_claim = trigger.clone();
    let trigger_done = trigger.clone();
    let trigger_dismiss = trigger.clone();
    let trigger_reopen = trigger.clone();

    view! {
        <div class="action">
            <div class="action-head">
                <span class="action-title">{action.title.clone()}</span>
                <span class=conf_class>{conf_label}</span>
                <span class="badge">{status}</span>
            </div>
            {action.details.clone().map(|d| view! {
                <div class="action-details">{d}</div>
            })}
            <div class="action-meta">
                {format!("{source} · {channel} · {evidence} evidence")}
            </div>
            <div class="action-actions">
                {show_button(current_status, ActionStatus::Claimed).then(|| view! {
                    <button class="btn btn-secondary" on:click=move |_| trigger_claim(ActionStatus::Claimed, None)>"Claim"</button>
                })}
                {show_button(current_status, ActionStatus::Done).then(|| view! {
                    <button class="btn btn-primary" on:click=move |_| trigger_done(ActionStatus::Done, None)>"Done"</button>
                })}
                {show_button(current_status, ActionStatus::Dismissed).then(|| view! {
                    <button class="btn btn-ghost" on:click=move |_| trigger_dismiss(ActionStatus::Dismissed, Some(String::new()))>"Dismiss"</button>
                })}
                {show_button(current_status, ActionStatus::Pending).then(|| view! {
                    <button class="btn btn-ghost" on:click=move |_| trigger_reopen(ActionStatus::Pending, None)>"Reopen"</button>
                })}
            </div>
        </div>
    }
}

/// Which buttons make sense from the current status. Done/Dismissed get a
/// Reopen affordance; active actions get Done/Dismiss; only fresh pending
/// items get an explicit Claim (auto_claimed is already claimed-by-agent).
fn show_button(current: ActionStatus, target: ActionStatus) -> bool {
    use ActionStatus::*;
    match (current, target) {
        (Pending, Claimed) => true,
        (Pending | AutoClaimed | Claimed, Done) => true,
        (Pending | AutoClaimed | Claimed, Dismissed) => true,
        (Done | Cancelled | Dismissed, Pending) => true,
        _ => false,
    }
}

#[component]
pub fn StatusPanel() -> impl IntoView {
    let status = LocalResource::new(|| async move { fetch_status().await });
    let syncing = RwSignal::new(false);
    let last_outcome: RwSignal<Option<Result<SyncOutcome, String>>> = RwSignal::new(None);

    let on_click = move |_| {
        if syncing.get() {
            return;
        }
        syncing.set(true);
        last_outcome.set(None);
        spawn_local(async move {
            let result = run_sync_now().await;
            last_outcome.set(Some(result));
            syncing.set(false);
            status.refetch();
        });
    };

    view! {
        <div class="status-panel">
            <div class="status-row">
                <Suspense fallback=|| view! { <span class="status-loading">"…"</span> }>
                    {move || status.get().map(|res| match res {
                        Ok(s) => view! { <StatusPanelInner snap=s /> }.into_any(),
                        Err(e) => view! { <span class="status-error">{format!("status error: {e}")}</span> }.into_any(),
                    })}
                </Suspense>
                <button
                    class="sync-button"
                    on:click=on_click
                    disabled=move || syncing.get()
                >
                    {move || if syncing.get() { "Syncing…" } else { "Sync now" }}
                </button>
            </div>
            {move || last_outcome.get().map(|res| match res {
                Ok(o) => view! {
                    <div class="status-toast status-toast-ok">
                        {format!(
                            "Synced {} source(s) · {} new message(s) · {} action(s)",
                            o.sources_synced, o.messages_ingested, o.actions_created
                        )}
                        {(!o.errors.is_empty()).then(|| view! {
                            <ul class="status-errors">
                                {o.errors.iter().map(|e| view! { <li>{e.clone()}</li> }).collect_view()}
                            </ul>
                        })}
                    </div>
                }.into_any(),
                Err(e) => view! {
                    <div class="status-toast status-toast-error">{format!("Sync failed: {e}")}</div>
                }.into_any(),
            })}
        </div>
    }
}

#[component]
fn StatusPanelInner(snap: StatusSnapshot) -> impl IntoView {
    let queue = snap.embed_queue_depth;
    let last_extraction = snap
        .last_extraction_at
        .map(format_relative)
        .map(|t| format!("last extract {t} ago"))
        .unwrap_or_else(|| "never extracted".to_string());

    view! {
        <div class="status-sources">
            {if snap.sources.is_empty() {
                view! { <span class="status-empty">"no sources configured"</span> }.into_any()
            } else {
                view! {
                    <For
                        each=move || snap.sources.clone()
                        key=|s| s.id
                        children=move |s| {
                            let cls = health_class(s.health);
                            let label = health_label(s.health);
                            let when = s.last_synced_at
                                .map(format_relative)
                                .map(|t| format!(" ({t} ago)"))
                                .unwrap_or_default();
                            view! {
                                <span class=format!("status-source {cls}") title=s.last_error.clone().unwrap_or_default()>
                                    {format!("{}: {}{}", s.name, label, when)}
                                </span>
                            }
                        }
                    />
                }.into_any()
            }}
        </div>
        <div class="status-meta">
            <span class="status-queue">{format!("embed queue: {queue}")}</span>
            <span class="status-extraction">{last_extraction}</span>
        </div>
    }
}

fn health_class(h: SourceHealth) -> &'static str {
    match h {
        SourceHealth::Ok => "status-ok",
        SourceHealth::Warning => "status-warning",
        SourceHealth::Failed => "status-failed",
        SourceHealth::Disabled => "status-disabled",
    }
}

fn health_label(h: SourceHealth) -> &'static str {
    match h {
        SourceHealth::Ok => "ok",
        SourceHealth::Warning => "warning",
        SourceHealth::Failed => "failed",
        SourceHealth::Disabled => "disabled",
    }
}

/// Coarse relative-time label. Frontend-only so we don't pull chrono into wasm
/// just for "2h ago"; the precision needed here is low.
fn format_relative(posted_at: i64) -> String {
    let now = (js_sys::Date::now() / 1000.0) as i64;
    let diff = (now - posted_at).max(0);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86_400 {
        format!("{}h", diff / 3600)
    } else if diff < 86_400 * 7 {
        format!("{}d", diff / 86_400)
    } else {
        format!("{}w", diff / (86_400 * 7))
    }
}

#[component]
pub fn InboxList(rows: Vec<MessageDto>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty">"No messages yet."</div> }.into_any();
    }
    view! {
        <div>
            <For
                each=move || rows.clone()
                key=|m| m.id
                children=move |m: MessageDto| view! { <MessageRow msg=m /> }
            />
        </div>
    }
    .into_any()
}

#[component]
fn MessageRow(msg: MessageDto) -> impl IntoView {
    let subject = msg
        .subject
        .clone()
        .unwrap_or_else(|| "(no subject)".to_string());
    let author = msg
        .author_display
        .clone()
        .unwrap_or_else(|| "?".to_string());
    let source = msg.source_name.clone().unwrap_or_else(|| "?".to_string());
    let channel = msg.channel_name.clone().unwrap_or_else(|| "?".to_string());
    let when = format_relative(msg.posted_at);

    view! {
        <div class="message">
            <div class="message-head">
                <span class="message-author">{author}</span>
                <span class="message-subject">{subject}</span>
                {msg.has_action.then(|| view! { <span class="badge badge-high">"action"</span> })}
                <span class="message-when">{when}</span>
            </div>
            <div class="message-snippet">{msg.snippet.clone()}</div>
            <div class="message-meta">{format!("{source} · {channel}")}</div>
        </div>
    }
}
