//! The mail reader: every account's folders in the sidebar, a folder's
//! threads (or every inbox's), and the open thread's messages. Read
//! messages start collapsed, unread ones and the latest open; an unread
//! message counts as read once it has been on screen for a moment.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys::{Element, HtmlIFrameElement, MouseEvent};
use leptos_router::components::A;
use mnemis_types::{
    DraftKind, MailScope, MailboxDto, MailboxRole, RenderedMessageDto, ThreadDto, ThreadMessageDto,
    ThreadSummaryDto,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::components::TalkAboutButton;
use crate::compose::compose;
use crate::markdown::is_safe_href;
use crate::{SyncTick, open_external};

/// Threads per page of the list.
const PAGE: i64 = 50;
/// How long an unread message is on screen before it counts as read.
const READ_AFTER_MS: u64 = 1500;
/// How often the mail-only sync runs while the app is open.
const SYNC_EVERY_SECS: u64 = 300;

/// Bumped when mail changed (a mail sync, a message marked read), so the
/// folder counts and thread lists reload.
#[derive(Clone, Copy)]
pub struct MailTick(pub RwSignal<u32>);

/// The folders, shared by the sidebar and the toolbar title.
#[derive(Clone, Copy)]
pub struct Mailboxes(pub RwSignal<Vec<MailboxDto>>);

async fn call<T: DeserializeOwned>(cmd: &str, args: impl Serialize) -> Result<T, String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    let raw = crate::invoke(cmd, args)
        .await
        .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
    serde_wasm_bindgen::from_value(raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct NoArgs {}

#[derive(Serialize)]
struct ThreadsArgs {
    scope: MailScope,
    before: Option<i64>,
    limit: i64,
}

#[derive(Serialize)]
struct ThreadArgs {
    thread_id: i64,
}

#[derive(Serialize)]
struct RenderArgs {
    message_id: i64,
    allow_remote: bool,
}

#[derive(Serialize)]
struct AttachmentArgs {
    message_id: i64,
    part: u32,
}

#[derive(Serialize)]
struct MessageIdArgs {
    message_id: i64,
}

#[derive(Serialize)]
struct SeenArgs {
    message_ids: Vec<i64>,
    seen: bool,
}

/// Set up the reader's shared state, and keep mail current while the app is
/// open: a mail-only sync now and every few minutes.
pub fn provide() {
    let tick = MailTick(RwSignal::new(0));
    provide_context(tick);
    provide_context(Mailboxes(RwSignal::new(Vec::new())));
    let sync = move || {
        spawn_local(async move {
            match call::<Vec<String>>("sync_mail", NoArgs {}).await {
                Ok(errors) => {
                    for e in errors {
                        gloo_console::warn!(format!("mail sync: {e}"));
                    }
                }
                Err(e) => gloo_console::warn!(format!("mail sync failed: {e}")),
            }
            tick.0.update(|v| *v += 1);
        })
    };
    sync();
    if let Ok(handle) =
        set_interval_with_handle(sync, std::time::Duration::from_secs(SYNC_EVERY_SECS))
    {
        on_cleanup(move || handle.clear());
    }
}

fn mail_tick() -> RwSignal<u32> {
    use_context::<MailTick>().expect("mail tick context").0
}

/// The mail section of the sidebar: All Inboxes, then each account's
/// folders with their unread counts.
#[component]
pub fn MailNav() -> impl IntoView {
    let SyncTick(sync_tick) = use_context::<SyncTick>().expect("sync tick context");
    let tick = mail_tick();
    let Mailboxes(boxes) = use_context::<Mailboxes>().expect("mailboxes context");
    Effect::new(move |_| {
        let _ = (sync_tick.get(), tick.get());
        spawn_local(async move {
            if let Ok(rows) = call::<Vec<MailboxDto>>("mailboxes", NoArgs {}).await {
                boxes.set(rows);
            }
        });
    });
    let inbox_unread = move || {
        boxes
            .get()
            .iter()
            .filter(|b| b.role == MailboxRole::Inbox)
            .map(|b| b.unread)
            .sum::<i64>()
    };
    let accounts = move || {
        let mut out: Vec<(i64, String, Vec<MailboxDto>)> = Vec::new();
        for b in boxes.get() {
            match out.iter_mut().find(|a| a.0 == b.source_id) {
                Some(a) => a.2.push(b),
                None => out.push((b.source_id, b.account.clone(), vec![b])),
            }
        }
        out
    };
    view! {
        <div class="side-section mail-nav">
            <div class="side-label">"Mail"</div>
            <A href="/inbox" attr:class="side-item" attr:data-mailbox="all-inboxes">
                <span class="ico">"\u{2709}"</span>"All Inboxes"
                {move || unread_badge(inbox_unread())}
            </A>
            // Re-rendered whole on each refresh, so the counts stay current;
            // it's a few dozen rows.
            {move || accounts().into_iter().map(|(_, account, folders)| view! {
                <div class="side-label side-account">{account}</div>
                {folders.into_iter().map(|b| {
                    let indent = format!("padding-left: {}px", 8 + 12 * depth(&b.path));
                    view! {
                        <A href=format!("/mail/{}", b.channel_id)
                            attr:class="side-item folder"
                            attr:style=indent
                            attr:data-mailbox=b.channel_id.to_string()
                            attr:title=b.path.clone()>
                            <span class="ico">{role_icon(b.role)}</span>
                            <span class="folder-name">{b.name.clone()}</span>
                            {unread_badge(b.unread)}
                        </A>
                    }
                }).collect_view()}
            }).collect_view()}
        </div>
    }
}

fn unread_badge(n: i64) -> Option<impl IntoView> {
    (n > 0).then(|| view! { <span class="count">{n.to_string()}</span> })
}

/// How deep a folder sits: levels below the top, as its path shows them.
fn depth(path: &str) -> usize {
    if path.contains('/') {
        path.matches('/').count()
    } else if path.len() > 6 && path[..6].eq_ignore_ascii_case("INBOX.") {
        path.matches('.').count()
    } else {
        0
    }
}

fn role_icon(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "\u{2709}",
        MailboxRole::Drafts => "\u{270E}",
        MailboxRole::Sent => "\u{27A4}",
        MailboxRole::Archive => "\u{25A4}",
        MailboxRole::Junk => "\u{26A0}",
        MailboxRole::Trash => "\u{2716}",
        MailboxRole::Other => "\u{25AB}",
    }
}

/// The toolbar title for a mail route.
pub fn title_for(path: &str) -> Option<String> {
    if path.starts_with("/inbox") {
        return Some("All Inboxes".to_string());
    }
    let id: i64 = path.strip_prefix("/mail/")?.parse().ok()?;
    let Mailboxes(boxes) = use_context::<Mailboxes>()?;
    Some(
        boxes
            .get()
            .into_iter()
            .find(|b| b.channel_id == id)
            .map(|b| format!("{} — {}", b.name, b.account))
            .unwrap_or_else(|| "Mail".to_string()),
    )
}

/// A folder's threads (or every inbox's) | the open thread. Nothing opens
/// by itself: opening a thread marks its mail read.
#[component]
pub fn MailPage(scope: MailScope) -> impl IntoView {
    let SyncTick(sync_tick) = use_context::<SyncTick>().expect("sync tick context");
    let tick = mail_tick();
    let pages = RwSignal::new(1i64);
    let selected = RwSignal::new(None::<i64>);
    let threads = LocalResource::new(move || {
        let _ = (sync_tick.get(), tick.get());
        let limit = PAGE * pages.get();
        async move {
            call::<Vec<ThreadSummaryDto>>(
                "mail_threads",
                ThreadsArgs {
                    scope,
                    before: None,
                    limit,
                },
            )
            .await
        }
    });
    view! {
        <div class="body mail">
            <div class="list" data-field="thread-list">
                // Transition, not Suspense: a refresh keeps the list on screen.
                <Transition fallback=|| view! { <div class="list-empty">"Loading\u{2026}"</div> }>
                    {move || threads.get().map(|res| match res {
                        Ok(rows) if rows.is_empty() => view! {
                            <div class="list-empty">"No mail here in the last 90 days."</div>
                        }.into_any(),
                        Ok(rows) => {
                            let more = rows.len() as i64 >= PAGE * pages.get_untracked();
                            view! {
                                <For
                                    each=move || rows.clone()
                                    key=|t| (t.thread_id, t.latest_at, t.unread_count, t.message_count)
                                    children=move |t| view! { <ThreadRow thread=t selected=selected /> }
                                />
                                {more.then(|| view! {
                                    <button class="btn btn-ghost load-more"
                                        on:click=move |_| pages.update(|p| *p += 1)>
                                        "Load more"
                                    </button>
                                })}
                            }.into_any()
                        }
                        Err(e) => view! { <div class="list-empty">{format!("Error: {e}")}</div> }.into_any(),
                    })}
                </Transition>
            </div>
            <div class="reading">
                {move || match selected.get() {
                    Some(id) => view! { <ThreadView thread_id=id /> }.into_any(),
                    None => view! {
                        <div class="reading-empty">"Select a conversation to read it."</div>
                    }.into_any(),
                }}
            </div>
        </div>
    }
}

#[component]
fn ThreadRow(thread: ThreadSummaryDto, selected: RwSignal<Option<i64>>) -> impl IntoView {
    let id = thread.thread_id;
    let unread = thread.unread_count > 0;
    let who = if thread.participants.is_empty() {
        "Unknown".to_string()
    } else {
        thread.participants.join(", ")
    };
    let subject = if thread.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        thread.subject.clone()
    };
    view! {
        <div class="row trow" class:read=!unread class:selected=move || selected.get() == Some(id)
            data-thread-id=id.to_string()
            on:click=move |_| selected.set(Some(id))>
            <span class="udot"></span>
            <span class="from">
                {who}
                {(thread.message_count > 1).then(|| view! {
                    <span class="tcount">{thread.message_count.to_string()}</span>
                })}
            </span>
            <span class="when">{crate::components::email_short_date(thread.latest_at)}</span>
            <span class="subj">{subject}</span>
            <span class="snip">{thread.snippet.clone()}</span>
            {(thread.todo_count > 0 || thread.has_attachments || thread.flagged).then(|| view! {
                <span class="tags">
                    {(thread.todo_count > 0).then(|| view! {
                        <span class="pill action">{format!("\u{25CE} {} to-do", thread.todo_count)}</span>
                    })}
                    {thread.has_attachments.then(|| view! { <span class="pill">"\u{1F4CE}"</span> })}
                    {thread.flagged.then(|| view! { <span class="pill warn">"\u{2691}"</span> })}
                </span>
            })}
        </div>
    }
}

/// Bumped from a message card when it made a to-do, so the thread's list of
/// to-dos reloads.
#[derive(Clone, Copy)]
struct ThreadTodosChanged(RwSignal<u32>);

/// The open thread: its to-dos, then its messages, oldest first.
#[component]
fn ThreadView(thread_id: i64) -> impl IntoView {
    // Bumped when the thread's missing mail has arrived, so incomplete
    // messages render again.
    let downloaded = RwSignal::new(0u32);
    // Bumped when the thread gained a to-do.
    let todos_changed = RwSignal::new(0u32);
    provide_context(ThreadTodosChanged(todos_changed));
    let thread = LocalResource::new(move || {
        let _ = (downloaded.get(), todos_changed.get());
        call::<ThreadDto>("mail_thread", ThreadArgs { thread_id })
    });
    spawn_local(async move {
        match call::<usize>("fetch_thread", ThreadArgs { thread_id }).await {
            Ok(n) if n > 0 => downloaded.update(|v| *v += 1),
            Ok(_) => {}
            Err(e) => gloo_console::warn!(format!("couldn't download the thread: {e}")),
        }
    });
    // Which messages are open is decided once, when the thread first loads:
    // unread ones and the latest. After that only the user opens and closes
    // them, so a message doesn't fold away as it's marked read.
    let open: RwSignal<Option<Vec<i64>>> = RwSignal::new(None);
    view! {
        <Transition fallback=|| view! { <div class="reading-empty">"Loading\u{2026}"</div> }>
            {move || thread.get().map(|res| match res {
                Ok(t) => {
                    if open.get_untracked().is_none() {
                        let last = t.messages.last().map(|m| m.message_id);
                        open.set(Some(
                            t.messages
                                .iter()
                                .filter(|m| !m.seen || Some(m.message_id) == last)
                                .map(|m| m.message_id)
                                .collect(),
                        ));
                    }
                    view! { <ThreadBody thread=t open=open downloaded=downloaded /> }.into_any()
                }
                Err(e) => view! { <div class="reading-empty">{format!("Error: {e}")}</div> }.into_any(),
            })}
        </Transition>
    }
}

#[component]
fn ThreadBody(
    thread: ThreadDto,
    open: RwSignal<Option<Vec<i64>>>,
    downloaded: RwSignal<u32>,
) -> impl IntoView {
    let subject = if thread.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        thread.subject.clone()
    };
    view! {
        <div class="thread-wrap" data-field="thread" data-thread-id=thread.thread_id.to_string()>
            <div class="read-subject">{subject}</div>
            {(!thread.todos.is_empty()).then(|| view! {
                <div class="thread-todos">
                    {thread.todos.iter().map(|t| view! {
                        <A href="/" attr:class="pill action" attr:data-action-id=t.action_id.to_string()>
                            {format!("\u{25CE} {}", t.title)}
                        </A>
                    }).collect_view()}
                </div>
            })}
            {thread.messages.into_iter().map(|m| view! {
                <MessageCard msg=m open=open downloaded=downloaded />
            }).collect_view()}
        </div>
    }
}

#[component]
fn MessageCard(
    msg: ThreadMessageDto,
    open: RwSignal<Option<Vec<i64>>>,
    downloaded: RwSignal<u32>,
) -> impl IntoView {
    let id = msg.message_id;
    let tick = mail_tick();
    let seen = RwSignal::new(msg.seen);
    let expanded = move || open.get().is_some_and(|ids| ids.contains(&id));
    let toggle = move |_| {
        open.update(|ids| {
            let ids = ids.get_or_insert_with(Vec::new);
            match ids.iter().position(|i| *i == id) {
                Some(p) => {
                    ids.remove(p);
                }
                None => ids.push(id),
            }
        })
    };
    // Unread and open for a moment: it's been read.
    Effect::new(move |_| {
        if expanded() && !seen.get_untracked() {
            set_timeout(
                move || {
                    // The card may be gone by now (another thread opened).
                    let still_open = open
                        .try_get_untracked()
                        .flatten()
                        .is_some_and(|ids| ids.contains(&id));
                    if still_open && seen.try_get_untracked() == Some(false) {
                        seen.set(true);
                        spawn_local(async move {
                            let _ = call::<()>(
                                "mark_seen",
                                SeenArgs {
                                    message_ids: vec![id],
                                    seen: true,
                                },
                            )
                            .await;
                            tick.update(|v| *v += 1);
                        });
                    }
                },
                std::time::Duration::from_millis(READ_AFTER_MS),
            );
        }
    });
    let more_open = RwSignal::new(false);
    let mark_unread = move |_| {
        seen.set(false);
        open.update(|ids| {
            if let Some(ids) = ids {
                ids.retain(|i| *i != id);
            }
        });
        spawn_local(async move {
            let _ = call::<()>(
                "mark_seen",
                SeenArgs {
                    message_ids: vec![id],
                    seen: false,
                },
            )
            .await;
            tick.update(|v| *v += 1);
        });
    };

    let name = if msg.from_me {
        "Me".to_string()
    } else {
        msg.from_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .or_else(|| msg.from_address.clone())
            .unwrap_or_else(|| "Unknown".to_string())
    };
    let initials = crate::components::initials_of(&name);
    let address = msg.from_address.clone().unwrap_or_default();
    let when = crate::components::email_full_date(msg.posted_at);
    let recipients = {
        let mut parts = Vec::new();
        if !msg.to.is_empty() {
            parts.push(format!("To: {}", msg.to.join(", ")));
        }
        if !msg.cc.is_empty() {
            parts.push(format!("Cc: {}", msg.cc.join(", ")));
        }
        parts.join(" · ")
    };
    let folders = msg.folders.join(", ");
    let snippet = msg.snippet.clone();

    view! {
        <div class="msg-card" class:open=expanded class:unread=move || !seen.get()
            data-message-id=id.to_string()>
            <div class="msg-head" on:click=toggle>
                <span class="avatar">{initials}</span>
                <span class="msg-who">
                    <span class="name">{name}</span>
                    <Show when=move || !expanded() fallback=move || view! {
                        <span class="addr">{address.clone()}</span>
                    }>
                        <span class="snip">{snippet.clone()}</span>
                    </Show>
                </span>
                <span class="when" title=folders>{when}</span>
            </div>
            <Show when=expanded fallback=|| ()>
                {(!recipients.is_empty()).then(|| view! { <div class="msg-to">{recipients.clone()}</div> })}
                <div class="msg-tools">
                    <button class="btn btn-ghost btn-sm btn-icon" data-action="reply" title="Reply"
                        on:click=move |_| compose(DraftKind::Reply, Some(id))>{icon(REPLY)}</button>
                    <button class="btn btn-ghost btn-sm btn-icon" data-action="forward" title="Forward"
                        on:click=move |_| compose(DraftKind::Forward, Some(id))>{icon(FORWARD)}</button>
                    <TalkAboutButton kind="message" id=id compact=true />
                    <ToDoButton message_id=id />
                    <span class="spacer"></span>
                    <span class="more-wrap">
                        <button class="btn btn-ghost btn-sm more-button" data-action="more" title="More"
                            on:click=move |_| more_open.update(|o| *o = !*o)>"\u{22EF}"</button>
                        <Show when=move || more_open.get() fallback=|| ()>
                            <div class="menu-backdrop" on:click=move |_| more_open.set(false)></div>
                            <div class="menu" on:click=move |_| more_open.set(false)>
                                <button class="btn btn-ghost" data-action="reply-all"
                                    on:click=move |_| compose(DraftKind::ReplyAll, Some(id))>"Reply all"</button>
                                <button class="btn btn-ghost" data-action="mark-unread" on:click=mark_unread>
                                    "Mark unread"
                                </button>
                            </div>
                        </Show>
                    </span>
                </div>
                <MessageBody message_id=id downloaded=downloaded />
            </Show>
        </div>
    }
}

/// "To-do": the agent writes a to-do from the email, which then shows atop
/// the thread.
#[component]
fn ToDoButton(message_id: i64) -> impl IntoView {
    let SyncTick(sync_tick) = use_context::<SyncTick>().expect("sync tick context");
    let changed = use_context::<ThreadTodosChanged>().map(|c| c.0);
    let working = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let on_click = move |_| {
        working.set(true);
        error.set(None);
        spawn_local(async move {
            match call::<i64>("to_do_from_message", MessageIdArgs { message_id }).await {
                Ok(_) => {
                    if let Some(changed) = changed {
                        changed.update(|v| *v += 1);
                    }
                    sync_tick.update(|v| *v += 1);
                }
                Err(e) => error.set(Some(e)),
            }
            working.set(false);
        });
    };
    view! {
        <button class="btn btn-ghost btn-sm" data-action="to-do" title="Make this email a to-do"
            disabled=move || working.get() on:click=on_click>
            {icon(TODO)}
            {move || if working.get() { " Writing to-do\u{2026}" } else { " To-do" }}
        </button>
        {move || error.get().map(|e| view! {
            <span class="msg-error" title=e.clone()>"Couldn't make a to-do"</span>
        })}
    }
}

// Line icons (24-unit grid, drawn in the text colour).
const REPLY: &str = "M9 17l-5-5 5-5M20 18v-2a4 4 0 0 0-4-4H4";
const FORWARD: &str = "M15 17l5-5-5-5M4 18v-2a4 4 0 0 1 4-4h12";
pub(crate) const CHAT: &str = "M7.9 20A9 9 0 1 0 4 16.1L2 22z";
const TODO: &str = "M9 11l3 3L22 4M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11";

pub(crate) fn icon(path: &'static str) -> impl IntoView {
    view! {
        <svg class="icon" viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor"
            stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d=path />
        </svg>
    }
}

/// A message's page in a sandboxed frame: no scripts run in it (the
/// sandbox and the page's own policy both forbid them), and every link goes
/// to the default browser instead of loading in the frame.
#[component]
fn MessageBody(message_id: i64, downloaded: RwSignal<u32>) -> impl IntoView {
    let allow_remote = RwSignal::new(false);
    let rendered = LocalResource::new(move || {
        let _ = downloaded.get();
        let allow_remote = allow_remote.get();
        call::<RenderedMessageDto>(
            "render_message",
            RenderArgs {
                message_id,
                allow_remote,
            },
        )
    });
    let frame: NodeRef<leptos::html::Iframe> = NodeRef::new();
    let fit = move || {
        if let Some(frame) = frame.get_untracked() {
            fit_frame(&frame);
        }
    };
    let _resize = window_event_listener(leptos::ev::resize, move |_| fit());
    view! {
        <Transition fallback=|| view! { <div class="msg-note">"Loading\u{2026}"</div> }>
            {move || rendered.get().map(|res| match res {
                Ok(r) => {
                    let remote = r.remote_resources;
                    view! {
                        {(!r.complete).then(|| view! {
                            <div class="msg-note" data-field="not-downloaded">
                                "Not downloaded yet \u{2014} showing the text."
                            </div>
                        })}
                        {(remote > 0 && !allow_remote.get_untracked()).then(|| view! {
                            <div class="msg-note">
                                {format!("{remote} remote image(s) blocked. ")}
                                <button class="btn btn-ghost btn-sm" data-action="load-images"
                                    on:click=move |_| allow_remote.set(true)>"Load images"</button>
                            </div>
                        })}
                        <iframe class="msg-frame" node_ref=frame sandbox="allow-same-origin"
                            srcdoc=r.html.clone()
                            on:load=move |ev| {
                                if let Some(f) = ev.target().and_then(|t| t.dyn_into::<HtmlIFrameElement>().ok()) {
                                    wire_frame(&f);
                                }
                            }
                        ></iframe>
                        {(!r.attachments.is_empty()).then(|| view! {
                            <div class="msg-attachments">
                                {r.attachments.iter().map(|a| {
                                    let part = a.part;
                                    let label = format!("\u{1F4CE} {} ({})", a.name, size_label(a.size));
                                    view! {
                                        <button class="btn btn-secondary btn-sm" data-action="open-attachment"
                                            on:click=move |_| spawn_local(async move {
                                                if let Err(e) = call::<()>("open_attachment", AttachmentArgs { message_id, part }).await {
                                                    gloo_console::warn!(format!("couldn't open the attachment: {e}"));
                                                }
                                            })>{label}</button>
                                    }
                                }).collect_view()}
                            </div>
                        })}
                    }.into_any()
                }
                Err(e) => view! { <div class="msg-note">{format!("Couldn't show the message: {e}")}</div> }.into_any(),
            })}
        </Transition>
    }
}

/// Size the frame to its page, keep it sized as images arrive, and send
/// link clicks to the default browser. A sandboxed frame without scripts can
/// still navigate itself, so every click on a link is stopped here.
fn wire_frame(frame: &HtmlIFrameElement) {
    fit_frame(frame);
    let Some(doc) = frame.content_document() else {
        return;
    };
    let on_click = Closure::<dyn FnMut(MouseEvent)>::new(|ev: MouseEvent| {
        let Some(anchor) = ev
            .target()
            .and_then(|t| t.dyn_into::<Element>().ok())
            .and_then(|el| el.closest("a").ok().flatten())
        else {
            return;
        };
        ev.prevent_default();
        if let Some(href) = anchor.get_attribute("href")
            && is_safe_href(&href)
        {
            spawn_local(async move {
                let _ = open_external(href).await;
            });
        }
    });
    let refit = {
        let frame = frame.clone();
        Closure::<dyn FnMut()>::new(move || fit_frame(&frame))
    };
    let _ = doc.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
    // Images load after the page: `load` doesn't bubble, so listen while
    // capturing.
    let _ =
        doc.add_event_listener_with_callback_and_bool("load", refit.as_ref().unchecked_ref(), true);
    // The listeners live as long as the frame's page; the page is replaced
    // when the message renders again.
    on_click.forget();
    refit.forget();
}

fn fit_frame(frame: &HtmlIFrameElement) {
    if let Some(root) = frame.content_document().and_then(|d| d.document_element()) {
        let height = root.scroll_height().max(40);
        let _ = frame.set_attribute("height", &height.to_string());
    }
}

fn size_label(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} B"),
    }
}
