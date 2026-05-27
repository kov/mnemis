use leptos::prelude::*;
use mnemis_types::{ActionDto, Confidence};

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
