use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys::{
    Element, HtmlInputElement, HtmlSelectElement, HtmlTextAreaElement, KeyboardEvent, MouseEvent,
};
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map};
use mnemis_types::{
    ActionDto, ActionStatus, CaldavCollectionDto, ChannelRowDto, ChatDto, ChatEvent, ChatTurnDto,
    Confidence, FeedbackKind, LlmConfigDto, MessageActionRef, MessageDetailDto, MessageDto,
    PendingResolutionDto, ProfileIdentifier, SourceHealth, SourceRowDto, StatusSnapshot,
    SyncOutcome, ThinkingLevel, UserProfileDto, summarize_sync_error,
};
use wasm_bindgen::JsCast;

use crate::markdown::{is_safe_href, markdown_to_html};
use crate::{
    ChatStream, ChatUiState, FirstRunTick, SyncTick, add_caldav_account, add_imap_source,
    cancel_chat_message, confidence_class, confirm_resolution, create_chat, delete_caldav_account,
    delete_chat, delete_source, discover_source_channels, fetch_actions, fetch_caldav_account,
    fetch_chat_seed, fetch_chat_turns, fetch_chats, fetch_is_first_run, fetch_llm_config,
    fetch_message, fetch_pending_resolutions, fetch_settings_sources, fetch_status,
    fetch_user_profile, list_caldav_collections, open_external, promote_to_reminder,
    reject_resolution, run_sync_caldav, run_sync_now, save_llm_config, save_user_profile,
    send_chat_message, set_caldav_collection, set_channel_muted, set_channels_muted_bulk,
    set_chat_archived, set_chat_show_reasoning, set_source_muted, status_label,
    submit_dismissal_feedback, update_action,
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

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `(year, month 1-12, day)` in UTC for a unix-seconds timestamp.
fn ymd_of_unix(secs: i64) -> (i32, u32, u32) {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(secs as f64 * 1000.0));
    (
        d.get_utc_full_year() as i32,
        d.get_utc_month() + 1,
        d.get_utc_date(),
    )
}

/// Today's `(year, month 1-12, day)` in UTC — the calendar's default month.
fn today_ymd() -> (i32, u32, u32) {
    let d = js_sys::Date::new_0();
    (
        d.get_utc_full_year() as i32,
        d.get_utc_month() + 1,
        d.get_utc_date(),
    )
}

/// A short human date like "Jun 1" for the reminder badge.
fn unix_to_short_date(secs: i64) -> String {
    let (_, month, day) = ymd_of_unix(secs);
    let name = MONTH_NAMES
        .get((month as usize).wrapping_sub(1))
        .copied()
        .unwrap_or("");
    format!("{name} {day}")
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 30,
    }
}

/// Day of week (0 = Sunday … 6 = Saturday) via Sakamoto's algorithm — pure, so
/// no timezone surprises from `js_sys::Date`.
fn weekday(year: i32, month: u32, day: u32) -> u32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    let idx = (month as usize).wrapping_sub(1).min(11);
    let w = (y + y / 4 - y / 100 + y / 400 + T[idx] + day as i32) % 7;
    (((w % 7) + 7) % 7) as u32
}

/// A self-contained calendar popover for (re)setting an action's reminder date.
/// Replaces the native `<input type=date>`, whose picker under webkit2gtk
/// segments the field on click and won't dismiss on day-click or outside-click.
/// Here a day-click commits and closes; clicking the backdrop (anywhere else)
/// closes without changing anything. Prev/next move the visible month.
#[component]
fn ReminderDatePicker(
    action_id: i64,
    due_at: Option<i64>,
    label: &'static str,
    refetch: Arc<dyn Fn() + Send + Sync>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    // The visible month starts on the current due date (so editing lands you on
    // the set date) or today for a fresh reminder.
    let (start_y, start_m, _) = due_at.map(ymd_of_unix).unwrap_or_else(today_ymd);
    let view_year = RwSignal::new(start_y);
    let view_month = RwSignal::new(start_m); // 1-12
    // The set day, for highlighting (only when there is a due date).
    let selected = due_at.map(ymd_of_unix);

    let prev = move |_| {
        let (y, m) = (view_year.get(), view_month.get());
        if m == 1 {
            view_year.set(y - 1);
            view_month.set(12);
        } else {
            view_month.set(m - 1);
        }
    };
    let next = move |_| {
        let (y, m) = (view_year.get(), view_month.get());
        if m == 12 {
            view_year.set(y + 1);
            view_month.set(1);
        } else {
            view_month.set(m + 1);
        }
    };

    let popover = move || {
        if !open.get() {
            return ().into_any();
        }
        // Rebuilt whenever the visible month changes; each day commits on click.
        let grid = {
            let refetch = refetch.clone();
            move || {
                let (y, m) = (view_year.get(), view_month.get());
                let lead = weekday(y, m, 1) as usize;
                let count = days_in_month(y, m);
                let mut cells: Vec<_> = (0..lead)
                    .map(|_| view! { <span class="cal-cell cal-blank"></span> }.into_any())
                    .collect();
                for day in 1..=count {
                    let is_selected = selected == Some((y, m, day));
                    let refetch = refetch.clone();
                    let pick = move |_| {
                        let ms = js_sys::Date::parse(&format!("{y:04}-{m:02}-{day:02}T00:00:00Z"));
                        if ms.is_nan() {
                            return;
                        }
                        let due = (ms / 1000.0) as i64;
                        let refetch = refetch.clone();
                        spawn_local(async move {
                            let _ = promote_to_reminder(action_id, due).await;
                            open.set(false);
                            refetch();
                        });
                    };
                    cells.push(
                        view! {
                            <button class="cal-cell cal-day" class:cal-selected=is_selected
                                on:click=pick>
                                {day.to_string()}
                            </button>
                        }
                        .into_any(),
                    );
                }
                cells.into_iter().collect_view()
            }
        };
        view! {
            <div class="cal-backdrop" on:click=move |_| open.set(false)></div>
            <div class="cal-popover">
                <div class="cal-head">
                    <button class="cal-nav" on:click=prev>"‹"</button>
                    <span class="cal-title">
                        {move || format!(
                            "{} {}",
                            MONTH_NAMES
                                .get((view_month.get() as usize).wrapping_sub(1))
                                .copied()
                                .unwrap_or(""),
                            view_year.get(),
                        )}
                    </span>
                    <button class="cal-nav" on:click=next>"›"</button>
                </div>
                <div class="cal-grid">
                    {["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"]
                        .into_iter()
                        .map(|d| view! { <span class="cal-cell cal-dow">{d}</span> })
                        .collect_view()}
                    {grid}
                </div>
            </div>
        }
        .into_any()
    };

    view! {
        <span class="reminder-set">
            <button class="btn btn-sm btn-ghost" on:click=move |_| open.update(|o| *o = !*o)>
                {label}
            </button>
            {popover}
        </span>
    }
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

    // Reminder state: a synced/dirty/needs_review badge that shows the due date
    // (or "pending" when a due date is set but not yet synced), plus a
    // toggle-to-edit date control to (re)promote.
    let due_at = action.due_at;
    let due_label = due_at.map(unix_to_short_date);
    let badge: Option<(String, bool)> = action
        .sync_status
        .as_deref()
        .map(|s| match s {
            "needs_review" => ("⏰ needs review", true),
            "dirty" => ("⏰ reminder ⟳", false),
            _ => ("⏰ reminder", false),
        })
        .or_else(|| due_at.map(|_| ("⏰ reminder (pending)", false)))
        .map(|(label, warn)| match &due_label {
            Some(date) => (format!("{label} · {date}"), warn),
            None => (label.to_string(), warn),
        });
    let can_remind = !matches!(
        current_status,
        ActionStatus::Done | ActionStatus::Cancelled | ActionStatus::Dismissed
    );
    let remind_label = if due_at.is_some() {
        "Change date"
    } else {
        "Remind"
    };

    view! {
        <div class="action">
            <div class="action-head">
                <span class="action-title">{action.title.clone()}</span>
                <span class=conf_class>{conf_label}</span>
                <span class="badge">{status}</span>
                {badge.map(|(label, warn)| view! {
                    <span class="reminder-badge" class:reminder-warn=warn>{label}</span>
                })}
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
                {can_remind.then(|| view! {
                    <ReminderDatePicker
                        action_id=id
                        due_at=due_at
                        label=remind_label
                        refetch=refetch.clone()
                    />
                })}
                <TalkAboutButton kind="action" id=id />
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
    matches!(
        (current, target),
        (Pending, Claimed)
            | (Pending | AutoClaimed | Claimed, Done)
            | (Pending | AutoClaimed | Claimed, Dismissed)
            | (Done | Cancelled | Dismissed, Pending)
    )
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

/// The Inbox as a true list | reading pane (the Mail.app shape). The list
/// carries snippet-only rows; selecting one lazily loads the full message
/// (`get_message`) into the reading pane. The first message is auto-selected.
#[component]
pub fn InboxPane(rows: Vec<MessageDto>) -> impl IntoView {
    let first = rows.first().map(|m| m.id);
    let selected = RwSignal::new(first);
    // Session-local "read" state: the unread dot clears once a row is opened.
    // (There's no persisted read flag yet — this is purely a per-view cue.)
    let opened: RwSignal<HashSet<i64>> = RwSignal::new(first.into_iter().collect());
    let count = rows.len();

    let detail = LocalResource::new(move || {
        let id = selected.get();
        async move {
            match id {
                Some(id) => fetch_message(id).await.map(Some),
                None => Ok(None),
            }
        }
    });
    let refetch: Arc<dyn Fn() + Send + Sync> = Arc::new(move || detail.refetch());

    view! {
        <div class="body">
            <div class="list">
                <div class="list-head">
                    <span class="h">{format!("Inbox · {count}")}</span>
                </div>
                {if rows.is_empty() {
                    view! { <div class="list-empty">"No messages yet."</div> }.into_any()
                } else {
                    view! {
                        <For
                            each=move || rows.clone()
                            key=|m| m.id
                            children=move |m: MessageDto| view! {
                                <MessageListRow msg=m selected=selected opened=opened />
                            }
                        />
                    }
                    .into_any()
                }}
            </div>
            <div class="reading">
                <Suspense fallback=|| view! {
                    <div class="reading-empty">"Loading…"</div>
                }>
                    {move || {
                        let refetch = refetch.clone();
                        detail.get().map(move |res| match res {
                            Ok(Some(d)) => view! { <MessageReading detail=d refetch=refetch /> }.into_any(),
                            Ok(None) => view! {
                                <div class="reading-empty">"Select a message to read it."</div>
                            }.into_any(),
                            Err(e) => view! {
                                <div class="reading-empty">{format!("Error: {e}")}</div>
                            }.into_any(),
                        })
                    }}
                </Suspense>
            </div>
        </div>
    }
}

#[component]
fn MessageListRow(
    msg: MessageDto,
    selected: RwSignal<Option<i64>>,
    opened: RwSignal<HashSet<i64>>,
) -> impl IntoView {
    let id = msg.id;
    let from = msg
        .author_display
        .clone()
        .unwrap_or_else(|| "?".to_string());
    let subject = msg
        .subject
        .clone()
        .unwrap_or_else(|| "(no subject)".to_string());
    let snippet = msg.snippet.clone();
    let when = format_relative(msg.posted_at);
    let has_action = msg.has_action;

    let is_selected = move || selected.get() == Some(id);
    let is_read = move || opened.get().contains(&id);
    let on_click = move |_| {
        opened.update(|s| {
            s.insert(id);
        });
        selected.set(Some(id));
    };

    view! {
        <div
            class="row"
            class:selected=is_selected
            class:read=is_read
            data-message-id=id.to_string()
            on:click=on_click
        >
            <span class="udot"></span>
            <span class="from">{from}</span>
            <span class="when">{when}</span>
            <span class="subj">{subject}</span>
            <span class="snip">{snippet}</span>
            {has_action.then(|| view! {
                <span class="tags"><span class="pill action">"\u{25CE} action"</span></span>
            })}
        </div>
    }
}

/// First initials of a display name, for the reading-pane avatar.
fn initials_of(name: &str) -> String {
    let mut words = name.split_whitespace();
    let a = words.next().and_then(|w| w.chars().next());
    let b = words.next().and_then(|w| w.chars().next());
    match (a, b) {
        (Some(a), Some(b)) => format!("{}{}", a.to_uppercase(), b.to_uppercase()),
        (Some(a), None) => a.to_uppercase().to_string(),
        _ => "?".to_string(),
    }
}

#[component]
fn MessageReading(detail: MessageDetailDto, refetch: Arc<dyn Fn() + Send + Sync>) -> impl IntoView {
    let subject = detail
        .subject
        .clone()
        .unwrap_or_else(|| "(no subject)".to_string());
    let name = detail
        .author_display
        .clone()
        .or_else(|| detail.author_addr.clone())
        .unwrap_or_else(|| "?".to_string());
    let addr = detail.author_addr.clone().unwrap_or_default();
    let initials = initials_of(&name);
    let when = unix_to_short_date(detail.posted_at);
    let source = detail.source_name.clone().unwrap_or_default();
    let channel = detail.channel_name.clone().unwrap_or_default();
    let origin = [source, channel]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let actions = detail.actions.clone();
    let body = detail.body.clone();

    view! {
        <div class="read-wrap">
            <div class="read-from">
                <span class="avatar">{initials}</span>
                <span>
                    <div class="name">{name}</div>
                    <div class="addr">{addr}</div>
                </span>
                <span class="when">{when}</span>
            </div>
            <div class="read-to">{origin}</div>
            <div class="read-subject">{subject}</div>
            {actions.into_iter().map(|a| {
                let refetch = refetch.clone();
                view! { <ExtractedAction action=a refetch=refetch /> }
            }).collect_view()}
            <div class="read-body">{body}</div>
        </div>
    }
}

/// The "mnemis extracted an action" callout inside the reading pane. Wired to
/// the same mutations the Actions page uses (claim / remind / dismiss), and
/// refetches the message detail so the callout reflects the new status.
#[component]
fn ExtractedAction(
    action: MessageActionRef,
    refetch: Arc<dyn Fn() + Send + Sync>,
) -> impl IntoView {
    let id = action.id;
    let conf_class = confidence_class(action.confidence);
    let conf_label = match action.confidence {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    };
    let status = status_label(action.status);
    let due_label = action.due_at.map(unix_to_short_date);
    let meta = match &due_label {
        Some(d) => format!("{status} · due {d}"),
        None => status.to_string(),
    };
    let active = !matches!(
        action.status,
        ActionStatus::Done | ActionStatus::Cancelled | ActionStatus::Dismissed
    );
    let remind_label = if action.due_at.is_some() {
        "Change date"
    } else {
        "Remind"
    };

    let refetch_claim = refetch.clone();
    let on_claim = move |_| {
        let refetch = refetch_claim.clone();
        spawn_local(async move {
            let _ = update_action(id, ActionStatus::Claimed, None).await;
            refetch();
        });
    };
    let refetch_dismiss = refetch.clone();
    let on_dismiss = move |_| {
        let refetch = refetch_dismiss.clone();
        spawn_local(async move {
            let _ = update_action(id, ActionStatus::Dismissed, None).await;
            refetch();
        });
    };

    view! {
        <div class="extracted">
            <div class="ex-head">"\u{25CE} mnemis extracted an action"</div>
            <div class="ex-title">{action.title.clone()}</div>
            <div class="ex-meta">
                <span class=conf_class>{conf_label}</span>
                " "
                {meta}
            </div>
            <div class="ex-actions">
                {active.then(|| view! {
                    <button class="btn btn-secondary" on:click=on_claim>"Claim"</button>
                })}
                {active.then(|| {
                    let refetch = refetch.clone();
                    view! {
                        <ReminderDatePicker
                            action_id=id
                            due_at=action.due_at
                            label=remind_label
                            refetch=refetch
                        />
                    }
                })}
                {active.then(|| view! {
                    <button class="btn btn-ghost" on:click=on_dismiss>"Dismiss"</button>
                })}
                <TalkAboutButton kind="action" id=id />
            </div>
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
        <ul class="settings-home">
            <li><A href="/settings/profile">"Profile"</A>
                <span class="settings-desc">" — display name + identifiers + extraction context"</span></li>
            <li><A href="/settings/llm">"LLM"</A>
                <span class="settings-desc">" — omlx endpoint + models"</span></li>
            <li><A href="/settings/sources">"Sources"</A>
                <span class="settings-desc">" — IMAP + chat connectors"</span></li>
            <li><A href="/settings/reminders">"Reminders"</A>
                <span class="settings-desc">" — CalDAV sync to your calendar (iCloud, Nextcloud)"</span></li>
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

    let initial_level = initial.thinking_level;

    let base_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let chat_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let embed_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let token_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let level_ref: NodeRef<leptos::html::Select> = NodeRef::new();

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
        let thinking_level = level_ref
            .get()
            .map(|el| ThinkingLevel::from_wire(&el.unchecked_ref::<HtmlSelectElement>().value()))
            .unwrap_or(initial_level);
        let cfg = LlmConfigDto {
            base_url: pull(base_ref, base_url.get()),
            chat_model: pull(chat_ref, chat.get()),
            embedding_model: pull(embed_ref, embed.get()),
            bearer_token: {
                let v = pull(token_ref, token.get());
                if v.is_empty() { None } else { Some(v) }
            },
            thinking_level,
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
            <label>"Thinking budget"</label>
            <select class="settings-select" node_ref=level_ref>
                {ThinkingLevel::ALL.into_iter().map(|lvl| view! {
                    <option value=lvl.as_str() selected=lvl == initial_level>
                        {lvl.label()}
                    </option>
                }).collect_view()}
            </select>
            <div class="settings-hint">
                "How much the model is allowed to think before answering. \
                 Higher reasons more thoroughly but is slower; sent on every \
                 call so models that don't think by default still get to."
            </div>
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
/// non-empty". The Tauri command writes to the DB + keychain; folders are
/// discovered when this source's folder list opens (and again on each sync).
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
    // Discover folders from the server when the view opens, so folders created
    // server-side since the last sync show up. Mute toggles update state in
    // place (see `ChannelsList`) and never refetch, so the list is never
    // remounted mid-edit — the scroll offset and tree expansion survive every
    // click, and muting triggers no network round-trip (or macOS keychain
    // prompt).
    let channels =
        LocalResource::new(move || async move { discover_source_channels(source_id).await });
    view! {
        <Suspense fallback=|| view! { <div class="loading">"Loading folders…"</div> }>
            {move || {
                channels.get().map(|res| match res {
                    Ok(rows) if rows.is_empty() => view! {
                        <div class="empty">"No folders found for this account."</div>
                    }.into_any(),
                    Ok(rows) => view! {
                        <ChannelsList rows=rows />
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

#[component]
fn ChannelsList(rows: Vec<ChannelRowDto>) -> impl IntoView {
    let tree = build_tree(rows.clone());

    // One muted-signal per channel, owned by this list so both single toggles
    // and the bulk buttons can update state *in place*. Nothing refetches on a
    // write, so the `<ul>` is never recreated — the scroll offset and tree
    // expansion survive every click. (The source-level "Muted" badge can be
    // slightly stale until next nav — fine, the user is editing at channel
    // granularity here.)
    let muted_signals: Arc<HashMap<i64, RwSignal<bool>>> = Arc::new(
        rows.iter()
            .map(|r| (r.id, RwSignal::new(r.muted)))
            .collect(),
    );
    let total = muted_signals.len();

    let bulk = {
        let muted_signals = muted_signals.clone();
        move |muted: bool| {
            let muted_signals = muted_signals.clone();
            let ids: Vec<i64> = muted_signals.keys().copied().collect();
            spawn_local(async move {
                if set_channels_muted_bulk(ids, muted).await.is_ok() {
                    for sig in muted_signals.values() {
                        sig.set(muted);
                    }
                }
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
            <span class="channels-count">{format!("{total} folder(s)")}</span>
        </div>
        <ul class="channels-tree channels-list">
            {tree.into_iter().map(|node| {
                render_node(node, muted_signals.clone())
            }).collect_view()}
        </ul>
    }
}

fn render_node(node: ChannelNode, muted_signals: Arc<HashMap<i64, RwSignal<bool>>>) -> AnyView {
    let label = node.label.clone();
    let children = node.children;
    let row_view = match node.channel {
        Some(channel) => {
            let id = channel.id;
            let kind = channel.kind.clone();
            let count = channel.message_count;
            // Signal is owned by `ChannelsList`, so toggling updates in place
            // with no DOM rebuild. Fall back to a local signal if a path
            // somehow has no entry, so a malformed tree can't panic.
            let muted = muted_signals
                .get(&id)
                .copied()
                .unwrap_or_else(|| RwSignal::new(channel.muted));
            let on_toggle = move |_| {
                let next = !muted.get();
                spawn_local(async move {
                    if set_channel_muted(id, next).await.is_ok() {
                        muted.set(next);
                    }
                });
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
        view! {
            <ul class="channel-children">
                {children.into_iter().map(|c| {
                    render_node(c, muted_signals.clone())
                }).collect_view()}
            </ul>
        }
        .into_any()
    };

    view! { <>{row_view}{children_view}</> }.into_any()
}

// ===================== Chat view (Phase 4) =====================

/// "Talk about this" button: opens a new chat seeded from an entity and
/// navigates to it. `kind` is `"action"` or `"message"`, `id` the entity's id.
#[component]
fn TalkAboutButton(kind: &'static str, id: i64) -> impl IntoView {
    let navigate = use_navigate();
    let on_click = move |_| {
        let navigate = navigate.clone();
        spawn_local(async move {
            if let Ok(chat_id) = create_chat(Some(kind.to_string()), Some(id)).await {
                navigate(&format!("/chats/{chat_id}"), Default::default());
            }
        });
    };
    view! { <button class="btn btn-ghost" on:click=on_click>"Talk about this"</button> }
}

/// CalDAV reminders settings: connect an account, discover + pick a VTODO task
/// list, then sync. The app-specific password goes straight to the keychain via
/// the command — this component never retains it after submit.
#[component]
pub fn SettingsReminders() -> impl IntoView {
    let account = LocalResource::new(|| async move { fetch_caldav_account().await });
    let refetch: Arc<dyn Fn() + Send + Sync> = Arc::new(move || account.refetch());
    let toast: RwSignal<Option<String>> = RwSignal::new(None);
    let collections: RwSignal<Option<Vec<CaldavCollectionDto>>> = RwSignal::new(None);

    let url_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let user_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let pass_ref: NodeRef<leptos::html::Input> = NodeRef::new();

    view! {
        <h1>"Reminders"</h1>
        <p class="settings-desc">
            "Sync action items that have a due date to a CalDAV calendar as all-day events \
             with a morning-of alert (iCloud, Nextcloud, Fastmail). Give an action a due date \
             to make it a reminder."
        </p>
        {move || toast.get().map(|m| view! { <div class="settings-toast">{m}</div> })}
        <Suspense fallback=|| view! { <div class="loading">"Loading…"</div> }>
            {move || {
                let refetch = refetch.clone();
                account.get().map(move |res| match res {
                    Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
                    Ok(acc) if !acc.configured => {
                        let refetch = refetch.clone();
                        let on_connect = move |_| {
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
                            let base_url = pull(url_ref);
                            let username = pull(user_ref);
                            let password = pass_ref
                                .get()
                                .map(|el| el.unchecked_ref::<HtmlInputElement>().value())
                                .unwrap_or_default();
                            if base_url.is_empty() || username.is_empty() || password.is_empty() {
                                toast.set(Some(
                                    "Server URL, username, and app-specific password are required."
                                        .to_string(),
                                ));
                                return;
                            }
                            let refetch = refetch.clone();
                            spawn_local(async move {
                                match add_caldav_account(base_url, username, password).await {
                                    Ok(()) => {
                                        toast.set(Some(
                                            "Connected. Now discover and pick a calendar."
                                                .to_string(),
                                        ));
                                        refetch();
                                    }
                                    Err(e) => toast.set(Some(format!("Connect failed: {e}"))),
                                }
                            });
                        };
                        view! {
                            <div class="caldav-form" data-caldav="connect">
                                <label>"CalDAV server URL"</label>
                                <input class="settings-input" node_ref=url_ref
                                    placeholder="https://caldav.icloud.com" />
                                <label>"Username (Apple ID / account email)"</label>
                                <input class="settings-input" node_ref=user_ref
                                    placeholder="you@icloud.com" />
                                <label>"App-specific password"</label>
                                <input class="settings-input" node_ref=pass_ref type="password"
                                    placeholder="xxxx-xxxx-xxxx-xxxx" />
                                <div class="settings-actions">
                                    <button class="btn btn-primary" on:click=on_connect>"Connect"</button>
                                </div>
                                <p class="settings-desc">
                                    "iCloud needs an app-specific password \
                                     (appleid.apple.com → Sign-In and Security → App-Specific Passwords)."
                                </p>
                            </div>
                        }
                        .into_any()
                    }
                    Ok(acc) => {
                        let refetch = refetch.clone();
                        let collection_label = acc
                            .collection_name
                            .clone()
                            .or_else(|| acc.collection_url.clone())
                            .unwrap_or_else(|| "— none selected —".to_string());
                        let has_collection = acc.collection_url.is_some();

                        let on_discover = move |_| {
                            spawn_local(async move {
                                match list_caldav_collections().await {
                                    Ok(found) if found.is_empty() => toast.set(Some(
                                        "No calendars found on this server.".to_string(),
                                    )),
                                    Ok(found) => collections.set(Some(found)),
                                    Err(e) => toast.set(Some(format!("Discovery failed: {e}"))),
                                }
                            });
                        };
                        let on_sync = move |_| {
                            spawn_local(async move {
                                match run_sync_caldav().await {
                                    Ok(s) => toast.set(Some(format!(
                                        "Synced — {} created, {} pushed, {} pulled, {} removed, \
                                         {} need review.",
                                        s.created, s.pushed, s.pulled, s.removed, s.conflicts
                                    ))),
                                    Err(e) => toast.set(Some(format!("Sync failed: {e}"))),
                                }
                            });
                        };
                        let refetch_disc = refetch.clone();
                        let on_disconnect = move |_| {
                            let refetch = refetch_disc.clone();
                            spawn_local(async move {
                                match delete_caldav_account().await {
                                    Ok(()) => {
                                        collections.set(None);
                                        toast.set(Some("Disconnected.".to_string()));
                                        refetch();
                                    }
                                    Err(e) => toast.set(Some(format!("Disconnect failed: {e}"))),
                                }
                            });
                        };
                        let refetch_pick = refetch.clone();
                        view! {
                            <div class="caldav-connected" data-caldav="connected">
                                <div class="caldav-row"><strong>"Server: "</strong>{acc.base_url.clone()}</div>
                                <div class="caldav-row"><strong>"Account: "</strong>{acc.username.clone()}</div>
                                <div class="caldav-row"><strong>"Calendar: "</strong>{collection_label}</div>
                                <div class="settings-actions">
                                    <button class="btn btn-secondary" on:click=on_discover>"Discover calendars"</button>
                                    {has_collection.then(|| view! {
                                        <button class="btn btn-primary" on:click=on_sync>"Sync now"</button>
                                    })}
                                    <button class="btn btn-ghost btn-danger" on:click=on_disconnect>"Disconnect"</button>
                                </div>
                                {move || collections.get().map(|cols| {
                                    let refetch = refetch_pick.clone();
                                    view! {
                                        <div class="caldav-collections">
                                            <div class="settings-desc">"Choose the calendar to sync into:"</div>
                                            <For
                                                each=move || cols.clone()
                                                key=|c| c.url.clone()
                                                children=move |c: CaldavCollectionDto| {
                                                    let name = c
                                                        .display_name
                                                        .clone()
                                                        .unwrap_or_else(|| c.url.clone());
                                                    let url = c.url.clone();
                                                    let dn = c.display_name.clone();
                                                    let refetch = refetch.clone();
                                                    let on_pick = move |_| {
                                                        let url = url.clone();
                                                        let dn = dn.clone();
                                                        let refetch = refetch.clone();
                                                        spawn_local(async move {
                                                            match set_caldav_collection(url, dn).await {
                                                                Ok(()) => {
                                                                    toast.set(Some(
                                                                        "Calendar selected.".to_string(),
                                                                    ));
                                                                    collections.set(None);
                                                                    refetch();
                                                                }
                                                                Err(e) => toast.set(Some(format!(
                                                                    "Select failed: {e}"
                                                                ))),
                                                            }
                                                        });
                                                    };
                                                    view! {
                                                        <div class="caldav-collection-row">
                                                            <span>{name}</span>
                                                            <button class="btn btn-sm btn-secondary"
                                                                on:click=on_pick>"Use this calendar"</button>
                                                        </div>
                                                    }
                                                }
                                            />
                                        </div>
                                    }
                                })}
                            </div>
                        }
                        .into_any()
                    }
                })
            }}
        </Suspense>
    }
}

/// Chats list + "New chat".
#[component]
pub fn ChatsPage() -> impl IntoView {
    let refresh = RwSignal::new(0u32);
    let show_archived = RwSignal::new(false);
    let chats = LocalResource::new(move || {
        let _ = refresh.get();
        let include_archived = show_archived.get();
        async move { fetch_chats(include_archived).await }
    });
    let navigate = use_navigate();
    let on_new = move |_| {
        let navigate = navigate.clone();
        spawn_local(async move {
            if let Ok(id) = create_chat(None, None).await {
                navigate(&format!("/chats/{id}"), Default::default());
            }
        });
    };

    view! {
        <div class="chats-head">
            <div class="chats-head-actions">
                <label class="chat-show-archived">
                    <input
                        type="checkbox"
                        prop:checked=move || show_archived.get()
                        on:change=move |_| show_archived.update(|v| *v = !*v)
                    />
                    " Show archived"
                </label>
                <button class="btn btn-primary" on:click=on_new>"New chat"</button>
            </div>
        </div>
        <Suspense fallback=|| view! { <div class="loading">"Loading…"</div> }>
            {move || chats.get().map(|res| match res {
                Ok(rows) => view! { <ChatsList rows=rows refresh=refresh /> }.into_any(),
                Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn ChatsList(rows: Vec<ChatDto>, refresh: RwSignal<u32>) -> impl IntoView {
    if rows.is_empty() {
        return view! {
            <div class="empty">
                "No chats yet. Start one, or use \u{201c}Talk about this\u{201d} on an action or message."
            </div>
        }
        .into_any();
    }
    view! {
        <div class="chat-list">
            <For
                each=move || rows.clone()
                key=|c| (c.id, c.archived)
                children=move |c: ChatDto| view! { <ChatRow chat=c refresh=refresh /> }
            />
        </div>
    }
    .into_any()
}

/// One row in the chat list: the title link plus archive and delete controls.
/// Delete is two-step (a click reveals a confirm) so a misclick can't nuke a
/// conversation. Both mutations bump `refresh` to refetch the list.
#[component]
fn ChatRow(chat: ChatDto, refresh: RwSignal<u32>) -> impl IntoView {
    let id = chat.id;
    let archived = chat.archived;
    let title = chat
        .title
        .clone()
        .unwrap_or_else(|| "(new chat)".to_string());
    let href = format!("/chats/{id}");
    let seed = chat.seeded_from_kind.clone();
    let confirming = RwSignal::new(false);

    let on_archive = move |_| {
        spawn_local(async move {
            let _ = set_chat_archived(id, !archived).await;
            refresh.update(|n| *n += 1);
        });
    };

    view! {
        <div class="chat-list-row" class:chat-archived=archived attr:data-chat-id=id.to_string()>
            <A href=href attr:class="chat-list-item">
                <span class="chat-list-title">{title}</span>
                {seed.map(|k| view! { <span class="badge">{k}</span> })}
            </A>
            <div class="chat-list-actions">
                <button class="btn btn-ghost btn-sm" on:click=on_archive>
                    {if archived { "Unarchive" } else { "Archive" }}
                </button>
                {move || if confirming.get() {
                    let on_confirm = move |_| {
                        spawn_local(async move {
                            let _ = delete_chat(id).await;
                            refresh.update(|n| *n += 1);
                        });
                    };
                    let on_cancel = move |_| confirming.set(false);
                    view! {
                        <span class="chat-confirm">"Delete?"</span>
                        <button class="btn btn-sm btn-danger" on:click=on_confirm>"Yes"</button>
                        <button class="btn btn-ghost btn-sm" on:click=on_cancel>"No"</button>
                    }
                    .into_any()
                } else {
                    let on_ask = move |_| confirming.set(true);
                    view! {
                        <button class="btn btn-ghost btn-sm" on:click=on_ask>"Delete"</button>
                    }
                    .into_any()
                }}
            </div>
        </div>
    }
    .into_any()
}

/// A single chat: transcript + streaming input.
#[component]
pub fn ChatDetail() -> impl IntoView {
    let params = use_params_map();
    let chat_id = Memo::new(move |_| params.read().get("id").and_then(|s| s.parse::<i64>().ok()));

    // Streaming + toggle state lives at app scope so it survives this
    // component's remount when the user flips to the chats list and back.
    let ui = use_context::<ChatUiState>().expect("chat ui state context");
    let ChatUiState {
        show_reasoning,
        active_id,
        pending_user,
        streaming_text,
        sending,
        refresh,
        error,
    } = ui;

    let turns = LocalResource::new(move || {
        // Subscribing to `refresh` makes the transcript refetch when an
        // in-flight send finishes — even one that finished while this view was
        // unmounted — so returning to the chat shows the completed answer
        // instead of a frozen, half-streamed state.
        let _ = refresh.get();
        let id = chat_id.get();
        async move {
            match id {
                Some(id) => fetch_chat_turns(id).await,
                None => Ok(Vec::new()),
            }
        }
    });

    // What a "Talk about this" chat is grounded in (None for a blank chat).
    let seed = LocalResource::new(move || {
        let id = chat_id.get();
        async move {
            match id {
                Some(id) => fetch_chat_seed(id).await.ok().flatten(),
                None => None,
            }
        }
    });

    // Is the in-flight send (if any) for *this* chat? Gates the optimistic
    // bubble + spinner so another chat's send never shows up here.
    let is_active =
        Memo::new(move |_| active_id.get().is_some() && active_id.get() == chat_id.get());

    let input_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();

    // Set while the engine streams `Compacting` (condensing the conversation to
    // fit the model's context window); cleared by the next real event or End.
    // Drives the "condensing…" hint so a longer pause reads as expected.
    let compacting = RwSignal::new(false);

    let do_send = move || {
        let Some(id) = chat_id.get() else { return };
        if sending.get() {
            return;
        }
        let Some(input) = input_ref.get() else { return };
        let text = input.value();
        if text.trim().is_empty() {
            return;
        }
        input.set_value("");
        active_id.set(Some(id));
        sending.set(true);
        error.set(None);
        streaming_text.set(String::new());
        pending_user.set(Some(text.clone()));
        send_chat_message(id, text, move |msg| match msg {
            // A turn-level failure isn't persisted, so surface it off to the
            // side instead of refetching for it.
            ChatStream::Event(ChatEvent::Error { message }) => error.set(Some((id, message))),
            // Transient (not persisted): the engine is condensing the
            // conversation before the next model call. Show the hint; the next
            // real event clears it.
            ChatStream::Event(ChatEvent::Compacting) => compacting.set(true),
            // Assistant text as it streams: accumulate into the live bubble.
            // Not persisted yet — the completed message (below) supersedes it.
            ChatStream::Event(ChatEvent::Delta { text }) => {
                compacting.set(false);
                streaming_text.update(|s| s.push_str(&text));
            }
            // Each turn was persisted before its event fired, so a refetch shows
            // exactly what was just streamed. Driving the transcript straight
            // from the DB (instead of a separate live buffer) means returning to
            // the chat mid-send can't double the in-flight turns against their
            // persisted copies. Clearing the live buffer here hands the bubble
            // off to its persisted copy without a flicker of both.
            ChatStream::Event(_) => {
                compacting.set(false);
                streaming_text.set(String::new());
                refresh.update(|n| *n += 1);
            }
            ChatStream::End => {
                sending.set(false);
                compacting.set(false);
                streaming_text.set(String::new());
                pending_user.set(None);
                active_id.set(None);
                refresh.update(|n| *n += 1);
            }
        });
    };

    // Stop the in-flight send for this chat. The engine returns the partial
    // answer and ends the turn cleanly, so the normal End handling above runs.
    let do_stop = move || {
        let Some(id) = chat_id.get() else { return };
        spawn_local(async move {
            let _ = cancel_chat_message(id).await;
        });
    };

    // `do_send` only captures Copy handles (signals, NodeRef, Memo), so it's
    // Copy — both the keydown and click handlers can take their own copy.
    let on_keydown = move |ev: KeyboardEvent| {
        if ev.key() == "Enter" && !ev.shift_key() {
            ev.prevent_default();
            do_send();
        }
    };

    // Sticky scroll: keep the transcript pinned to the newest message while
    // streaming, but leave it alone once the user scrolls up to read history.
    let transcript_ref: NodeRef<leptos::html::Div> = NodeRef::new();
    // Whether the view was at (or near) the bottom *before* the latest content
    // arrived. Updated on every scroll; the effect below re-pins only when true.
    let at_bottom = RwSignal::new(true);
    let on_scroll = move |_| {
        if let Some(el) = transcript_ref.get_untracked() {
            let dist = el.scroll_height() - el.scroll_top() - el.client_height();
            at_bottom.set(dist <= 64);
        }
    };
    Effect::new(move |_| {
        // Re-run after anything that changes the transcript's height. Reading
        // the resource ties this to the *resolved* turns, so it fires after the
        // `<For>` has appended the new rows (not before).
        let _ = turns.get();
        let _ = refresh.get();
        let _ = pending_user.get();
        let _ = sending.get();
        if at_bottom.get_untracked()
            && let Some(el) = transcript_ref.get_untracked()
        {
            // Defer past the DOM reconciliation so scroll_height is final.
            request_animation_frame(move || el.set_scroll_top(el.scroll_height()));
        }
    });

    view! {
        <div class="chat-detail">
            <div class="chat-toolbar">
                <A href="/chats" attr:class="btn btn-ghost">"\u{2190} Chats"</A>
                // Always shown, even before a reply lands: it's a setting the
                // user wants to flip *before* sending, and it's harmless when a
                // turn has no reasoning (the reasoning block simply isn't there).
                <label class="chat-reasoning-toggle">
                    <input
                        type="checkbox"
                        prop:checked=move || show_reasoning.get()
                        on:change=move |_| {
                            let next = !show_reasoning.get();
                            show_reasoning.set(next);
                            // Persist so it's remembered across runs.
                            spawn_local(async move {
                                let _ = set_chat_show_reasoning(next).await;
                            });
                        }
                    />
                    " Show reasoning"
                </label>
            </div>
            {move || seed.get().flatten().map(|label| view! {
                <div class="chat-seed-banner">"About "<b>{label}</b></div>
            })}
            <div class="chat-transcript" node_ref=transcript_ref on:scroll=on_scroll>
                // Transition (not Suspense) so a refetch keeps the current
                // transcript on screen instead of collapsing to the fallback —
                // which is what reset the scroll on every streamed event.
                <Transition fallback=|| view! { <div class="loading">"Loading…"</div> }>
                    {move || turns.get().map(|res| match res {
                        Ok(rows) => view! {
                            <ChatTranscript turns=rows show_reasoning=show_reasoning />
                        }.into_any(),
                        Err(e) => view! { <div class="error">{format!("Error: {e}")}</div> }.into_any(),
                    })}
                </Transition>
                // Optimistic echo of the just-sent message, so it appears
                // instantly rather than after the (slow) first model turn. It's
                // dropped the moment its persisted copy shows up in `turns`,
                // which is what keeps it from rendering twice.
                {move || {
                    if !is_active.get() {
                        return None;
                    }
                    let pending = pending_user.get()?;
                    let already_persisted = turns
                        .get()
                        .and_then(|res| res.ok())
                        .map(|rows| {
                            rows.iter().any(|t| {
                                t.role == "user" && t.content.as_deref() == Some(pending.as_str())
                            })
                        })
                        .unwrap_or(false);
                    (!already_persisted)
                        .then(|| view! { <div class="chat-msg chat-user">{pending}</div> })
                }}
                // The assistant's answer as it streams in, before its persisted
                // copy lands. Cleared (handed off to the transcript) on the
                // completed message — see the Delta handler in `do_send`.
                {move || {
                    if !is_active.get() {
                        return None;
                    }
                    let text = streaming_text.get();
                    (!text.is_empty())
                        .then(|| view! { <div class="chat-msg chat-assistant chat-streaming">{text}</div> })
                }}
                {move || {
                    // While streaming text is on screen the live bubble already
                    // shows progress, so the dots only fill the gap before the
                    // first token (prefill) or while condensing.
                    let show =
                        is_active.get() && sending.get() && streaming_text.get().is_empty();
                    let condensing = compacting.get();
                    show.then(|| {
                        let label = if condensing {
                            "Condensing earlier conversation\u{2026}"
                        } else {
                            "\u{2026}"
                        };
                        view! { <div class="chat-thinking" class:chat-compacting=condensing>{label}</div> }
                    })
                }}
                // A turn-level failure for *this* chat (survives the spinner
                // stopping so the user actually sees why it stopped).
                {move || error.get()
                    .filter(|(cid, _)| Some(*cid) == chat_id.get())
                    .map(|(_, msg)| view! { <div class="error chat-error">{msg}</div> })}
            </div>
            <div class="chat-input">
                // The textarea stays enabled while a send runs so the user can
                // draft their next message; only the Send button is disabled, so
                // the message can't actually go out until the turn finishes.
                <textarea
                    node_ref=input_ref
                    placeholder="Ask about your actions or messages\u{2026}"
                    on:keydown=on_keydown
                />
                // Send becomes Stop while this chat's turn is in flight, so the
                // user decides when a slow answer has gone on long enough.
                {move || {
                    if sending.get() && is_active.get() {
                        view! {
                            <button class="btn btn-secondary chat-stop" on:click=move |_| do_stop()>
                                "Stop"
                            </button>
                        }
                        .into_any()
                    } else {
                        view! {
                            <button
                                class="btn btn-primary"
                                on:click=move |_| do_send()
                                prop:disabled=move || sending.get()
                            >
                                "Send"
                            </button>
                        }
                        .into_any()
                    }
                }}
            </div>
        </div>
    }
}

#[component]
fn ChatTranscript(turns: Vec<ChatTurnDto>, show_reasoning: RwSignal<bool>) -> impl IntoView {
    if turns.is_empty() {
        return view! {
            <div class="empty">"No messages yet — ask a question to get started."</div>
        }
        .into_any();
    }
    view! {
        <For
            each=move || turns.clone()
            key=|t| t.id
            children=move |t: ChatTurnDto| view! {
                <ChatTurnView turn=t show_reasoning=show_reasoning />
            }
        />
    }
    .into_any()
}

#[component]
fn ChatTurnView(turn: ChatTurnDto, show_reasoning: RwSignal<bool>) -> impl IntoView {
    let role = turn.role.clone();
    let content = turn.content.clone().unwrap_or_default();
    let reasoning = turn.reasoning.clone().filter(|r| !r.trim().is_empty());

    let reasoning_view = reasoning.map(|r| {
        view! {
            <div class="chat-reasoning" class:hidden=move || !show_reasoning.get()>
                <pre>{r}</pre>
            </div>
        }
    });

    let body = match (role.as_str(), turn.tool_name.clone()) {
        ("user", _) => view! { <div class="chat-msg chat-user">{content}</div> }.into_any(),
        ("tool", Some(name)) => tool_disclosure(false, name, content),
        ("assistant", Some(name)) => tool_disclosure(true, name, content),
        ("assistant", None) if !content.is_empty() => {
            // The model answers in markdown; render it as sanitized HTML so
            // formatting (lists, code, links) shows the way the user expects.
            let html = markdown_to_html(&content);
            view! {
                <div
                    class="chat-msg chat-assistant"
                    inner_html=html
                    on:click=open_link_externally
                ></div>
            }
            .into_any()
        }
        _ => ().into_any(),
    };

    view! { <>{reasoning_view}{body}</> }
}

/// Click handler for the assistant bubble: when a rendered markdown link is
/// clicked, send it to the OS browser instead of navigating the webview.
fn open_link_externally(ev: MouseEvent) {
    let Some(anchor) = ev
        .target()
        .and_then(|t| t.dyn_into::<Element>().ok())
        .and_then(|el| el.closest("a").ok().flatten())
    else {
        return;
    };
    let Some(href) = anchor.get_attribute("href") else {
        return;
    };
    if is_safe_href(&href) {
        ev.prevent_default();
        spawn_local(async move {
            let _ = open_external(href).await;
        });
    }
}

/// A collapsed `<details>` block for a tool call or result (Claude-Code style).
/// `is_call` picks the direction marker and its color: a blue ▶ for the agent's
/// outgoing call, a green ◀ for the tool's reply.
fn tool_disclosure(is_call: bool, name: String, body: String) -> AnyView {
    let (marker, marker_class) = if is_call {
        ("\u{25B6}", "chat-tool-arrow chat-tool-call")
    } else {
        ("\u{25C0}", "chat-tool-arrow chat-tool-result")
    };
    view! {
        <details class="chat-tool">
            // Marker trails the name so it doesn't sit next to the <details>
            // revealer triangle (which would read as a second direction arrow).
            <summary>
                {name}
                " "
                <span class=marker_class>{marker}</span>
            </summary>
            <pre class="chat-tool-body">{body}</pre>
        </details>
    }
    .into_any()
}
