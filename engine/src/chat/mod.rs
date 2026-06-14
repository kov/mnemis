//! Interactive chat agent (Phase 4): persistent, tool-using conversations with
//! the local model.
//!
//! The loop mirrors extraction's (client-side history reconstruction, shared
//! stall-retry from [`crate::agent`]) but is purpose-built for chat: it
//! **persists every turn to SQLite before streaming the matching event** to the
//! UI, captures the model's reasoning for display (never replaying it), and
//! rebuilds the model input from the persisted turns each iteration — so the DB
//! is the single source of truth and the channel is a display accelerator.

mod compact;
pub mod prompt;
pub mod store;
mod tools;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use mnemis_types::ChatEvent;

use crate::agent;
use crate::extract::tools::{FetchBudget, chat_definitions};
use crate::extract::trace::TraceWriter;
use crate::llm::{ContentItem, InputItem, LlmTransport, OutputItem, Role};

/// Max model turns for a single user message before we bail (tool-call loop
/// guard). A chat answer rarely needs more than a couple of tool round-trips.
const MAX_CHAT_TURNS: usize = 12;

/// How many times one send may be retried after a context-overflow rejection,
/// compacting harder each time. A small bound: each retry folds more of the
/// conversation away, so if it doesn't fit within a couple of folds a single
/// message is genuinely too big for the window.
const MAX_COMPACT_RETRIES: usize = 3;

/// Where the agent loop pushes streamed events. The app adapts a
/// `tauri::ipc::Channel<ChatEvent>` to this; the CLI prints them; tests collect
/// them. A trait object keeps the engine Tauri-agnostic.
pub type ChatSink<'a> = &'a (dyn Fn(ChatEvent) + Send + Sync);

/// Create a chat (optionally seeded from an entity) and, for a seeded chat,
/// title it up front from its seed label so the list reads "action A-12 ·
/// Renew the cert" instead of "(new chat)". Blank chats stay untitled here;
/// they get a model-generated title after their first turn via
/// [`maybe_generate_title`].
pub async fn create_chat(
    pool: &SqlitePool,
    seeded_from_kind: Option<&str>,
    seeded_from_id: Option<i64>,
) -> Result<i64> {
    let id = store::create_chat(pool, seeded_from_kind, seeded_from_id).await?;
    if seeded_from_kind.is_some()
        && let Some(label) = prompt::seed_label(pool, id).await?
    {
        store::set_title(pool, id, &label).await?;
    }
    Ok(id)
}

/// Run one user message through the chat agent: persist it, then loop the model
/// and its tools until it answers, persisting **every** turn before streaming
/// the matching event to `sink`. Emits `Done` on a clean finish, or `Error`
/// (and returns the error) on failure — so a caller that only watches the sink
/// still learns the outcome.
#[allow(clippy::too_many_arguments)]
pub async fn run_chat_turn(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    system_prompt: &str,
    chat_id: i64,
    user_text: &str,
    fetch_budget_chars: usize,
    max_context_tokens: usize,
    idle_timeout: Duration,
    cancel: Option<CancellationToken>,
    sink: ChatSink<'_>,
    traces_dir: Option<&Path>,
) -> Result<()> {
    // One JSONL trace per user message, alongside the extraction traces, so a
    // "the model had trouble" report can be read back line-by-line.
    let mut trace =
        traces_dir.map(|dir| TraceWriter::open_chat(dir, Utc::now().timestamp(), chat_id));
    // No Stop button (CLI / tests) → a token that's never fired.
    let cancel = cancel.unwrap_or_default();
    match run_inner(
        pool,
        llm,
        system_prompt,
        chat_id,
        user_text,
        fetch_budget_chars,
        max_context_tokens,
        idle_timeout,
        cancel,
        sink,
        trace.as_mut(),
    )
    .await
    {
        Ok(()) => {
            sink(ChatEvent::Done);
            Ok(())
        }
        Err(e) => {
            if let Some(w) = trace.as_mut() {
                w.agent_error(&format!("{e:#}"));
            }
            sink(ChatEvent::Error {
                message: format!("{e:#}"),
            });
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    system_prompt: &str,
    chat_id: i64,
    user_text: &str,
    fetch_budget_chars: usize,
    max_context_tokens: usize,
    idle_timeout: Duration,
    cancel: CancellationToken,
    sink: ChatSink<'_>,
    mut trace: Option<&mut TraceWriter>,
) -> Result<()> {
    // Persist the user's message first; everything else reconstructs from the DB.
    store::append_turn(pool, chat_id, "user", Some(user_text), None, None, None).await?;
    store::ensure_title(pool, chat_id, user_text).await?;

    let tool_defs = chat_definitions();
    let budgets = compact::Budgets::new(max_context_tokens);
    let mut fetch_budget = FetchBudget::new(fetch_budget_chars);
    if let Some(w) = trace.as_deref_mut() {
        w.system_prompt(system_prompt);
        w.tools(&tool_defs);
    }

    // The streaming controls for every send this turn: the user's Stop token,
    // and a channel onto which the transport pushes assistant output-text
    // deltas. The pump loop around each send forwards those to the UI.
    let (delta_tx, mut delta_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let stream = agent::StreamCtx {
        idle_timeout,
        cancel: cancel.clone(),
        deltas: Some(delta_tx),
    };

    for step in 0..MAX_CHAT_TURNS {
        // A Stop pressed between sends (e.g. during a tool dispatch) ends the
        // turn cleanly here rather than firing one more model call.
        if cancel.is_cancelled() {
            store::touch_chat(pool, chat_id).await?;
            return Ok(());
        }

        // Keep the conversation within the model's context window: fold older
        // turns into a running summary if we're near the limit, then rebuild
        // the model input from what's persisted (excludes reasoning;
        // `history_for_send` also drops the summary on a turn whose verbatim
        // tail alone is near the limit).
        if let compact::Compacted::Cancelled = compact::maybe_compact(
            pool,
            llm,
            chat_id,
            system_prompt,
            &tool_defs,
            budgets,
            idle_timeout,
            &cancel,
            sink,
            trace.as_deref_mut(),
        )
        .await?
        {
            // Stop pressed during the "Condensing…" phase: end the turn cleanly.
            store::touch_chat(pool, chat_id).await?;
            return Ok(());
        }
        let mut history =
            compact::history_for_send(pool, chat_id, system_prompt, &tool_defs, budgets).await?;

        trace!(step, chat_id, "chat LLM: send");
        let started = std::time::Instant::now();

        // Send, with a reactive backstop: if the server still rejects the
        // prompt as oversize (our estimate was off, or a tool pulled a lot in
        // the previous turn), compact harder and retry. If there's nothing left
        // to fold, a single message is genuinely too big for this window.
        let mut compact_retries = 0usize;
        let response = loop {
            if let Some(w) = trace.as_deref_mut() {
                w.llm_send(step, &history, None);
            }
            // Run the send while pumping streamed deltas to the UI on the same
            // task (no spawn → `sink` stays usable inline). The select resolves
            // when the send finishes; we then flush any deltas it left buffered.
            // Scoped so the future's borrow of `history` ends before the
            // overflow arm below can rebuild `history`.
            let sent = {
                let send_fut = agent::send_with_stall_retry(
                    llm,
                    system_prompt,
                    &history,
                    &tool_defs,
                    step,
                    Some(&stream),
                    |_| {},
                );
                tokio::pin!(send_fut);
                loop {
                    tokio::select! {
                        r = &mut send_fut => break r,
                        Some(delta) = delta_rx.recv() => sink(ChatEvent::Delta { text: delta }),
                    }
                }
            };
            while let Ok(delta) = delta_rx.try_recv() {
                sink(ChatEvent::Delta { text: delta });
            }
            match sent {
                Ok(r) => break r,
                Err(e) => {
                    let chain = format!("{e:#}");
                    if agent::is_context_overflow(&chain) {
                        if compact_retries >= MAX_COMPACT_RETRIES {
                            anyhow::bail!(
                                "This conversation is too large for this model's \
                                 {max_context_tokens}-token context window, even after \
                                 condensing it. Start a new chat, or point mnemis at a model \
                                 with a larger context window."
                            );
                        }
                        compact_retries += 1;
                        warn!(
                            step,
                            chat_id, "chat: send overflowed; compacting and retrying"
                        );
                        match compact::compact(
                            pool,
                            llm,
                            chat_id,
                            system_prompt,
                            &tool_defs,
                            budgets,
                            idle_timeout,
                            &cancel,
                            sink,
                            trace.as_deref_mut(),
                        )
                        .await?
                        {
                            compact::Compacted::Folded => {
                                history = compact::history_for_send(
                                    pool,
                                    chat_id,
                                    system_prompt,
                                    &tool_defs,
                                    budgets,
                                )
                                .await?;
                                continue;
                            }
                            compact::Compacted::Cancelled => {
                                // Stop pressed while compacting the oversize prompt.
                                store::touch_chat(pool, chat_id).await?;
                                return Ok(());
                            }
                            compact::Compacted::NothingToFold | compact::Compacted::Stuck => {
                                anyhow::bail!(
                                    "This conversation has a message too large to fit this \
                                     model's {max_context_tokens}-token context window. Start a \
                                     new chat, or use a model with a larger context window."
                                );
                            }
                        }
                    }
                    return Err(e);
                }
            }
        };
        if let Some(w) = trace.as_deref_mut() {
            w.llm_recv(step, started.elapsed().as_secs(), &response);
        }
        let response_id = (!response.id.is_empty()).then(|| response.id.clone());

        // Walk the output in order, persisting then emitting. Reasoning rides on
        // the first assistant turn (message or tool call) it precedes.
        //
        // A response that also makes a tool call may carry a `message` item: that
        // text is a *preamble* — the model narrating its plan before acting, not
        // an answer to the user. We fold it into the reasoning so it renders in
        // the reasoning block (before the tool call) and is never replayed,
        // rather than surfacing as a standalone assistant bubble.
        let has_tool_call = response
            .output
            .iter()
            .any(|i| matches!(i, OutputItem::FunctionCall { .. }));
        let mut function_calls = Vec::new();
        let mut pending_reasoning: Option<String> = None;
        let mut reasoning_attached = false;

        for item in response.output {
            match item {
                OutputItem::Reasoning { summary } => {
                    let text = summary
                        .iter()
                        .map(|s| s.text.as_str())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        pending_reasoning = Some(match pending_reasoning.take() {
                            Some(prev) => format!("{prev}\n{text}"),
                            None => text,
                        });
                    }
                }
                OutputItem::Message { content } => {
                    let mut text = String::new();
                    for c in content {
                        if let ContentItem::OutputText { text: t } = c {
                            text.push_str(&t);
                        }
                    }
                    if text.is_empty() {
                        continue;
                    }
                    if has_tool_call {
                        // Preamble alongside a tool call → fold into reasoning.
                        pending_reasoning = Some(match pending_reasoning.take() {
                            Some(prev) => format!("{prev}\n{text}"),
                            None => text,
                        });
                    } else {
                        let turn_id = store::append_turn(
                            pool,
                            chat_id,
                            "assistant",
                            Some(&text),
                            None,
                            None,
                            response_id.as_deref(),
                        )
                        .await?;
                        attach_reasoning(
                            pool,
                            turn_id,
                            &mut pending_reasoning,
                            &mut reasoning_attached,
                            sink,
                        )
                        .await?;
                        sink(ChatEvent::AssistantMessage { text });
                    }
                }
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    let turn_id = store::append_turn(
                        pool,
                        chat_id,
                        "assistant",
                        Some(&arguments),
                        Some(&name),
                        Some(&call_id),
                        response_id.as_deref(),
                    )
                    .await?;
                    attach_reasoning(
                        pool,
                        turn_id,
                        &mut pending_reasoning,
                        &mut reasoning_attached,
                        sink,
                    )
                    .await?;
                    sink(ChatEvent::ToolCall {
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    function_calls.push((call_id, name, arguments));
                }
                OutputItem::Unknown => {}
            }
        }

        // Reasoning with no assistant turn to ride on: park it on a bare turn so
        // it isn't lost (rare — the model emitted only reasoning).
        if pending_reasoning.is_some() && !reasoning_attached {
            let turn_id =
                store::append_turn(pool, chat_id, "assistant", None, None, None, None).await?;
            attach_reasoning(
                pool,
                turn_id,
                &mut pending_reasoning,
                &mut reasoning_attached,
                sink,
            )
            .await?;
        }

        // No tool calls → the model has answered. Done.
        if function_calls.is_empty() {
            store::touch_chat(pool, chat_id).await?;
            return Ok(());
        }

        // Run the tools, persist each result, stream it. The next iteration
        // rebuilds history from the DB (now including these tool turns).
        for (call_id, name, arguments) in function_calls {
            debug!(name = %name, chat_id, "chat dispatching tool");
            let out = tools::dispatch(pool, chat_id, &mut fetch_budget, &name, &arguments).await;
            if let Some(w) = trace.as_deref_mut() {
                w.tool_dispatch(step, &call_id, &name, &arguments, &out.output);
            }
            store::append_turn(
                pool,
                chat_id,
                "tool",
                Some(&out.output),
                Some(&name),
                Some(&call_id),
                None,
            )
            .await?;
            sink(ChatEvent::ToolResult {
                name,
                output: out.output,
            });
        }
        store::touch_chat(pool, chat_id).await?;
    }

    anyhow::bail!("chat agent exceeded {MAX_CHAT_TURNS} turns")
}

/// Persist any pending reasoning onto `turn_id` (once per model response) and
/// emit it. No-op after the first attachment in a response.
async fn attach_reasoning(
    pool: &SqlitePool,
    turn_id: i64,
    pending: &mut Option<String>,
    attached: &mut bool,
    sink: ChatSink<'_>,
) -> Result<()> {
    if *attached {
        return Ok(());
    }
    if let Some(text) = pending.take() {
        store::append_reasoning(pool, turn_id, &text).await?;
        *attached = true;
        sink(ChatEvent::Reasoning { text });
    }
    Ok(())
}

/// After a turn, upgrade an unseeded chat's provisional title (the truncated
/// first message) to a short model-generated one. A no-op for seeded chats
/// (already labelled) and for any chat whose title has already moved past the
/// provisional — so it runs exactly once, on the first exchange, regardless of
/// how many tool round-trips that exchange took.
///
/// `first_user_text` is the message that opened the chat. The caller runs this
/// **after** `run_chat_turn` (so the answer is already streamed and the channel
/// has closed) and treats any error as best-effort: the provisional title
/// simply stays.
pub async fn maybe_generate_title(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    chat_id: i64,
    first_user_text: &str,
) -> Result<()> {
    if store::is_seeded(pool, chat_id).await? {
        return Ok(());
    }
    // Only act while the title is still the provisional first-message title;
    // once it's a generated title (or the user has sent later messages) leave
    // it alone.
    let current = store::current_title(pool, chat_id).await?;
    if current.as_deref() != Some(store::title_from(first_user_text).as_str()) {
        return Ok(());
    }
    let title = generate_title(llm, first_user_text).await?;
    store::set_title(pool, chat_id, &title).await
}

/// One-shot, tool-less call asking the model for a short conversation title.
async fn generate_title(llm: &dyn LlmTransport, user_text: &str) -> Result<String> {
    const SYSTEM: &str = "You name conversations. Given the user's opening message, reply with a \
        short, specific title of 3 to 6 words so they can find the conversation later. Reply with \
        the title only — no surrounding quotes, no trailing punctuation, no \"Title:\" prefix.";
    let history = vec![InputItem::Message {
        role: Role::User,
        content: user_text.to_string(),
    }];
    let response =
        agent::send_with_stall_retry(llm, SYSTEM, &history, &[], 0, None, |_| {}).await?;
    let mut title = String::new();
    for item in response.output {
        if let OutputItem::Message { content } = item {
            for c in content {
                if let ContentItem::OutputText { text } = c {
                    title.push_str(&text);
                }
            }
        }
    }
    let title = sanitize_title(&title);
    if title.is_empty() {
        anyhow::bail!("model returned an empty title");
    }
    Ok(title)
}

/// Pull a clean title out of the model's reply: first non-empty line, minus a
/// stray "Title:" prefix or wrapping quotes, clamped to the usual length.
fn sanitize_title(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line
        .strip_prefix("Title:")
        .or_else(|| line.strip_prefix("title:"))
        .unwrap_or(line)
        .trim()
        .trim_matches('"')
        .trim();
    store::title_from(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::llm::{ReasoningSummary, Role};
    use crate::test_util::{SeedMessage, mock, seed_messages, seed_minimal};
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Collects streamed events for assertions.
    fn collector() -> (
        std::sync::Arc<Mutex<Vec<ChatEvent>>>,
        impl Fn(ChatEvent) + Send + Sync,
    ) {
        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let sink = move |e: ChatEvent| sink_events.lock().unwrap().push(e);
        (events, sink)
    }

    #[tokio::test]
    async fn streams_reasoning_tool_and_answer_persisting_each() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let ctx = seed_minimal(&pool).await.unwrap();
        seed_messages(
            &pool,
            ctx.source_id,
            ctx.channel_id,
            &[SeedMessage {
                external_id: "m1",
                author_email: "ana@example.com",
                author_name: "Ana",
                subject: "Renewal",
                body: "Please renew the cert before Friday.",
                recipients: &[],
            }],
        )
        .await
        .unwrap();

        // Turn 0: reasoning + a fetch_messages tool call. Turn 1: final answer.
        let llm = crate::test_util::MockLlm::new(vec![
            mock::turn(vec![
                OutputItem::Reasoning {
                    summary: vec![ReasoningSummary {
                        text: "I should read m1 first".to_string(),
                    }],
                },
                OutputItem::FunctionCall {
                    call_id: "c1".to_string(),
                    name: "fetch_messages".to_string(),
                    arguments: serde_json::json!({ "external_ids": ["m1"] }).to_string(),
                },
            ]),
            mock::no_tools("Ana asked you to renew the cert before Friday."),
        ]);

        let chat = store::create_chat(&pool, None, None).await.unwrap();
        let (events, sink) = collector();
        run_chat_turn(
            &pool,
            &llm,
            "system",
            chat,
            "what does Ana need?",
            100_000,
            1_000_000,
            Duration::from_secs(60),
            None,
            &sink,
            None,
        )
        .await
        .unwrap();

        // Event order: reasoning → tool call → tool result → answer → done.
        let kinds: Vec<&str> = events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                ChatEvent::Delta { .. } => "delta",
                ChatEvent::Reasoning { .. } => "reasoning",
                ChatEvent::AssistantMessage { .. } => "assistant",
                ChatEvent::ToolCall { .. } => "tool_call",
                ChatEvent::ToolResult { .. } => "tool_result",
                ChatEvent::Compacting => "compacting",
                ChatEvent::Done => "done",
                ChatEvent::Error { .. } => "error",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["reasoning", "tool_call", "tool_result", "assistant", "done"],
            "events: {kinds:?}"
        );

        // Persisted: user + (assistant tool-call) + tool + assistant answer.
        let turns = store::load_turns(&pool, chat).await.unwrap();
        let roles: Vec<&str> = turns.iter().map(|t| t.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "assistant"]);
        // Reasoning persisted on the tool-call turn, not the user/answer turns.
        let call = turns.iter().find(|t| t.tool_name.is_some()).unwrap();
        assert_eq!(call.reasoning.as_deref(), Some("I should read m1 first"));

        // The rebuilt history (what the model sees) never contains reasoning.
        let history = store::build_history(&pool, chat).await.unwrap();
        let dump = serde_json::to_string(
            &history
                .iter()
                .map(|i| serde_json::to_value(i).unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(
            !dump.contains("I should read m1 first"),
            "reasoning leaked into history: {dump}"
        );
        // And it does carry the user message + the fetched tool output.
        assert!(history.iter().any(
            |i| matches!(i, crate::llm::InputItem::Message { role: Role::User, content } if content.contains("Ana"))
        ));
    }

    #[tokio::test]
    async fn folds_a_preamble_message_into_reasoning_before_the_tool_call() {
        // A model that emits a *message* alongside a tool call is narrating its
        // plan, not answering. That preamble must render as reasoning (before the
        // tool call), persist on the tool-call turn, and never be replayed — not
        // surface as a standalone assistant bubble.
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let ctx = seed_minimal(&pool).await.unwrap();
        seed_messages(
            &pool,
            ctx.source_id,
            ctx.channel_id,
            &[SeedMessage {
                external_id: "m1",
                author_email: "ana@example.com",
                author_name: "Ana",
                subject: "Renewal",
                body: "Please renew the cert before Friday.",
                recipients: &[],
            }],
        )
        .await
        .unwrap();

        // Turn 0: a preamble message + a tool call (no separate reasoning item).
        // Turn 1: the final answer.
        let llm = crate::test_util::MockLlm::new(vec![
            mock::turn(vec![
                OutputItem::Message {
                    content: vec![ContentItem::OutputText {
                        text: "Let me check m1 first.".to_string(),
                    }],
                },
                OutputItem::FunctionCall {
                    call_id: "c1".to_string(),
                    name: "fetch_messages".to_string(),
                    arguments: serde_json::json!({ "external_ids": ["m1"] }).to_string(),
                },
            ]),
            mock::no_tools("Ana asked you to renew the cert before Friday."),
        ]);

        let chat = store::create_chat(&pool, None, None).await.unwrap();
        let (events, sink) = collector();
        run_chat_turn(
            &pool,
            &llm,
            "system",
            chat,
            "what does Ana need?",
            100_000,
            1_000_000,
            Duration::from_secs(60),
            None,
            &sink,
            None,
        )
        .await
        .unwrap();

        // The preamble is emitted as reasoning, before the tool call — no
        // standalone assistant message for it.
        let kinds: Vec<&str> = events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                ChatEvent::Delta { .. } => "delta",
                ChatEvent::Reasoning { .. } => "reasoning",
                ChatEvent::AssistantMessage { .. } => "assistant",
                ChatEvent::ToolCall { .. } => "tool_call",
                ChatEvent::ToolResult { .. } => "tool_result",
                ChatEvent::Compacting => "compacting",
                ChatEvent::Done => "done",
                ChatEvent::Error { .. } => "error",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["reasoning", "tool_call", "tool_result", "assistant", "done"],
            "events: {kinds:?}"
        );

        // Only one assistant *message* turn (the final answer); the preamble did
        // not become its own bubble.
        let turns = store::load_turns(&pool, chat).await.unwrap();
        let roles: Vec<&str> = turns.iter().map(|t| t.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "assistant"]);
        // The preamble landed as reasoning on the tool-call turn.
        let call = turns.iter().find(|t| t.tool_name.is_some()).unwrap();
        assert_eq!(call.reasoning.as_deref(), Some("Let me check m1 first."));

        // And it never leaks into the history the model is re-sent.
        let history = store::build_history(&pool, chat).await.unwrap();
        let dump = serde_json::to_string(
            &history
                .iter()
                .map(|i| serde_json::to_value(i).unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(
            !dump.contains("Let me check m1 first."),
            "preamble leaked into history: {dump}"
        );
    }

    #[tokio::test]
    async fn run_writes_a_jsonl_trace_when_a_traces_dir_is_given() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let ctx = seed_minimal(&pool).await.unwrap();
        seed_messages(
            &pool,
            ctx.source_id,
            ctx.channel_id,
            &[SeedMessage {
                external_id: "m1",
                author_email: "ana@example.com",
                author_name: "Ana",
                subject: "Renewal",
                body: "Please renew the cert before Friday.",
                recipients: &[],
            }],
        )
        .await
        .unwrap();

        let llm = crate::test_util::MockLlm::new(vec![
            mock::turn(vec![OutputItem::FunctionCall {
                call_id: "c1".to_string(),
                name: "fetch_messages".to_string(),
                arguments: serde_json::json!({ "external_ids": ["m1"] }).to_string(),
            }]),
            mock::no_tools("Ana asked you to renew the cert before Friday."),
        ]);

        let traces = TempDir::new().unwrap();
        let chat = store::create_chat(&pool, None, None).await.unwrap();
        let (_events, sink) = collector();
        run_chat_turn(
            &pool,
            &llm,
            "system",
            chat,
            "what does Ana need?",
            100_000,
            1_000_000,
            Duration::from_secs(60),
            None,
            &sink,
            Some(traces.path()),
        )
        .await
        .unwrap();

        // One trace file, named for this chat, holding the full transcript.
        let file = std::fs::read_dir(traces.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(&format!("-chat{chat}.jsonl")))
            })
            .expect("a -chat<id>.jsonl trace file should exist");
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();
        for want in [
            "system_prompt",
            "tools",
            "llm_send",
            "llm_recv",
            "tool_dispatch",
        ] {
            assert!(events.contains(&want), "trace missing {want}: {events:?}");
        }
        // The tool dispatch line carries the call's args + output verbatim.
        let dispatch = lines
            .iter()
            .find(|l| l["event"] == "tool_dispatch")
            .unwrap();
        assert_eq!(dispatch["name"], "fetch_messages");
        assert_eq!(dispatch["arguments"]["external_ids"][0], "m1");
        assert!(
            dispatch["output"]["messages"].is_array(),
            "fetch_messages output should be captured: {dispatch}"
        );
    }

    #[tokio::test]
    async fn generates_a_title_for_an_unseeded_chat_only_once() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();

        let chat = store::create_chat(&pool, None, None).await.unwrap();
        let first = "track that I need to renew the cert by friday";
        store::append_turn(&pool, chat, "user", Some(first), None, None, None)
            .await
            .unwrap();
        store::ensure_title(&pool, chat, first).await.unwrap();
        // Provisional title is the (clamped) first message.
        assert_eq!(
            store::current_title(&pool, chat).await.unwrap().as_deref(),
            Some(first)
        );

        let llm = crate::test_util::MockLlm::new(vec![mock::no_tools("Renew TLS certificate")]);
        maybe_generate_title(&pool, &llm, chat, first)
            .await
            .unwrap();
        assert_eq!(
            store::current_title(&pool, chat).await.unwrap().as_deref(),
            Some("Renew TLS certificate"),
            "first turn should upgrade the provisional title"
        );

        // A second call is a no-op: the title is no longer provisional, so the
        // model is never consulted (the empty mock script would panic if it
        // were).
        let llm2 = crate::test_util::MockLlm::new(vec![]);
        maybe_generate_title(&pool, &llm2, chat, "and tell me when it's due")
            .await
            .unwrap();
        assert_eq!(
            store::current_title(&pool, chat).await.unwrap().as_deref(),
            Some("Renew TLS certificate")
        );
    }

    #[tokio::test]
    async fn seeded_chat_keeps_its_label_title_without_consulting_the_model() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();

        // Seeded chat with a label title already set by `create_chat` is left
        // alone — an empty mock script proves the model is never called.
        let chat = store::create_chat(&pool, Some("action"), Some(7))
            .await
            .unwrap();
        store::set_title(&pool, chat, "action A-7 · Renew the cert")
            .await
            .unwrap();
        store::append_turn(&pool, chat, "user", Some("why?"), None, None, None)
            .await
            .unwrap();

        let llm = crate::test_util::MockLlm::new(vec![]);
        maybe_generate_title(&pool, &llm, chat, "why?")
            .await
            .unwrap();
        assert_eq!(
            store::current_title(&pool, chat).await.unwrap().as_deref(),
            Some("action A-7 · Renew the cert")
        );
    }

    #[tokio::test]
    async fn agent_can_record_an_action_from_chat() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let ctx = seed_minimal(&pool).await.unwrap();
        seed_messages(
            &pool,
            ctx.source_id,
            ctx.channel_id,
            &[SeedMessage {
                external_id: "m1",
                author_email: "ana@example.com",
                author_name: "Ana",
                subject: "Renewal",
                body: "Please renew the cert before Friday.",
                recipients: &[],
            }],
        )
        .await
        .unwrap();

        // The user tells the agent to track it; agent records high-confidence,
        // then confirms. Mutating tools run with Global scope (cross-source).
        let llm = crate::test_util::MockLlm::new(vec![
            mock::record_action("Renew the cert", "high", &["m1"]),
            mock::no_tools("Done — I've added \"Renew the cert\" and auto-claimed it."),
        ]);

        let chat = store::create_chat(&pool, None, None).await.unwrap();
        let (_events, sink) = collector();
        run_chat_turn(
            &pool,
            &llm,
            "system",
            chat,
            "track that I need to renew the cert",
            100_000,
            1_000_000,
            Duration::from_secs(60),
            None,
            &sink,
            None,
        )
        .await
        .unwrap();

        // The action landed, auto-claimed, with an audit event.
        let (count, status): (i64, String) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(MAX(status), '') FROM actions")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1, "chat agent should have created one action");
        assert_eq!(status, "auto_claimed");
        let (events,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM action_events WHERE event_kind = 'created'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(events, 1, "a 'created' audit event should be written");
    }

    /// Summarizer calls (tool-less) return a fixed recap; chat turns (with
    /// tools) return a fixed answer. Lets a compaction test run regardless of
    /// how many summarizer chunks the fold takes.
    struct ScriptedCompactLlm;

    #[async_trait::async_trait]
    impl crate::llm::LlmTransport for ScriptedCompactLlm {
        async fn send(
            &self,
            _instructions: &str,
            _input: Vec<InputItem>,
            tools: &[crate::llm::ToolDef],
            _previous_response_id: Option<&str>,
        ) -> Result<crate::llm::ResponsesResponse> {
            if tools.is_empty() {
                Ok(mock::no_tools("CONDENSED SUMMARY"))
            } else {
                Ok(mock::no_tools("Here is the answer."))
            }
        }
    }

    #[tokio::test]
    async fn compacts_a_long_conversation_and_keeps_the_full_transcript() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let chat = store::create_chat(&pool, None, None).await.unwrap();

        // Seed a long prior conversation so the very first send is over the
        // proactive compaction trigger.
        for i in 0..60 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            store::append_turn(
                &pool,
                chat,
                role,
                Some(&"context ".repeat(50)),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }
        let before = store::load_turns(&pool, chat).await.unwrap().len();

        let llm = ScriptedCompactLlm;
        let (events, sink) = collector();
        run_chat_turn(
            &pool,
            &llm,
            "system",
            chat,
            "given all that, what should I do next?",
            100_000,
            8192,
            Duration::from_secs(60),
            None,
            &sink,
            None,
        )
        .await
        .unwrap();

        // It announced the compaction and wrote a checkpoint.
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, ChatEvent::Compacting)),
            "a Compacting event should have been streamed"
        );
        assert!(
            store::latest_summary(&pool, chat).await.unwrap().is_some(),
            "a summary checkpoint should have been written"
        );

        // The model now sees a compact history led by the summary, not all the
        // raw turns.
        let history = store::build_history(&pool, chat).await.unwrap();
        assert!(
            matches!(&history[0], InputItem::Message { content, .. } if content.contains("CONDENSED SUMMARY")),
            "history should lead with the summary"
        );
        assert!(
            history.len() < before,
            "compacted history ({}) should be smaller than the {before} raw turns",
            history.len()
        );

        // But the UI transcript still has everything: the seeded turns + the new
        // user message + the assistant answer.
        assert_eq!(
            store::load_turns(&pool, chat).await.unwrap().len(),
            before + 2,
            "the full transcript must survive compaction"
        );
    }

    /// Overflows the first chat send, then answers once the conversation has
    /// been compacted. Summarizer calls (tool-less) always succeed.
    struct OverflowThenAnswerLlm {
        chat_calls: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl crate::llm::LlmTransport for OverflowThenAnswerLlm {
        async fn send(
            &self,
            _instructions: &str,
            _input: Vec<InputItem>,
            tools: &[crate::llm::ToolDef],
            _previous_response_id: Option<&str>,
        ) -> Result<crate::llm::ResponsesResponse> {
            if tools.is_empty() {
                return Ok(mock::no_tools("CONDENSED"));
            }
            let mut n = self.chat_calls.lock().unwrap();
            *n += 1;
            if *n == 1 {
                anyhow::bail!(
                    "LLM API error (HTTP 400 Bad Request): Prompt too long: 40000 tokens \
                     exceeds max context window of 32768 tokens"
                );
            }
            Ok(mock::no_tools("answer after compaction"))
        }
    }

    #[tokio::test]
    async fn reactive_compaction_recovers_from_an_overflow_rejection() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let chat = store::create_chat(&pool, None, None).await.unwrap();

        // A history under the proactive trigger but big enough to have a
        // foldable prefix, so the *overflow* (not the estimate) drives the fold.
        for i in 0..12 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            store::append_turn(
                &pool,
                chat,
                role,
                Some(&"context-word ".repeat(80)),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }
        let before = store::load_turns(&pool, chat).await.unwrap().len();

        let llm = OverflowThenAnswerLlm {
            chat_calls: std::sync::Mutex::new(0),
        };
        let (events, sink) = collector();
        run_chat_turn(
            &pool,
            &llm,
            "system",
            chat,
            "what's the status?",
            100_000,
            8192,
            Duration::from_secs(60),
            None,
            &sink,
            None,
        )
        .await
        .unwrap();

        {
            let events = events.lock().unwrap();
            assert!(
                events.iter().any(|e| matches!(e, ChatEvent::Compacting)),
                "the 400 should have triggered a compaction"
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, ChatEvent::AssistantMessage { text } if text.contains("answer after compaction"))),
                "the retry after compaction should have answered"
            );
        }
        assert!(store::latest_summary(&pool, chat).await.unwrap().is_some());
        assert_eq!(
            store::load_turns(&pool, chat).await.unwrap().len(),
            before + 2,
            "transcript intact after the reactive fold"
        );
    }

    /// Streams output-text deltas over the `deltas` sink, then answers — and can
    /// self-cancel after the first delta to simulate the user pressing Stop
    /// mid-generation (returning the partial as a `cancelled` response).
    struct StreamingMockLlm {
        deltas: Vec<String>,
        final_text: String,
        stop_after_first_delta: bool,
    }

    #[async_trait::async_trait]
    impl crate::llm::LlmTransport for StreamingMockLlm {
        async fn send(
            &self,
            _instructions: &str,
            _input: Vec<InputItem>,
            _tools: &[crate::llm::ToolDef],
            _previous_response_id: Option<&str>,
        ) -> Result<crate::llm::ResponsesResponse> {
            Ok(mock::no_tools(&self.final_text))
        }

        async fn send_stream(
            &self,
            _instructions: &str,
            _input: Vec<InputItem>,
            _tools: &[crate::llm::ToolDef],
            _previous_response_id: Option<&str>,
            _idle_timeout: std::time::Duration,
            cancel: CancellationToken,
            deltas: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        ) -> Result<crate::llm::ResponsesResponse> {
            let mut acc = String::new();
            for (i, d) in self.deltas.iter().enumerate() {
                acc.push_str(d);
                if let Some(tx) = &deltas {
                    let _ = tx.send(d.clone());
                }
                if self.stop_after_first_delta && i == 0 {
                    cancel.cancel();
                    return Ok(partial_cancelled(acc));
                }
            }
            if cancel.is_cancelled() {
                return Ok(partial_cancelled(acc));
            }
            Ok(mock::no_tools(&self.final_text))
        }
    }

    /// A `cancelled`-status response carrying the partial assistant text — what
    /// the real transport returns on Stop.
    fn partial_cancelled(text: String) -> crate::llm::ResponsesResponse {
        crate::llm::ResponsesResponse {
            id: String::new(),
            status: "cancelled".to_string(),
            output: vec![OutputItem::Message {
                content: vec![ContentItem::OutputText { text }],
            }],
        }
    }

    #[tokio::test]
    async fn streams_output_text_deltas_then_the_answer() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let chat = store::create_chat(&pool, None, None).await.unwrap();

        let llm = StreamingMockLlm {
            deltas: vec!["Hel".to_string(), "lo".to_string()],
            final_text: "Hello".to_string(),
            stop_after_first_delta: false,
        };
        let (events, sink) = collector();
        run_chat_turn(
            &pool,
            &llm,
            "system",
            chat,
            "hi",
            100_000,
            1_000_000,
            Duration::from_secs(60),
            None,
            &sink,
            None,
        )
        .await
        .unwrap();

        let events = events.lock().unwrap();
        // The deltas were forwarded live, in order...
        let streamed: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "Hello", "deltas should stream in order");
        // ...and the completed answer still arrived as a persisted message.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ChatEvent::AssistantMessage { text } if text == "Hello")),
            "the final answer should be emitted: {events:?}"
        );
    }

    #[tokio::test]
    async fn stop_keeps_the_partial_answer_and_ends_cleanly() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let chat = store::create_chat(&pool, None, None).await.unwrap();

        // Streams one chunk, then Stop fires mid-generation.
        let llm = StreamingMockLlm {
            deltas: vec!["partial answer".to_string()],
            final_text: "this full answer should never appear".to_string(),
            stop_after_first_delta: true,
        };
        let (events, sink) = collector();
        run_chat_turn(
            &pool,
            &llm,
            "system",
            chat,
            "go",
            100_000,
            1_000_000,
            Duration::from_secs(60),
            Some(CancellationToken::new()),
            &sink,
            None,
        )
        .await
        .expect("a Stop is a clean end, not an error");

        {
            let events = events.lock().unwrap();
            // No error, and the un-generated full answer never leaked.
            assert!(
                !events.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
                "Stop should not surface as an error: {events:?}"
            );
            assert!(
                !events.iter().any(
                    |e| matches!(e, ChatEvent::AssistantMessage { text } if text.contains("never appear"))
                ),
                "the cancelled full answer must not appear"
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, ChatEvent::AssistantMessage { text } if text == "partial answer")),
                "the partial that streamed should be kept: {events:?}"
            );
        }

        // The partial is persisted as the assistant turn (user + assistant).
        let turns = store::load_turns(&pool, chat).await.unwrap();
        let roles: Vec<&str> = turns.iter().map(|t| t.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        assert_eq!(
            turns.last().unwrap().content.as_deref(),
            Some("partial answer")
        );
    }

    /// Streams one delta, signals that it's mid-flight, then parks on the cancel
    /// token — modelling a real send that's still waiting on the server when the
    /// user hits Stop. Returns the partial once the *external* token fires.
    struct BlockingStreamLlm {
        first_delta: String,
        reached: std::sync::Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::llm::LlmTransport for BlockingStreamLlm {
        async fn send(
            &self,
            _instructions: &str,
            _input: Vec<InputItem>,
            _tools: &[crate::llm::ToolDef],
            _previous_response_id: Option<&str>,
        ) -> Result<crate::llm::ResponsesResponse> {
            unreachable!("the chat loop streams")
        }

        async fn send_stream(
            &self,
            _instructions: &str,
            _input: Vec<InputItem>,
            _tools: &[crate::llm::ToolDef],
            _previous_response_id: Option<&str>,
            _idle_timeout: std::time::Duration,
            cancel: CancellationToken,
            deltas: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        ) -> Result<crate::llm::ResponsesResponse> {
            if let Some(tx) = &deltas {
                let _ = tx.send(self.first_delta.clone());
            }
            // Tell the test we're now blocked awaiting the server, then wait for
            // the user's Stop (the external token) — never a self-cancel.
            self.reached.notify_one();
            cancel.cancelled().await;
            Ok(partial_cancelled(self.first_delta.clone()))
        }
    }

    #[tokio::test]
    async fn an_external_stop_interrupts_an_in_flight_send() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let chat = store::create_chat(&pool, None, None).await.unwrap();

        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let llm = BlockingStreamLlm {
            first_delta: "in-flight partial".to_string(),
            reached: reached.clone(),
        };
        // The token the command would hold and the UI would fire on Stop.
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();

        let (events, sink) = collector();
        let run = run_chat_turn(
            &pool,
            &llm,
            "system",
            chat,
            "go",
            100_000,
            1_000_000,
            Duration::from_secs(60),
            Some(cancel),
            &sink,
            None,
        );
        // Press Stop only once the send is actually mid-flight, from a separate
        // task — exactly how `cancel_chat_message` reaches a running turn.
        let presser = async move {
            reached.notified().await;
            stopper.cancel();
        };
        let (res, ()) = tokio::join!(run, presser);
        res.expect("an external Stop is a clean end, not an error");

        {
            let events = events.lock().unwrap();
            assert!(
                !events.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
                "external Stop should not surface as an error: {events:?}"
            );
            assert!(
                events.iter().any(
                    |e| matches!(e, ChatEvent::AssistantMessage { text } if text == "in-flight partial")
                ),
                "the partial streamed before Stop should be kept: {events:?}"
            );
        }

        // Persisted as a normal assistant turn — the turn ended cleanly.
        let turns = store::load_turns(&pool, chat).await.unwrap();
        let roles: Vec<&str> = turns.iter().map(|t| t.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
    }
}
