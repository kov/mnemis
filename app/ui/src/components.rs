use leptos::prelude::*;
use mnemis_types::{ActionDto, Confidence, MessageDto};

use crate::{confidence_class, status_label};

#[component]
pub fn ActionsList(rows: Vec<ActionDto>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="empty">"No active actions."</div> }.into_any();
    }

    let (low, rest): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|a| matches!(a.confidence, Confidence::Low));

    let show_low = RwSignal::new(false);
    let low_count = low.len();
    let low_for_view = StoredValue::new(low);

    view! {
        <div>
            <For
                each=move || rest.clone()
                key=|a| a.id
                children=move |a: ActionDto| view! { <ActionCard action=a /> }
            />

            {move || (low_count > 0).then(|| view! {
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
                        children=move |a: ActionDto| view! { <ActionCard action=a /> }
                    />
                </Show>
            })}
        </div>
    }
    .into_any()
}

#[component]
fn ActionCard(action: ActionDto) -> impl IntoView {
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
        </div>
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
