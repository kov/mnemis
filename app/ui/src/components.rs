use std::sync::Arc;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys::{HtmlInputElement, HtmlSelectElement, HtmlTextAreaElement};
use leptos_router::components::A;
use mnemis_types::{
    ActionDto, ActionStatus, ChannelRowDto, Confidence, FeedbackKind, LlmConfigDto, MessageDto,
    PendingResolutionDto, ProfileIdentifier, SourceHealth, SourceRowDto, StatusSnapshot,
    SyncOutcome, UserProfileDto, summarize_sync_error,
};
use wasm_bindgen::JsCast;

use crate::{
    FirstRunTick, SyncTick, add_imap_source, confidence_class, confirm_resolution, delete_source,
    fetch_actions, fetch_is_first_run, fetch_llm_config, fetch_pending_resolutions,
    fetch_settings_sources, fetch_source_channels, fetch_status, fetch_user_profile,
    reject_resolution, run_sync_now, save_llm_config, save_user_profile, set_channel_muted,
    set_channels_muted_bulk, set_source_muted, status_label, submit_dismissal_feedback,
    update_action,
};

#[component]
pub fn ActionsPage() -> impl IntoView {
    let SyncTick(sync_tick) = use_context::<SyncTick>().expect("sync tick context");
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
    let SyncTick(sync_tick) = use_context::<SyncTick>().expect("sync tick context");
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
    let SyncTick(sync_tick) = use_context::<SyncTick>().expect("sync tick context");
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
                Ok(o) => {
                    // A sync that returns Ok can still carry per-channel
                    // errors (partial success). Render those amber, not
                    // green, so they don't read as "all clear". Fully-clean
                    // syncs stay green; total failure is the Err branch.
                    let had_errors = !o.errors.is_empty();
                    let toast_class = if had_errors {
                        "status-toast status-toast-warning"
                    } else {
                        "status-toast status-toast-ok"
                    };
                    let headline = format!(
                        "Synced {} source(s) · {} new message(s) · {} action(s){}",
                        o.sources_synced,
                        o.messages_ingested,
                        o.actions_created,
                        if had_errors {
                            format!(" · {} error(s)", o.errors.len())
                        } else {
                            String::new()
                        },
                    );
                    view! {
                    <div class=toast_class>
                        {headline}
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
                }.into_any()},
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

/// First-run banner: shown above everything when no `user_profile` row has
/// been saved. Disappears the instant the profile is saved (the resource is
/// keyed off sync_tick which the profile editor bumps on save).
#[component]
pub fn FirstRunBanner() -> impl IntoView {
    // The banner subscribes to its own tick — bumped from the profile-save
    // handler — so it re-checks `is_first_run` *without* remounting the
    // profile form (which would discard the "Saved." toast mid-render).
    let FirstRunTick(first_run_tick) =
        use_context::<FirstRunTick>().expect("first run tick context");
    let first_run = LocalResource::new(move || {
        let _ = first_run_tick.get();
        async move { fetch_is_first_run().await }
    });
    view! {
        <Suspense fallback=|| view! { <></> }>
            {move || first_run.get().and_then(|res| match res {
                Ok(true) => Some(view! {
                    <div class="first-run-banner">
                        <span>"Welcome to mnemis. "</span>
                        <A href="/settings/profile">"Set up your profile"</A>
                        <span>" to start ingesting."</span>
                    </div>
                }.into_any()),
                _ => None,
            })}
        </Suspense>
    }
}

/// Settings landing page — links to the sub-sections. Keeps the URL stable
/// so the first-run banner can deep-link to a specific sub-page.
#[component]
pub fn SettingsHome() -> impl IntoView {
    view! {
        <h1>"Settings"</h1>
        <ul class="settings-home">
            <li><A href="/settings/profile">"Profile"</A>
                <span class="settings-desc">" — display name + identifiers + extraction context"</span></li>
            <li><A href="/settings/llm">"LLM"</A>
                <span class="settings-desc">" — omlx endpoint + models"</span></li>
            <li><A href="/settings/sources">"Sources"</A>
                <span class="settings-desc">" — IMAP + chat connectors"</span></li>
        </ul>
    }
}

/// Profile editor: display_name, custom_prompt, identifiers (kind+value rows
/// the user can add/remove). Save writes through to `user_profile` +
/// reconciles `contact_identifiers` for the self-contact.
#[component]
pub fn SettingsProfile() -> impl IntoView {
    // Load once on mount; don't subscribe to sync_tick — a refetch would
    // remount the inner ProfileForm and wipe the in-flight "Saved." toast.
    let profile = LocalResource::new(|| async move { fetch_user_profile().await });
    view! {
        <h1>"Profile"</h1>
        <Suspense fallback=|| view! { <div class="loading">"Loading…"</div> }>
            {move || profile.get().map(|res| match res {
                Ok(p) => view! { <ProfileForm initial=p /> }.into_any(),
                Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn ProfileForm(initial: UserProfileDto) -> impl IntoView {
    let display_name = RwSignal::new(initial.display_name.clone());
    let custom_prompt = RwSignal::new(initial.custom_prompt.clone().unwrap_or_default());
    let identifiers: RwSignal<Vec<ProfileIdentifier>> = RwSignal::new(initial.identifiers.clone());
    let toast: RwSignal<Option<String>> = RwSignal::new(None);
    let FirstRunTick(first_run_tick) =
        use_context::<FirstRunTick>().expect("first run tick context");

    let name_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let prompt_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let new_kind_ref: NodeRef<leptos::html::Select> = NodeRef::new();
    let new_value_ref: NodeRef<leptos::html::Input> = NodeRef::new();

    let on_add_identifier = move |_| {
        let kind = new_kind_ref
            .get()
            .map(|el| el.unchecked_ref::<HtmlSelectElement>().value())
            .unwrap_or_else(|| "email".to_string());
        let value = new_value_ref
            .get()
            .map(|el| {
                el.unchecked_ref::<HtmlInputElement>()
                    .value()
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        if value.is_empty() {
            return;
        }
        identifiers.update(|v| {
            if !v.iter().any(|i| i.kind == kind && i.value == value) {
                v.push(ProfileIdentifier {
                    kind: kind.clone(),
                    value: value.clone(),
                });
            }
        });
        if let Some(el) = new_value_ref.get() {
            el.unchecked_ref::<HtmlInputElement>().set_value("");
        }
    };

    let on_save = move |_| {
        let name = name_ref
            .get()
            .map(|el| {
                el.unchecked_ref::<HtmlInputElement>()
                    .value()
                    .trim()
                    .to_string()
            })
            .unwrap_or_else(|| display_name.get());
        let prompt = prompt_ref
            .get()
            .map(|el| el.unchecked_ref::<HtmlTextAreaElement>().value())
            .unwrap_or_else(|| custom_prompt.get());
        let prompt = prompt.trim().to_string();
        let p = UserProfileDto {
            display_name: name,
            custom_prompt: if prompt.is_empty() {
                None
            } else {
                Some(prompt)
            },
            identifiers: identifiers.get(),
        };
        spawn_local(async move {
            match save_user_profile(p).await {
                Ok(_) => {
                    toast.set(Some("Saved.".to_string()));
                    // Bump the dedicated first-run tick — NOT sync_tick —
                    // so the banner re-checks `is_first_run` without
                    // remounting this form (which would wipe the toast).
                    first_run_tick.update(|v| *v += 1);
                }
                Err(e) => toast.set(Some(format!("Save failed: {e}"))),
            }
        });
    };

    view! {
        <div class="settings-form" data-form="profile">
            <label>"Display name"</label>
            <input
                class="settings-input"
                node_ref=name_ref
                prop:value=move || display_name.get()
            />

            <label>"Custom prompt (optional)"</label>
            <textarea
                class="settings-textarea"
                node_ref=prompt_ref
                rows="6"
                placeholder="Anything the extractor should know about you and your priorities."
                prop:value=move || custom_prompt.get()
            />

            <label>"Identifiers"</label>
            <div class="identifiers-list">
                <For
                    each=move || identifiers.get()
                    key=|i| format!("{}:{}", i.kind, i.value)
                    children=move |i: ProfileIdentifier| {
                        let kind = i.kind.clone();
                        let value = i.value.clone();
                        let to_remove = (kind.clone(), value.clone());
                        let on_remove = move |_| {
                            let to_remove = to_remove.clone();
                            identifiers.update(|v| {
                                v.retain(|cur| !(cur.kind == to_remove.0 && cur.value == to_remove.1));
                            });
                        };
                        view! {
                            <div class="identifier-row">
                                <span class="identifier-kind">{kind}</span>
                                <span class="identifier-value">{value}</span>
                                <button class="btn btn-ghost" on:click=on_remove>"Remove"</button>
                            </div>
                        }
                    }
                />
            </div>

            <div class="identifier-add">
                <select class="settings-select" node_ref=new_kind_ref>
                    <option value="email">"email"</option>
                    <option value="mattermost_handle">"mattermost_handle"</option>
                    <option value="discord_id">"discord_id"</option>
                    <option value="phone">"phone"</option>
                </select>
                <input class="settings-input" node_ref=new_value_ref placeholder="value" />
                <button class="btn btn-secondary" on:click=on_add_identifier>"Add"</button>
            </div>

            <div class="settings-actions">
                <button class="btn btn-primary" on:click=on_save>"Save profile"</button>
            </div>
            {move || toast.get().map(|t| view! { <div class="settings-toast">{t}</div> })}
        </div>
    }
}

/// LLM config editor. Writes to `config.toml`; the user is told the change
/// takes effect on next app restart since `LlmStack` is built once at
/// startup. (Deferring hot-reload is fine for now — settings is rare.)
#[component]
pub fn SettingsLlm() -> impl IntoView {
    let cfg = LocalResource::new(|| async move { fetch_llm_config().await });
    view! {
        <h1>"LLM"</h1>
        <Suspense fallback=|| view! { <div class="loading">"Loading…"</div> }>
            {move || cfg.get().map(|res| match res {
                Ok(c) => view! { <LlmForm initial=c /> }.into_any(),
                Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn LlmForm(initial: LlmConfigDto) -> impl IntoView {
    let base_url = RwSignal::new(initial.base_url.clone());
    let chat = RwSignal::new(initial.chat_model.clone());
    let embed = RwSignal::new(initial.embedding_model.clone());
    let token = RwSignal::new(initial.bearer_token.clone().unwrap_or_default());
    let config_path = initial.config_path.clone();
    let toast: RwSignal<Option<String>> = RwSignal::new(None);

    let base_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let chat_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let embed_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let token_ref: NodeRef<leptos::html::Input> = NodeRef::new();

    let on_save = move |_| {
        let pull = |r: NodeRef<leptos::html::Input>, default: String| -> String {
            r.get()
                .map(|el| {
                    el.unchecked_ref::<HtmlInputElement>()
                        .value()
                        .trim()
                        .to_string()
                })
                .unwrap_or(default)
        };
        let cfg = LlmConfigDto {
            base_url: pull(base_ref, base_url.get()),
            chat_model: pull(chat_ref, chat.get()),
            embedding_model: pull(embed_ref, embed.get()),
            bearer_token: {
                let v = pull(token_ref, token.get());
                if v.is_empty() { None } else { Some(v) }
            },
            config_path: String::new(),
        };
        spawn_local(async move {
            match save_llm_config(cfg).await {
                Ok(_) => toast.set(Some("Saved. Restart the app to apply.".to_string())),
                Err(e) => toast.set(Some(format!("Save failed: {e}"))),
            }
        });
    };

    view! {
        <div class="settings-form" data-form="llm">
            <div class="settings-hint">{format!("Writes to {config_path}")}</div>
            <label>"Base URL"</label>
            <input class="settings-input" node_ref=base_ref prop:value=move || base_url.get() />
            <label>"Chat model"</label>
            <input class="settings-input" node_ref=chat_ref prop:value=move || chat.get() />
            <label>"Embedding model"</label>
            <input class="settings-input" node_ref=embed_ref prop:value=move || embed.get() />
            <label>"Bearer token (optional)"</label>
            <input class="settings-input" node_ref=token_ref prop:value=move || token.get() />
            <div class="settings-actions">
                <button class="btn btn-primary" on:click=on_save>"Save LLM config"</button>
            </div>
            {move || toast.get().map(|t| view! { <div class="settings-toast">{t}</div> })}
        </div>
    }
}

/// Sources list with per-source mute + delete + an Add IMAP modal.
#[component]
pub fn SettingsSources() -> impl IntoView {
    let SyncTick(sync_tick) = use_context::<SyncTick>().expect("sync tick context");
    let sources = LocalResource::new(move || {
        let _ = sync_tick.get();
        async move { fetch_settings_sources().await }
    });
    let refetch: Arc<dyn Fn() + Send + Sync> = Arc::new(move || sources.refetch());
    let refetch_for_table = refetch.clone();
    let refetch_for_modal = refetch.clone();
    let add_open = RwSignal::new(false);
    let on_add = move |_| add_open.set(true);
    view! {
        <h1>"Sources"</h1>
        <div class="settings-actions">
            <button class="btn btn-primary" on:click=on_add>"Add IMAP source"</button>
        </div>
        <Suspense fallback=|| view! { <div class="loading">"Loading…"</div> }>
            {move || {
                let refetch = refetch_for_table.clone();
                sources.get().map(move |res| match res {
                    Ok(rows) if rows.is_empty() => view! {
                        <div class="empty">
                            "No sources configured yet."
                        </div>
                    }.into_any(),
                    Ok(rows) => view! { <SourcesTable rows=rows refetch=refetch /> }.into_any(),
                    Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
                })
            }}
        </Suspense>
        <Show when=move || add_open.get() fallback=|| view! { <></> }>
            {
                let refetch = refetch_for_modal.clone();
                view! {
                    <AddImapModal
                        on_close=Arc::new(move |added| {
                            add_open.set(false);
                            if added {
                                refetch();
                            }
                        })
                    />
                }
            }
        </Show>
    }
}

/// IMAP add modal. Pure form — no validation beyond "name/server/username
/// non-empty". The Tauri command writes to the DB + keychain; channel
/// discovery happens on the next sync.
#[component]
fn AddImapModal(on_close: Arc<dyn Fn(bool) + Send + Sync>) -> impl IntoView {
    let toast: RwSignal<Option<String>> = RwSignal::new(None);
    let name_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let server_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let port_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let user_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let pass_ref: NodeRef<leptos::html::Input> = NodeRef::new();

    let on_close_cancel = on_close.clone();
    let on_cancel = move |_| on_close_cancel(false);

    let on_close_save = on_close.clone();
    let on_save = move |_| {
        let pull = |r: NodeRef<leptos::html::Input>| -> String {
            r.get()
                .map(|el| {
                    el.unchecked_ref::<HtmlInputElement>()
                        .value()
                        .trim()
                        .to_string()
                })
                .unwrap_or_default()
        };
        let name = pull(name_ref);
        let server = pull(server_ref);
        let port_str = pull(port_ref);
        let username = pull(user_ref);
        let password = pass_ref
            .get()
            .map(|el| el.unchecked_ref::<HtmlInputElement>().value())
            .unwrap_or_default();
        if name.is_empty() || server.is_empty() || username.is_empty() {
            toast.set(Some("Name, server, and username are required.".to_string()));
            return;
        }
        let port: u16 = port_str.parse().unwrap_or(993);
        let on_close = on_close_save.clone();
        spawn_local(async move {
            match add_imap_source(name, server, port, username, password).await {
                Ok(_) => on_close(true),
                Err(e) => toast.set(Some(format!("Add failed: {e}"))),
            }
        });
    };

    view! {
        <div class="add-source-modal" data-source-kind="imap">
            <div class="modal-title">"Add IMAP source"</div>
            <label>"Name (display)"</label>
            <input class="settings-input" node_ref=name_ref placeholder="work" />
            <label>"Server"</label>
            <input class="settings-input" node_ref=server_ref placeholder="imap.example.com" />
            <label>"Port"</label>
            <input class="settings-input" node_ref=port_ref placeholder="993" />
            <label>"Username"</label>
            <input class="settings-input" node_ref=user_ref placeholder="you@example.com" />
            <label>"Password"</label>
            <input class="settings-input" node_ref=pass_ref type="password" />
            <div class="settings-actions">
                <button class="btn btn-ghost" on:click=on_cancel>"Cancel"</button>
                <button class="btn btn-primary" on:click=on_save>"Add source"</button>
            </div>
            {move || toast.get().map(|t| view! { <div class="settings-toast">{t}</div> })}
        </div>
    }
}

#[component]
fn SourcesTable(rows: Vec<SourceRowDto>, refetch: Arc<dyn Fn() + Send + Sync>) -> impl IntoView {
    view! {
        <table class="sources-table">
            <thead>
                <tr>
                    <th></th>
                    <th>"Name"</th>
                    <th>"Kind"</th>
                    <th>"Health"</th>
                    <th>"Muted"</th>
                    <th></th>
                </tr>
            </thead>
            <tbody>
                <For
                    each=move || rows.clone()
                    key=|s| s.id
                    children={
                        let refetch = refetch.clone();
                        move |s: SourceRowDto| view! {
                            <SourceRowView row=s refetch=refetch.clone() />
                        }
                    }
                />
            </tbody>
        </table>
    }
}

#[component]
fn SourceRowView(row: SourceRowDto, refetch: Arc<dyn Fn() + Send + Sync>) -> impl IntoView {
    let id = row.id;
    let health = row.health;
    let muted = RwSignal::new(row.muted);
    let expanded = RwSignal::new(false);

    let on_toggle = {
        let refetch = refetch.clone();
        move |_| {
            let refetch = refetch.clone();
            let next = !muted.get();
            spawn_local(async move {
                if set_source_muted(id, next).await.is_ok() {
                    muted.set(next);
                }
                refetch();
            });
        }
    };
    let on_delete = {
        let refetch = refetch.clone();
        move |_| {
            let refetch = refetch.clone();
            spawn_local(async move {
                let _ = delete_source(id).await;
                refetch();
            });
        }
    };
    let on_expand = move |_| expanded.update(|v| *v = !*v);

    view! {
        <tr class="source-row" data-source-id=id.to_string()>
            <td>
                <button class="btn btn-ghost source-expand" on:click=on_expand>
                    {move || if expanded.get() { "▾" } else { "▸" }}
                </button>
            </td>
            <td>{row.name.clone()}</td>
            <td>{row.kind.clone()}</td>
            <td>
                <span class=format!("badge {}", health_class(health))>
                    {health_label(health)}
                </span>
            </td>
            <td>
                <button class="btn btn-ghost" on:click=on_toggle>
                    {move || if muted.get() { "Unmute" } else { "Mute" }}
                </button>
            </td>
            <td>
                <button class="btn btn-ghost" on:click=on_delete>"Delete"</button>
            </td>
        </tr>
        <Show when=move || expanded.get() fallback=|| view! { <tr style="display:none"></tr> }>
            <tr class="source-channels-row" data-source-id=id.to_string()>
                <td></td>
                <td colspan="5">
                    <SourceChannels source_id=id />
                </td>
            </tr>
        </Show>
    }
}

#[component]
fn SourceChannels(source_id: i64) -> impl IntoView {
    let channels = LocalResource::new(move || async move {
        fetch_source_channels(source_id).await
    });
    let refetch: Arc<dyn Fn() + Send + Sync> = Arc::new(move || channels.refetch());
    view! {
        <Suspense fallback=|| view! { <div class="loading">"Loading channels…"</div> }>
            {move || {
                let refetch = refetch.clone();
                channels.get().map(move |res| match res {
                    Ok(rows) if rows.is_empty() => view! {
                        <div class="empty">"No channels yet — sync once to discover them."</div>
                    }.into_any(),
                    Ok(rows) => view! {
                        <ChannelsList rows=rows refetch=refetch />
                    }.into_any(),
                    Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
                })
            }}
        </Suspense>
    }
}

/// Tree node for the channel hierarchy. A node may carry a channel of its own
/// (its full path matches an actual mailbox), have children (paths that
/// extend it), or both — IMAP folders like `INBOX/Lembrar` are mailboxes
/// *and* parents of `INBOX/Lembrar/Sub`.
#[derive(Clone)]
struct ChannelNode {
    /// Last path segment, e.g. "Lembrar".
    label: String,
    channel: Option<ChannelRowDto>,
    children: Vec<ChannelNode>,
}

/// Build the tree, splitting on '/'. Sorted at each level for stable order.
fn build_tree(rows: Vec<ChannelRowDto>) -> Vec<ChannelNode> {
    fn insert(siblings: &mut Vec<ChannelNode>, segments: &[&str], row: ChannelRowDto) {
        let segment = segments[0];
        let idx = match siblings.iter().position(|n| n.label == segment) {
            Some(i) => i,
            None => {
                siblings.push(ChannelNode {
                    label: segment.to_string(),
                    channel: None,
                    children: Vec::new(),
                });
                siblings.len() - 1
            }
        };
        if segments.len() == 1 {
            siblings[idx].channel = Some(row);
        } else {
            insert(&mut siblings[idx].children, &segments[1..], row);
        }
    }
    fn sort_in_place(nodes: &mut [ChannelNode]) {
        nodes.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
        for n in nodes {
            sort_in_place(&mut n.children);
        }
    }
    let mut roots: Vec<ChannelNode> = Vec::new();
    for row in rows {
        let segments: Vec<String> = row
            .name
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if segments.is_empty() {
            continue;
        }
        let refs: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();
        insert(&mut roots, &refs, row);
    }
    sort_in_place(&mut roots);
    roots
}

/// Walk the tree collecting every channel id (skipping pure folders).
fn collect_channel_ids(nodes: &[ChannelNode], out: &mut Vec<i64>) {
    for n in nodes {
        if let Some(c) = &n.channel {
            out.push(c.id);
        }
        collect_channel_ids(&n.children, out);
    }
}

#[component]
fn ChannelsList(
    rows: Vec<ChannelRowDto>,
    refetch: Arc<dyn Fn() + Send + Sync>,
) -> impl IntoView {
    let tree = build_tree(rows.clone());
    let all_ids = {
        let mut v = Vec::new();
        collect_channel_ids(&tree, &mut v);
        v
    };
    let total = all_ids.len();

    // Deliberately *don't* bump the source-list refetch on channel writes:
    // remounting the source row from a fresh fetch would collapse the
    // expanded tree mid-toggle. The source-level "Muted" badge can be
    // slightly stale until next nav — acceptable trade-off because the user
    // is now working at channel granularity.
    let bulk = {
        let refetch = refetch.clone();
        move |muted: bool| {
            let refetch = refetch.clone();
            let ids = all_ids.clone();
            spawn_local(async move {
                let _ = set_channels_muted_bulk(ids, muted).await;
                refetch();
            });
        }
    };
    let on_enable_all = {
        let bulk = bulk.clone();
        move |_| bulk(false)
    };
    let on_disable_all = move |_| bulk(true);

    view! {
        <div class="channels-toolbar">
            <button class="btn btn-ghost channels-enable-all" on:click=on_enable_all>"Enable all"</button>
            <button class="btn btn-ghost channels-disable-all" on:click=on_disable_all>"Disable all"</button>
            <span class="channels-count">{format!("{total} channel(s)")}</span>
        </div>
        <ul class="channels-tree channels-list">
            {tree.into_iter().map(|node| {
                render_node(node, refetch.clone())
            }).collect_view()}
        </ul>
    }
}

fn render_node(node: ChannelNode, refetch: Arc<dyn Fn() + Send + Sync>) -> AnyView {
    let label = node.label.clone();
    let children = node.children;
    let row_view = match node.channel {
        Some(channel) => {
            let id = channel.id;
            let kind = channel.kind.clone();
            let count = channel.message_count;
            let muted = RwSignal::new(channel.muted);
            let on_toggle = {
                let refetch = refetch.clone();
                move |_| {
                    let refetch = refetch.clone();
                    let next = !muted.get();
                    spawn_local(async move {
                        if set_channel_muted(id, next).await.is_ok() {
                            muted.set(next);
                        }
                        refetch();
                    });
                }
            };
            view! {
                <li class="channel-row"
                    data-channel-id=id.to_string()
                    data-channel-muted=move || muted.get().to_string()>
                    <input
                        type="checkbox"
                        class="channel-checkbox"
                        prop:checked=move || !muted.get()
                        on:change=on_toggle
                    />
                    <span class="channel-name">{label.clone()}</span>
                    <span class="channel-kind">{format!("({kind})")}</span>
                    <span class="channel-count">{format!("{count} msg")}</span>
                </li>
            }
            .into_any()
        }
        None => view! {
            // A pure folder in the hierarchy with no mailbox of its own.
            // Shouldn't happen in practice (IMAP servers expose folders as
            // selectable mailboxes), but handled so a misbehaving server
            // can't crash the tree.
            <li class="channel-row channel-folder">
                <span class="channel-name">{label.clone()}</span>
            </li>
        }
        .into_any(),
    };

    let children_view = if children.is_empty() {
        ().into_any()
    } else {
        let refetch = refetch.clone();
        view! {
            <ul class="channel-children">
                {children.into_iter().map(|c| {
                    render_node(c, refetch.clone())
                }).collect_view()}
            </ul>
        }
        .into_any()
    };

    view! { <>{row_view}{children_view}</> }.into_any()
}
