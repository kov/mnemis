//! Writing mail: the composer, the reply buttons that open it, the card an
//! assistant's draft shows in chat, and each account's sending settings.
//! Nothing is sent but by the composer's Send button.

use leptos::prelude::*;
use leptos::task::spawn_local;
use mnemis_types::{DraftDto, DraftKind, DraftStatus, SmtpSettingsDto, SourceRowDto};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::fetch_settings_sources;

/// The draft open in the composer, if any.
#[derive(Clone, Copy)]
pub struct Composer {
    pub draft: RwSignal<Option<i64>>,
    /// Bumped whenever a draft changes, so cards showing one reload it.
    pub version: RwSignal<u32>,
    /// Why opening the composer failed.
    pub error: RwSignal<Option<String>>,
}

impl Composer {
    pub fn new() -> Self {
        Self {
            draft: RwSignal::new(None),
            version: RwSignal::new(0),
            error: RwSignal::new(None),
        }
    }
}

fn composer() -> Composer {
    use_context::<Composer>().expect("composer context")
}

async fn call<T: DeserializeOwned>(cmd: &str, args: impl Serialize) -> Result<T, String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    let raw = crate::invoke(cmd, args)
        .await
        .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
    serde_wasm_bindgen::from_value(raw).map_err(|e| e.to_string())
}

/// For commands that return nothing.
async fn run(cmd: &str, args: impl Serialize) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    crate::invoke(cmd, args)
        .await
        .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
    Ok(())
}

#[derive(Serialize)]
struct DraftIdArgs {
    draft_id: i64,
}

#[derive(Serialize)]
struct StartDraftArgs {
    kind: DraftKind,
    message_id: Option<i64>,
}

#[derive(Serialize)]
struct SaveDraftArgs<'a> {
    draft: &'a DraftDto,
}

#[derive(Serialize)]
struct MessageIdArgs {
    message_id: i64,
}

#[derive(Serialize)]
struct SourceIdArgs {
    source_id: i64,
}

#[derive(Serialize)]
struct SaveSmtpArgs {
    settings: SmtpSettingsDto,
    password: Option<String>,
}

async fn get_draft(draft_id: i64) -> Result<DraftDto, String> {
    call("get_draft", DraftIdArgs { draft_id }).await
}

/// Start a mail of `kind`, from `message_id` for a reply or forward, and open
/// the composer on it.
fn compose(kind: DraftKind, message_id: Option<i64>) {
    let c = composer();
    spawn_local(async move {
        match call::<i64>("start_draft", StartDraftArgs { kind, message_id }).await {
            Ok(id) => c.draft.set(Some(id)),
            Err(e) => c.error.set(Some(format!("Couldn't start the email: {e}"))),
        }
    });
}

/// Reply, Reply all and Forward for the email a to-do came from.
#[component]
pub fn ReplyButtons(message_id: i64) -> impl IntoView {
    // So Reply opens without waiting on the server.
    spawn_local(async move {
        let _ = run("prefetch_original", MessageIdArgs { message_id }).await;
    });
    view! {
        <button class="btn btn-secondary" data-action="reply"
            on:click=move |_| compose(DraftKind::Reply, Some(message_id))>"Reply"</button>
        <button class="btn btn-ghost" data-action="reply-all"
            on:click=move |_| compose(DraftKind::ReplyAll, Some(message_id))>"Reply all"</button>
        <button class="btn btn-ghost" data-action="forward"
            on:click=move |_| compose(DraftKind::Forward, Some(message_id))>"Forward"</button>
    }
}

/// The composer, over everything while a draft is open.
#[component]
pub fn ComposeHost() -> impl IntoView {
    let c = composer();
    view! {
        {move || c.error.get().map(|e| view! {
            <div class="compose-toast" on:click=move |_| c.error.set(None)>{e}</div>
        })}
        {move || c.draft.get().map(|id| view! { <ComposeLoader draft_id=id /> })}
    }
}

#[component]
fn ComposeLoader(draft_id: i64) -> impl IntoView {
    let c = composer();
    let draft = LocalResource::new(move || get_draft(draft_id));
    let accounts = LocalResource::new(fetch_settings_sources);
    view! {
        <Suspense fallback=|| ()>
            {move || match (draft.get(), accounts.get()) {
                (Some(Ok(d)), Some(a)) => {
                    let accounts: Vec<SourceRowDto> = a
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|s| s.kind == "imap")
                        .collect();
                    view! { <ComposeForm draft=d accounts=accounts /> }.into_any()
                }
                (Some(Err(e)), _) => {
                    c.error.set(Some(format!("Couldn't open the draft: {e}")));
                    c.draft.set(None);
                    ().into_any()
                }
                _ => ().into_any(),
            }}
        </Suspense>
    }
}

#[component]
fn ComposeForm(draft: DraftDto, accounts: Vec<SourceRowDto>) -> impl IntoView {
    let c = composer();
    let source_id = RwSignal::new(draft.source_id);
    let to = RwSignal::new(draft.to.clone());
    let cc = RwSignal::new(draft.cc.clone());
    let bcc = RwSignal::new(draft.bcc.clone());
    let subject = RwSignal::new(draft.subject.clone());
    let body = RwSignal::new(draft.body.clone());
    let show_copies = RwSignal::new(!draft.cc.is_empty() || !draft.bcc.is_empty());
    let sending = RwSignal::new(false);
    let error: RwSignal<Option<String>> = RwSignal::new(None);

    let title = match draft.kind {
        DraftKind::New => "New email",
        DraftKind::Reply | DraftKind::ReplyAll => "Reply",
        DraftKind::Forward => "Forward",
    };
    let is_forward = draft.kind == DraftKind::Forward;
    let keepable = draft.chat_id.is_some();
    let by_agent = draft.by_agent;
    let from = draft.from.clone();
    let id = draft.id;
    let base = StoredValue::new(draft);

    let current = move || {
        let mut d = base.get_value();
        d.source_id = source_id.get_untracked();
        d.to = to.get_untracked();
        d.cc = cc.get_untracked();
        d.bcc = bcc.get_untracked();
        d.subject = subject.get_untracked();
        d.body = body.get_untracked();
        d
    };
    let close = move || {
        c.draft.set(None);
        c.version.update(|v| *v += 1);
    };

    let on_send = move |_| {
        let d = current();
        sending.set(true);
        error.set(None);
        spawn_local(async move {
            let result = async {
                run("save_draft", SaveDraftArgs { draft: &d }).await?;
                call::<DraftDto>("send_draft", DraftIdArgs { draft_id: id }).await
            }
            .await;
            sending.set(false);
            match result {
                Ok(sent) => {
                    // Sent, but the copy wasn't filed: worth a note, not a failure.
                    if let Some(note) = sent.error {
                        c.error.set(Some(note));
                    }
                    close();
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let on_keep = move |_| {
        let d = current();
        spawn_local(async move {
            match run("save_draft", SaveDraftArgs { draft: &d }).await {
                Ok(()) => close(),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let on_discard = move |_| {
        spawn_local(async move {
            let _ = run("discard_draft", DraftIdArgs { draft_id: id }).await;
            close();
        });
    };

    let from_field = if accounts.len() > 1 {
        view! {
            <select class="settings-select" data-field="compose-from"
                on:change=move |ev| source_id.set(event_target_value(&ev).parse().ok())>
                {accounts.into_iter().map(|a| view! {
                    <option value=a.id.to_string() selected=Some(a.id) == source_id.get_untracked()>{a.name}</option>
                }).collect_view()}
            </select>
        }
        .into_any()
    } else {
        let shown = if from.is_empty() {
            "No sender address: set one in Settings → Sources".to_string()
        } else {
            from
        };
        view! { <span class="compose-from" data-field="compose-from">{shown}</span> }.into_any()
    };

    let field = move |label: &'static str, name: &'static str, value: RwSignal<String>| {
        view! {
            <label>{label}</label>
            <input
                class="settings-input"
                data-field=name
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            />
        }
    };

    view! {
        <div class="compose-backdrop">
            <div class="compose" data-draft-id=id.to_string()>
                <div class="compose-head">
                    <span class="modal-title">{title}</span>
                    {by_agent.then(|| view! { <span class="pill">"drafted by the assistant"</span> })}
                </div>
                <div class="compose-fields">
                    <label>"From"</label>
                    {from_field}
                    {field("To", "compose-to", to)}
                    {move || if show_copies.get() {
                        view! {
                            {field("Cc", "compose-cc", cc)}
                            {field("Bcc", "compose-bcc", bcc)}
                        }
                        .into_any()
                    } else {
                        view! {
                            <span></span>
                            <button class="btn btn-ghost btn-sm compose-copies"
                                on:click=move |_| show_copies.set(true)>"Add Cc or Bcc"</button>
                        }
                        .into_any()
                    }}
                    {field("Subject", "compose-subject", subject)}
                </div>
                <textarea
                    class="compose-body"
                    data-field="compose-body"
                    prop:value=move || body.get()
                    on:input=move |ev| body.set(event_target_value(&ev))
                />
                {is_forward.then(|| view! {
                    <div class="settings-hint">"The original email and its attachments go along when you send."</div>
                })}
                {move || error.get().map(|e| view! { <div class="settings-toast" data-field="compose-error">{format!("Not sent: {e}")}</div> })}
                <div class="settings-actions">
                    <button class="btn btn-ghost" data-action="discard" on:click=on_discard>"Discard"</button>
                    {keepable.then(|| view! {
                        <button class="btn btn-ghost" data-action="keep" on:click=on_keep>"Keep for later"</button>
                    })}
                    <button
                        class="btn btn-primary"
                        data-action="send"
                        prop:disabled=move || sending.get()
                        on:click=on_send
                    >
                        {move || if sending.get() { "Sending…" } else { "Send" }}
                    </button>
                </div>
            </div>
        </div>
    }
}

/// The draft id in a `draft_email` tool result, when it made one.
pub fn draft_id_in(output: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(output)
        .ok()?
        .get("draft_id")?
        .as_i64()
}

/// An email the assistant drafted, in the conversation: who it's to, how it
/// starts, and the way to review and send it.
#[component]
pub fn DraftCard(draft_id: i64) -> impl IntoView {
    let c = composer();
    let draft = LocalResource::new(move || {
        c.version.track();
        get_draft(draft_id)
    });
    view! {
        <Suspense fallback=|| ()>
            {move || draft.get().map(|res| match res {
                Ok(d) => {
                    let status = match d.status {
                        DraftStatus::Draft => "Email draft",
                        DraftStatus::Sent => "Email sent",
                        DraftStatus::Discarded => "Email discarded",
                    };
                    let open = d.status == DraftStatus::Draft;
                    let subject = if d.subject.is_empty() { "(no subject)".to_string() } else { d.subject.clone() };
                    view! {
                        <div class="draft-card" data-draft-id=draft_id.to_string()>
                            <div class="draft-card-status">{status}</div>
                            {(!d.to.is_empty()).then(|| view! { <div class="draft-card-to">{format!("To {}", d.to)}</div> })}
                            <div class="draft-card-subject">{subject}</div>
                            <div class="draft-card-body">{d.body.trim().to_string()}</div>
                            {open.then(|| view! {
                                <button class="btn btn-primary btn-sm" data-action="review-draft"
                                    on:click=move |_| c.draft.set(Some(draft_id))>"Review and send"</button>
                            })}
                        </div>
                    }
                    .into_any()
                }
                Err(e) => view! { <div class="error">{format!("Couldn't load the draft: {e}")}</div> }.into_any(),
            })}
        </Suspense>
    }
}

/// How an account sends: the outgoing server, the sender, and whether to
/// file a copy in Sent.
#[component]
pub fn SendingSettings(source_id: i64) -> impl IntoView {
    let settings = LocalResource::new(move || {
        call::<SmtpSettingsDto>("get_smtp_settings", SourceIdArgs { source_id })
    });
    view! {
        <Suspense fallback=|| view! { <div class="loading">"Loading…"</div> }>
            {move || settings.get().map(|res| match res {
                Ok(s) => view! { <SendingForm initial=s /> }.into_any(),
                Err(e) => view! { <div class="error">{e}</div> }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn SendingForm(initial: SmtpSettingsDto) -> impl IntoView {
    let from_name = RwSignal::new(initial.from_name.clone());
    let from_address = RwSignal::new(initial.from_address.clone());
    let server = RwSignal::new(initial.server.clone());
    let port = RwSignal::new(initial.port.to_string());
    let starttls = RwSignal::new(initial.starttls);
    let username = RwSignal::new(initial.username.clone());
    let password = RwSignal::new(String::new());
    let own_password = RwSignal::new(initial.own_password);
    let save_sent = RwSignal::new(initial.save_sent);
    let saved = RwSignal::new(initial.saved);
    let toast: RwSignal<Option<String>> = RwSignal::new(None);
    let source_id = initial.source_id;

    let on_save = move |_| {
        let Ok(port_number) = port.get_untracked().trim().parse::<u16>() else {
            toast.set(Some("The port must be a number.".into()));
            return;
        };
        let settings = SmtpSettingsDto {
            source_id,
            server: server.get_untracked(),
            port: port_number,
            starttls: starttls.get_untracked(),
            username: username.get_untracked(),
            own_password: own_password.get_untracked(),
            from_name: from_name.get_untracked(),
            from_address: from_address.get_untracked(),
            save_sent: save_sent.get_untracked(),
            saved: true,
        };
        let pw = Some(password.get_untracked()).filter(|p| !p.is_empty());
        spawn_local(async move {
            match run(
                "save_smtp_settings",
                SaveSmtpArgs {
                    settings,
                    password: pw.clone(),
                },
            )
            .await
            {
                Ok(()) => {
                    if pw.is_some() {
                        own_password.set(true);
                    }
                    password.set(String::new());
                    saved.set(true);
                    toast.set(Some("Saved.".into()));
                }
                Err(e) => toast.set(Some(format!("Not saved: {e}"))),
            }
        });
    };

    let text = move |label: &'static str, name: &'static str, value: RwSignal<String>| {
        view! {
            <label>{label}</label>
            <input
                class="settings-input"
                data-field=name
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            />
        }
    };

    view! {
        <div class="settings-form sending-form" data-form="smtp">
            <div class="settings-subhead">"Sending"</div>
            {move || (!saved.get()).then(|| view! {
                <div class="settings-hint">"Guessed from the incoming server; check them before your first send."</div>
            })}
            {text("Sender name", "smtp-from-name", from_name)}
            {text("Sender address", "smtp-from-address", from_address)}
            {text("Outgoing server", "smtp-server", server)}
            {text("Port", "smtp-port", port)}
            <label class="settings-check">
                <input type="checkbox" prop:checked=move || starttls.get()
                    on:change=move |ev| starttls.set(event_target_checked(&ev)) />
                "Use STARTTLS (usually port 587) instead of TLS from the start (465)"
            </label>
            {text("Username", "smtp-username", username)}
            <label>"Password"</label>
            <input
                class="settings-input"
                type="password"
                data-field="smtp-password"
                placeholder=move || if own_password.get() { "saved" } else { "same as for reading mail" }
                prop:value=move || password.get()
                on:input=move |ev| password.set(event_target_value(&ev))
            />
            <label class="settings-check">
                <input type="checkbox" data-field="smtp-save-sent" prop:checked=move || save_sent.get()
                    on:change=move |ev| save_sent.set(event_target_checked(&ev)) />
                "Save a copy to Sent"
            </label>
            <div class="settings-hint">"Only if the server doesn't already keep the mail you send. Gmail does, so a copy there would show up twice."</div>
            <div class="settings-actions">
                <button class="btn btn-primary" data-action="save-smtp" on:click=on_save>"Save sending settings"</button>
            </div>
            {move || toast.get().map(|t| view! { <div class="settings-toast">{t}</div> })}
        </div>
    }
}
