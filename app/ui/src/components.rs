use std::sync::Arc;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys::HtmlTextAreaElement;
use mnemis_types::{
    ActionDto, ActionStatus, Confidence, FeedbackKind, MessageDto, PendingResolutionDto,
    SourceHealth, StatusSnapshot, SyncOutcome, summarize_sync_error,
};
use wasm_bindgen::JsCast;

use crate::{
    confidence_class, confirm_resolution, fetch_actions, fetch_pending_resolutions, fetch_status,
    reject_resolution, run_sync_now, status_label, submit_dismissal_feedback, update_action,
};

#[component]
pub fn ActionsPage() -> impl IntoView {
    let sync_tick = use_context::<RwSignal<u32>>().expect("sync tick context");
    let actions = LocalResource::new(move || {
        // Subscribing to sync_tick here makes the resource re-fetch whenever
        // StatusPanel bumps it after a successful sync, so the user doesn't
        // have to navigate away and back to see fresh actions.
        let _ = sync_tick.get();
        async move { fetch_actions(false).await }
    });
    let refetch: Arc<dyn Fn() + Send + Sync> = Arc::new(move || actions.refetch());

    view! {
        <h1>"Actions"</h1>
        <SuggestedResolutions />
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

/// "Suggested resolutions" panel: shows medium/low confidence resolve_action
/// calls the extractor queued for user review. Confirm applies the proposed
/// status; Reject discards the suggestion without changing the action.
#[component]
fn SuggestedResolutions() -> impl IntoView {
    let sync_tick = use_context::<RwSignal<u32>>().expect("sync tick context");
    let suggestions = LocalResource::new(move || {
        let _ = sync_tick.get();
        async move { fetch_pending_resolutions().await }
    });
    let refetch: Arc<dyn Fn() + Send + Sync> = Arc::new(move || suggestions.refetch());

    view! {
        <Suspense fallback=|| view! { <></> }>
            {move || {
                let refetch = refetch.clone();
                suggestions.get().and_then(|res| match res {
                    Ok(rows) if rows.is_empty() => None,
                    Ok(rows) => Some(view! { <SuggestionsList rows=rows refetch=refetch /> }.into_any()),
                    // A failure here shouldn't blow the whole page; just log
                    // via the dev console (already wired through fetch_*).
                    Err(_) => None,
                })
            }}
        </Suspense>
    }
}

#[component]
fn SuggestionsList(
    rows: Vec<PendingResolutionDto>,
    refetch: Arc<dyn Fn() + Send + Sync>,
) -> impl IntoView {
    view! {
        <div class="suggestions-panel">
            <div class="suggestions-header">{format!("{} suggested resolution(s)", rows.len())}</div>
            <For
                each=move || rows.clone()
                key=|r| r.action_id
                children={
                    let refetch = refetch.clone();
                    move |r: PendingResolutionDto| view! {
                        <SuggestionRow row=r refetch=refetch.clone() />
                    }
                }
            />
        </div>
    }
}

#[component]
fn SuggestionRow(row: PendingResolutionDto, refetch: Arc<dyn Fn() + Send + Sync>) -> impl IntoView {
    let action_id = row.action_id;
    // Translate the wire string to the typed enum the Tauri command expects.
    let suggested_status = match row.suggested_status.as_str() {
        "cancelled" => ActionStatus::Cancelled,
        _ => ActionStatus::Done,
    };
    let suggested_label = match suggested_status {
        ActionStatus::Cancelled => "cancelled",
        _ => "done",
    };
    let conf_class = confidence_class(row.confidence);
    let conf_label = match row.confidence {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    };
    let rationale = row.rationale.unwrap_or_default();

    let on_confirm = {
        let refetch = refetch.clone();
        move |_| {
            let refetch = refetch.clone();
            spawn_local(async move {
                let _ = confirm_resolution(action_id, suggested_status).await;
                refetch();
            });
        }
    };
    let on_reject = {
        let refetch = refetch.clone();
        move |_| {
            let refetch = refetch.clone();
            spawn_local(async move {
                let _ = reject_resolution(action_id).await;
                refetch();
            });
        }
    };

    view! {
        <div class="suggestion" data-action-id=action_id.to_string()>
            <div class="suggestion-head">
                <span class="suggestion-title">{row.action_title.clone()}</span>
                <span class=conf_class>{conf_label}</span>
                <span class="badge">{format!("→ {suggested_label}")}</span>
            </div>
            {(!rationale.is_empty()).then(|| view! {
                <div class="suggestion-rationale">{rationale}</div>
            })}
            <div class="suggestion-actions">
                <button class="btn btn-primary" on:click=on_confirm>"Confirm"</button>
                <button class="btn btn-ghost" on:click=on_reject>"Reject"</button>
            </div>
        </div>
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

    // None = no modal open. Some(kind) = feedback modal open with that kind.
    // The status change has already been applied by the time the modal opens;
    // we hold off refetching until the user picks Submit or Skip so the card
    // stays on screen while they decide.
    let feedback_open: RwSignal<Option<FeedbackKind>> = RwSignal::new(None);

    let refetch_for_status = refetch.clone();
    let trigger_status = move |target: ActionStatus, then_open: Option<FeedbackKind>| {
        let refetch = refetch_for_status.clone();
        spawn_local(async move {
            let _ = update_action(id, target, None).await;
            match then_open {
                Some(kind) => feedback_open.set(Some(kind)),
                None => refetch(),
            }
        });
    };

    let trigger_claim = trigger_status.clone();
    let trigger_done = trigger_status.clone();
    let trigger_dismiss = trigger_status.clone();
    let trigger_unclaim = trigger_status.clone();
    let trigger_reopen = trigger_status.clone();

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
                // Undo affordance for an auto-claim. Sends the action back to
                // pending and opens the feedback modal pre-tagged as a wrong
                // auto-claim — but the user can Skip if there's nothing to
                // teach (e.g. they handled it out of band).
                {matches!(current_status, ActionStatus::AutoClaimed).then(|| view! {
                    <button class="btn btn-ghost" on:click=move |_| trigger_unclaim(ActionStatus::Pending, Some(FeedbackKind::WrongAutoClaim))>"Undo"</button>
                })}
                {show_button(current_status, ActionStatus::Done).then(|| view! {
                    <button class="btn btn-primary" on:click=move |_| trigger_done(ActionStatus::Done, None)>"Done"</button>
                })}
                {show_button(current_status, ActionStatus::Dismissed).then(|| view! {
                    <button class="btn btn-ghost" on:click=move |_| trigger_dismiss(ActionStatus::Dismissed, Some(FeedbackKind::Dismissed))>"Dismiss"</button>
                })}
                {show_button(current_status, ActionStatus::Pending).then(|| view! {
                    <button class="btn btn-ghost" on:click=move |_| trigger_reopen(ActionStatus::Pending, None)>"Reopen"</button>
                })}
            </div>
            <Show when=move || feedback_open.get().is_some() fallback=|| view! { <></> }>
                {
                    let refetch = refetch.clone();
                    view! {
                        <FeedbackModal
                            action_id=id
                            kind=feedback_open.get().expect("guarded by Show")
                            on_close=Arc::new({
                                let refetch = refetch.clone();
                                move || {
                                    feedback_open.set(None);
                                    refetch();
                                }
                            })
                        />
                    }
                }
            </Show>
        </div>
    }
}

/// Optional-comment feedback dialog. Used for both "I'm dismissing this" and
/// "the auto-claim was wrong". Skip closes without writing a row — important
/// for the auto-claim case where the user often took action out of band and
/// there's genuinely nothing the model could have learned.
#[component]
fn FeedbackModal(
    action_id: i64,
    kind: FeedbackKind,
    on_close: Arc<dyn Fn() + Send + Sync>,
) -> impl IntoView {
    let textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let prompt = match kind {
        FeedbackKind::Dismissed => "Why isn't this an action item? (optional)",
        FeedbackKind::WrongAutoClaim => {
            "What made this a wrong auto-claim? Leave blank if there's nothing to teach."
        }
    };

    let on_close_submit = on_close.clone();
    let on_submit = move |_| {
        let comment = textarea_ref.get().and_then(|el| {
            let v = el.unchecked_ref::<HtmlTextAreaElement>().value();
            let trimmed = v.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        });
        let close = on_close_submit.clone();
        spawn_local(async move {
            let _ = submit_dismissal_feedback(action_id, kind, comment).await;
            close();
        });
    };

    let on_close_skip = on_close.clone();
    let on_skip = move |_| on_close_skip();

    view! {
        <div class="feedback-modal" data-feedback-kind=kind.as_str()>
            <div class="feedback-prompt">{prompt}</div>
            <textarea class="feedback-input" node_ref=textarea_ref placeholder="Comment (optional)" />
            <div class="feedback-actions">
                <button class="btn btn-ghost" on:click=on_skip>"Skip"</button>
                <button class="btn btn-primary" on:click=on_submit>"Submit feedback"</button>
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
    let sync_tick = use_context::<RwSignal<u32>>().expect("sync tick context");
    let status = LocalResource::new(move || {
        // Status panel also reacts to sync_tick so source health stays fresh.
        let _ = sync_tick.get();
        async move { fetch_status().await }
    });
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
            let succeeded = result.is_ok();
            last_outcome.set(Some(result));
            syncing.set(false);
            if succeeded {
                // Bump the tick so resources subscribed to it (Actions,
                // Inbox, Status itself) all refetch on their own. Without
                // this the user has to navigate away and back to see new
                // data.
                sync_tick.update(|v| *v += 1);
            } else {
                status.refetch();
            }
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
                                {o.errors.iter().map(|e| {
                                    let short = summarize_sync_error(e);
                                    view! {
                                        // title= keeps the full raw chain available on hover
                                        // for the rare case the short summary is misleading.
                                        <li title=e.clone()>{short}</li>
                                    }
                                }).collect_view()}
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
