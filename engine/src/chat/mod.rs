//! Interactive chat agent (Phase 4): persistent, tool-using conversations with
//! the local model.
//!
//! The loop mirrors extraction's (client-side history reconstruction, shared
//! stall-retry from [`crate::agent`]) but is purpose-built for chat: it
//! **persists every turn to SQLite before streaming the matching event** to the
//! UI, captures the model's reasoning for display (never replaying it), and
//! rebuilds the model input from the persisted turns each iteration — so the DB
//! is the single source of truth and the channel is a display accelerator.

pub mod prompt;
pub mod store;

use anyhow::Result;
use sqlx::SqlitePool;
use tracing::{debug, trace};

use mnemis_types::ChatEvent;

use crate::agent;
use crate::extract::tools::{self, FetchBudget, ToolScope};
use crate::llm::{ContentItem, InputItem, LlmTransport, OutputItem, Role};

/// Max model turns for a single user message before we bail (tool-call loop
/// guard). A chat answer rarely needs more than a couple of tool round-trips.
const MAX_CHAT_TURNS: usize = 12;

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
pub async fn run_chat_turn(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    system_prompt: &str,
    chat_id: i64,
    user_text: &str,
    fetch_budget_chars: usize,
    sink: ChatSink<'_>,
) -> Result<()> {
    match run_inner(
        pool,
        llm,
        system_prompt,
        chat_id,
        user_text,
        fetch_budget_chars,
        sink,
    )
    .await
    {
        Ok(()) => {
            sink(ChatEvent::Done);
            Ok(())
        }
        Err(e) => {
            sink(ChatEvent::Error {
                message: format!("{e:#}"),
            });
            Err(e)
        }
    }
}

async fn run_inner(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    system_prompt: &str,
    chat_id: i64,
    user_text: &str,
    fetch_budget_chars: usize,
    sink: ChatSink<'_>,
) -> Result<()> {
    // Persist the user's message first; everything else reconstructs from the DB.
    store::append_turn(pool, chat_id, "user", Some(user_text), None, None, None).await?;
    store::ensure_title(pool, chat_id, user_text).await?;

    let tool_defs = tools::chat_definitions();
    let mut fetch_budget = FetchBudget::new(fetch_budget_chars);

    for turn in 0..MAX_CHAT_TURNS {
        // Rebuild the model input from what's persisted (excludes reasoning).
        let history = store::build_history(pool, chat_id).await?;
        trace!(turn, chat_id, "chat LLM: send");
        let response =
            agent::send_with_stall_retry(llm, system_prompt, &history, &tool_defs, turn, |_| {})
                .await?;
        let response_id = (!response.id.is_empty()).then(|| response.id.clone());

        // Walk the output in order, persisting then emitting. Reasoning rides on
        // the first assistant turn (message or tool call) it precedes.
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
                    if !text.is_empty() {
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
            let out = tools::dispatch(
                pool,
                ToolScope::Global,
                &mut fetch_budget,
                &name,
                &arguments,
            )
            .await;
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
    let response = agent::send_with_stall_retry(llm, SYSTEM, &history, &[], 0, |_| {}).await?;
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
            &sink,
        )
        .await
        .unwrap();

        // Event order: reasoning → tool call → tool result → answer → done.
        let kinds: Vec<&str> = events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                ChatEvent::Reasoning { .. } => "reasoning",
                ChatEvent::AssistantMessage { .. } => "assistant",
                ChatEvent::ToolCall { .. } => "tool_call",
                ChatEvent::ToolResult { .. } => "tool_result",
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
            &sink,
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
}
