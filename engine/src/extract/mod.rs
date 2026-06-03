use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};

use crate::agent;
use crate::llm::{InputItem, LlmTransport, OutputItem, Role};
use crate::source::Recipient;

pub mod prompt;
pub mod tools;
pub mod trace;

use mnemis_types::FeedbackKind;
use prompt::{ExistingAction, FeedbackExample, PromptInputs, WindowMessage};
use tools::ToolScope;
use trace::TraceWriter;

/// Identifies which channel + source an extraction run is processing. Used for
/// the loop's logging context; the message tools see only its `source_id` (via
/// `ToolScope::Source`).
#[derive(Debug, Clone, Copy)]
pub struct ExtractionScope {
    pub source_id: i64,
    pub channel_id: i64,
}

impl ExtractionScope {
    /// The tool-layer scope for this run: bound to its source.
    fn tool_scope(self) -> ToolScope {
        ToolScope::Source(self.source_id)
    }
}

pub const PROMPT_VERSION: i64 = 1;
const WINDOW_LIMIT: i64 = 100;
const MAX_AGENT_TURNS: usize = 20;

/// Chars of body preview shown per message in the metadata-first window.
/// The full body is never in the window — the model fetches it on demand
/// with `fetch_messages`. Matches `search_messages`' snippet length so the
/// model sees a consistent preview size whichever path surfaced the message.
/// Public so the CLI `dump_prompt` command can reproduce the exact window.
pub const SNIPPET_CHARS: usize = 200;

/// Default server context window (tokens) assumed when `[llm]
/// max_context_tokens` isn't set. omlx commonly defaults a local model to
/// 32k; the budget math derives the window size from this.
pub const DEFAULT_MAX_CONTEXT_TOKENS: usize = 32_768;

/// Fraction of the server context window we let a batch's *bodies* occupy.
/// Under metadata-first extraction the initial window is just metadata +
/// snippets (tiny), but the model fetches full bodies on demand and — because
/// turns thread server-side, append-only — every fetched body stays in context
/// for the rest of the run. So this fraction now bounds the *fetch
/// accumulation*: `split_into_batches` packs each batch so its bodies sum under
/// this, and the same value caps `fetch_messages` per run. The rest of the
/// context is reserved for scaffolding (preamble, existing actions, feedback)
/// and the agent loop's multi-turn growth (tool calls, the model's
/// server-retained reasoning). A quarter is the conservative default; the
/// retry-down path in [`extract_for_channel`] catches the cases this
/// under-estimates.
const WINDOW_CONTEXT_FRACTION: f64 = 0.25;

/// Rough chars-per-token used to convert the token budget into the
/// char-based measure `split_into_batches` works in. Deliberately on the low
/// side (English prose is ~4) so the estimate errs toward smaller windows.
const CHARS_PER_TOKEN: usize = 4;

/// Per-batch character budget — also the per-run `fetch_messages` budget —
/// derived from the server's context window. Kept as chars because that's what
/// we can cheaply measure without a tokenizer.
pub fn window_char_budget_for(max_context_tokens: usize) -> usize {
    ((max_context_tokens as f64 * WINDOW_CONTEXT_FRACTION) as usize).saturating_mul(CHARS_PER_TOKEN)
}

/// Char budget used by tests and as the fallback when nothing is configured.
/// Equals [`window_char_budget_for`]\([`DEFAULT_MAX_CONTEXT_TOKENS`]\).
pub const DEFAULT_WINDOW_CHAR_BUDGET: usize = DEFAULT_MAX_CONTEXT_TOKENS / 4 * CHARS_PER_TOKEN;

/// What a single batch attempt resolved to. `Overflow` means the agent hit
/// the server's context limit and the batch is splittable — the caller
/// halves it and retries — so no `extraction_runs` row is written for the
/// discarded attempt.
enum BatchResult {
    Done {
        actions: usize,
        summary: Option<String>,
        up_to: Option<i64>,
    },
    Overflow,
}

#[derive(Debug)]
pub struct ExtractionOutcome {
    pub result: &'static str,
    pub actions_created: usize,
    pub up_to_message_id: Option<i64>,
    pub summary: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub async fn extract_for_channel(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    channel_id: i64,
    model_name: &str,
    window_char_budget: usize,
    idle_timeout: Duration,
    traces_dir: Option<&Path>,
) -> Result<ExtractionOutcome> {
    let channel = load_channel(pool, channel_id).await?;
    let watermark = load_watermark(pool, channel_id).await?;
    let window = load_window(pool, channel_id, watermark).await?;

    if window.is_empty() {
        record_run(
            pool,
            channel_id,
            None,
            model_name,
            "no_activity",
            0,
            0,
            None,
        )
        .await?;
        return Ok(ExtractionOutcome {
            result: "no_activity",
            actions_created: 0,
            up_to_message_id: watermark,
            summary: None,
        });
    }

    // Static-per-channel inputs: load once, reused across batches.
    let profile = load_user_profile_for(pool, &channel.source_kind).await?;
    let feedback = load_feedback_for(pool, channel.source_id, channel_id).await?;

    // Split the window so no single LLM call carries a prompt large enough
    // to time out / OOM / over-context the server. Each batch is its own
    // extractor session that records its own extraction_runs row (advancing
    // the watermark), so a mid-window failure leaves earlier batches
    // committed and the rest to be retried next sync.
    //
    // The char budget is only an estimate of where the *full* prompt
    // (window + scaffolding + multi-turn growth) lands, so a batch can still
    // overflow at turn N. When that happens we halve the offending batch and
    // re-run the pieces — a work queue, not a fixed list, so a split feeds
    // straight back in.
    let initial = split_into_batches(window, window_char_budget);
    if initial.len() > 1 {
        debug!(
            channel_id,
            batches = initial.len(),
            budget = window_char_budget,
            "extract: window split into batches"
        );
    }
    let mut queue: std::collections::VecDeque<Vec<WindowRow>> = initial.into();

    let mut total_actions = 0usize;
    let mut last_summary: Option<String> = None;
    let mut last_up_to = watermark;

    while let Some(batch) = queue.pop_front() {
        let result = extract_one_batch(
            pool,
            llm,
            &channel,
            channel_id,
            model_name,
            &profile,
            &feedback,
            &batch,
            window_char_budget,
            idle_timeout,
            traces_dir,
        )
        .await
        .map_err(|e| e.context(format!("channel {channel_id}")))?;
        match result {
            BatchResult::Done {
                actions,
                summary,
                up_to,
            } => {
                total_actions += actions;
                if summary.is_some() {
                    last_summary = summary;
                }
                last_up_to = up_to;
            }
            BatchResult::Overflow => {
                // extract_one_batch only returns Overflow for a splittable
                // (len > 1) batch. Halve it and process the halves next, in
                // order, ahead of the rest of the queue.
                let mut left = batch;
                let right = left.split_off(left.len() / 2);
                warn!(
                    channel_id,
                    left = left.len(),
                    right = right.len(),
                    "extract: batch over context window, splitting and retrying"
                );
                queue.push_front(right);
                queue.push_front(left);
            }
        }
    }

    Ok(ExtractionOutcome {
        result: "ok",
        actions_created: total_actions,
        up_to_message_id: last_up_to,
        summary: last_summary,
    })
}

/// Greedily pack the window into batches whose combined message bodies stay
/// under `char_budget`. We cost by full *body* length (plus subject) even
/// though the window only renders snippets, because the budget bounds the
/// worst case where the model fetches every message in the batch — sizing
/// batches this way guarantees a run can fetch all its bodies without blowing
/// the fetch budget. A single message that already exceeds the budget still
/// gets its own batch (we never drop messages or stall), so the budget is a
/// soft target, not a hard cap.
fn split_into_batches(window: Vec<WindowRow>, char_budget: usize) -> Vec<Vec<WindowRow>> {
    let mut batches: Vec<Vec<WindowRow>> = Vec::new();
    let mut current: Vec<WindowRow> = Vec::new();
    let mut current_chars = 0usize;

    for msg in window {
        let cost = msg.subject.as_deref().map_or(0, str::len) + msg.body.len();
        // Start a new batch when adding this message would blow the budget,
        // but only if the current batch already has something — otherwise a
        // lone oversize message would loop forever.
        if !current.is_empty() && current_chars + cost > char_budget {
            batches.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current_chars += cost;
        current.push(msg);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Run one batch as a self-contained extractor session: build the prompt
/// from just this batch's messages, run the agent loop, and record an
/// `extraction_runs` row (advancing the watermark to the batch's last
/// message on success, or an `error` row on a terminal failure).
///
/// Returns `BatchResult::Overflow` (without recording a run) when the agent
/// hit the server's context limit and the batch is splittable — the caller
/// halves it and retries. A non-context failure, or an over-context failure
/// on a single-message batch (nothing left to split), records an `error` run
/// and is returned as `Err`.
#[allow(clippy::too_many_arguments)]
async fn extract_one_batch(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    channel: &ChannelInfo,
    channel_id: i64,
    model_name: &str,
    profile: &UserProfile,
    feedback: &[FeedbackExample],
    batch: &[WindowRow],
    window_char_budget: usize,
    idle_timeout: Duration,
    traces_dir: Option<&Path>,
) -> Result<BatchResult> {
    let up_to = batch.last().map(|m| m.id);
    // Reload existing actions per batch so a later batch can see (and dedup
    // against) actions an earlier batch in this same sync just recorded.
    let existing = load_existing_actions(pool, channel_id).await?;

    let window_for_prompt: Vec<WindowMessage> = batch
        .iter()
        .map(|m| WindowMessage {
            external_id: m.external_id.clone(),
            posted_at: DateTime::<Utc>::from_timestamp(m.posted_at, 0).unwrap_or_else(Utc::now),
            author: m.author_display.clone().unwrap_or_else(|| "?".to_string()),
            recipients: m.recipients.clone(),
            subject: m.subject.clone(),
            snippet: tools::snippet(&m.body, SNIPPET_CHARS),
        })
        .collect();

    let inputs = PromptInputs {
        source_kind: &channel.source_kind,
        channel_name: &channel.name,
        user_display_name: profile.display_name.as_deref().unwrap_or("(unknown)"),
        user_identifiers: &profile.identifiers,
        custom_prompt: profile.custom_prompt.as_deref(),
        current_time: Utc::now(),
        existing_actions: &existing,
        feedback,
        window: &window_for_prompt,
    };
    let system_prompt = prompt::build(&inputs);

    let scope = ExtractionScope {
        source_id: channel.source_id,
        channel_id,
    };
    let ran_at = Utc::now().timestamp();
    let mut writer = traces_dir.map(|dir| TraceWriter::open(dir, ran_at, channel_id));
    if let Some(w) = writer.as_mut() {
        w.system_prompt(&system_prompt);
        w.tools(&tools::definitions());
    }
    // The batch's bodies sum to under window_char_budget (split_into_batches
    // packs to that), so the same value as the fetch budget lets the model
    // fetch every message in this batch without over-contexting.
    let outcome = run_agent_loop(
        pool,
        llm,
        scope,
        system_prompt,
        window_char_budget,
        idle_timeout,
        writer.as_mut(),
    )
    .await;

    // Record the run first either way, then bubble the error so the
    // orchestrator can surface it in SyncOutcome.errors → the toast.
    // Swallowing it here meant per-channel extract failures were invisible
    // in the UI even though they wrote to extraction_runs.
    match outcome {
        Ok((actions_created, summary)) => {
            if let Some(w) = writer.as_mut() {
                w.finish(actions_created, summary.as_deref());
            }
            record_run(
                pool,
                channel_id,
                up_to,
                model_name,
                "ok",
                actions_created,
                0,
                summary.clone(),
            )
            .await?;
            Ok(BatchResult::Done {
                actions: actions_created,
                summary,
                up_to,
            })
        }
        Err(e) => {
            let chain = format!("{e:#}");
            warn!(error = %chain, channel_id, "extraction agent failed");
            if let Some(w) = writer.as_mut() {
                w.agent_error(&chain);
            }
            // Over-context on a splittable batch isn't a real failure — let
            // the caller halve it and retry. Don't record a run for the
            // discarded attempt (the trace already captured the agent_error);
            // the sub-batches will record their own.
            if crate::agent::is_context_overflow(&chain) && batch.len() > 1 {
                debug!(
                    channel_id,
                    messages = batch.len(),
                    "extract: batch overflowed context, deferring to caller for split"
                );
                return Ok(BatchResult::Overflow);
            }
            record_run(
                pool,
                channel_id,
                up_to,
                model_name,
                "error",
                0,
                0,
                Some(chain.clone()),
            )
            .await?;
            Err(e)
        }
    }
}

struct ChannelInfo {
    source_id: i64,
    source_kind: String,
    name: String,
}

async fn load_channel(pool: &SqlitePool, channel_id: i64) -> Result<ChannelInfo> {
    let (source_id, source_kind, name): (i64, String, String) = sqlx::query_as(
        "SELECT c.source_id, s.kind, c.name \
         FROM channels c JOIN sources s ON s.id = c.source_id \
         WHERE c.id = ?",
    )
    .bind(channel_id)
    .fetch_one(pool)
    .await
    .context("loading channel")?;
    Ok(ChannelInfo {
        source_id,
        source_kind,
        name,
    })
}

async fn load_watermark(pool: &SqlitePool, channel_id: i64) -> Result<Option<i64>> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        // Tie-break on id DESC: batched extraction records several runs for
        // one channel that can share a `ran_at` second, and we need the
        // last-inserted (highest watermark) to win — otherwise the next sync
        // re-processes messages an earlier batch already handled.
        "SELECT up_to_message_id FROM extraction_runs \
         WHERE channel_id = ? AND result IN ('ok', 'no_activity') \
         ORDER BY ran_at DESC, id DESC LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(opt,)| opt))
}

struct WindowRow {
    id: i64,
    external_id: String,
    posted_at: i64,
    subject: Option<String>,
    /// Full body — kept for snippet generation and batch-cost accounting
    /// (worst-case fetch size), but NOT rendered into the window.
    body: String,
    author_display: Option<String>,
    recipients: Vec<Recipient>,
}

#[allow(clippy::type_complexity)]
async fn load_window(
    pool: &SqlitePool,
    channel_id: i64,
    watermark: Option<i64>,
) -> Result<Vec<WindowRow>> {
    let rows: Vec<(
        i64,
        String,
        i64,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT m.id, m.external_id, m.posted_at, m.subject, m.body, p.display_name, \
             m.recipients_json \
             FROM messages m \
             LEFT JOIN people p ON p.id = m.author_id \
             WHERE m.channel_id = ? AND m.id > ? \
             ORDER BY m.id ASC LIMIT ?",
    )
    .bind(channel_id)
    .bind(watermark.unwrap_or(0))
    .bind(WINDOW_LIMIT)
    .fetch_all(pool)
    .await
    .context("loading window")?;
    Ok(rows
        .into_iter()
        .map(
            |(id, external_id, posted_at, subject, body, author_display, recipients_json)| {
                // Tolerate NULL / malformed recipients_json — recipients are a
                // triage hint, never load-bearing for correctness.
                let recipients = recipients_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Vec<Recipient>>(s).ok())
                    .unwrap_or_default();
                WindowRow {
                    id,
                    external_id,
                    posted_at,
                    subject,
                    body,
                    author_display,
                    recipients,
                }
            },
        )
        .collect())
}

async fn load_existing_actions(pool: &SqlitePool, channel_id: i64) -> Result<Vec<ExistingAction>> {
    let rows: Vec<(i64, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT DISTINCT a.id, a.title, a.details, a.due_at \
         FROM actions a \
         JOIN action_evidence ae ON ae.action_id = a.id \
         JOIN messages m ON m.id = ae.message_id \
         WHERE m.channel_id = ? AND a.status IN ('pending', 'auto_claimed', 'claimed') \
         ORDER BY a.extracted_at DESC LIMIT 50",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await
    .context("loading existing actions")?;
    Ok(rows
        .into_iter()
        .map(|(id, title, details, due_at)| ExistingAction {
            id,
            title,
            details,
            due_at: due_at.and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0)),
        })
        .collect())
}

/// Pull the most-recent feedback rows the extractor should consider as
/// negative examples. Scoping is union-of-three (channel + source + global),
/// capped at ~10 newest overall — the design note in `v2-redesign` keeps the
/// budget tight so the section doesn't crowd the window. Both
/// `FeedbackKind::Dismissed` and `FeedbackKind::WrongAutoClaim` rows are
/// returned together; the prompt builder groups them.
pub async fn load_feedback_for(
    pool: &SqlitePool,
    source_id: i64,
    channel_id: i64,
) -> Result<Vec<FeedbackExample>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT kind, example_text, reason FROM dismissal_feedback \
         WHERE (scope_kind = 'channel' AND scope_id = ?) \
            OR (scope_kind = 'source'  AND scope_id = ?) \
            OR  scope_kind = 'global' \
         ORDER BY created_at DESC LIMIT 10",
    )
    .bind(channel_id)
    .bind(source_id)
    .fetch_all(pool)
    .await
    .context("loading dismissal_feedback")?;

    Ok(rows
        .into_iter()
        .filter_map(|(kind, example_text, reason)| {
            FeedbackKind::parse(&kind).map(|kind| FeedbackExample {
                kind,
                example_text,
                reason,
            })
        })
        .collect())
}

struct UserProfile {
    display_name: Option<String>,
    custom_prompt: Option<String>,
    identifiers: Vec<String>,
}

async fn load_user_profile_for(pool: &SqlitePool, source_kind: &str) -> Result<UserProfile> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT display_name, custom_prompt FROM user_profile WHERE id = 1")
            .fetch_optional(pool)
            .await?;
    let (display_name, custom_prompt) = match row {
        Some((n, c)) => (Some(n), c),
        None => (None, None),
    };

    // Map source kind to identifier kinds the user might have on it.
    let identifier_kinds: &[&str] = match source_kind {
        "imap" => &["email"],
        "mattermost" => &["mattermost_handle", "email"],
        "discord" => &["discord_id"],
        _ => &[],
    };

    let mut identifiers = Vec::new();
    if !identifier_kinds.is_empty() {
        for kind in identifier_kinds {
            let rows: Vec<(String,)> = sqlx::query_as(
                "SELECT ci.value FROM contact_identifiers ci \
                 JOIN contacts c ON c.id = ci.contact_id \
                 WHERE c.relationship = 'self' AND ci.kind = ?",
            )
            .bind(*kind)
            .fetch_all(pool)
            .await?;
            identifiers.extend(rows.into_iter().map(|(v,)| v));
        }
    }

    Ok(UserProfile {
        display_name,
        custom_prompt,
        identifiers,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_loop(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    scope: ExtractionScope,
    system_prompt: String,
    fetch_budget_chars: usize,
    idle_timeout: Duration,
    mut trace: Option<&mut TraceWriter>,
) -> Result<(usize, Option<String>)> {
    let tool_defs = tools::definitions();
    // The full conversation, rebuilt client-side each turn instead of threaded
    // server-side via `previous_response_id`. Threading wedged omlx on the
    // occasional stuck continuation — re-POSTing the same response id was
    // futile — and it replayed the model's prior reasoning, which Qwen/Gemma
    // mishandle. Reconstructing ourselves means every turn (and every retry) is
    // a fresh request with no stuck server-side state to inherit, and we
    // control what's replayed: the assistant's messages and tool calls, but
    // never its reasoning (the reasoning-replay rule — see the v1-gotchas
    // memory).
    let mut history = vec![InputItem::Message {
        role: Role::User,
        content: "Process the messages in the window. Record any actions you find.".to_string(),
    }];
    let mut actions_created = 0usize;
    // Bounds the total fetched-body chars this run accumulates in `history`.
    // The batch was sized so its bodies fit under this; the cap is the backstop
    // for re-fetches / search_messages pulls.
    let mut fetch_budget = tools::FetchBudget::new(fetch_budget_chars);

    // Stream the extraction sends too — same wedge risk as chat: a large batch
    // prefill can legitimately emit nothing for longer than the non-streaming
    // total timeout, getting cut mid-generation. Headless (no Stop button, no
    // live rendering): the win here is purely the per-chunk idle timeout.
    let stream = agent::StreamCtx::headless(idle_timeout);

    for turn in 0..MAX_AGENT_TURNS {
        let t = Instant::now();
        if let Some(w) = trace.as_deref_mut() {
            w.llm_send(turn, &history, None);
        }
        trace!(turn, channel_id = scope.channel_id, "LLM: send");
        // A wedged omlx server accepts the request, emits an HTTP 200, then
        // stalls — surfacing as an empty body or timeout. The shared helper
        // retries transient stalls with backoff (deterministic failures, like
        // context overflow, bubble straight to the caller's split-down path)
        // and reports each attempt so we can record it in the trace.
        let response = agent::send_with_stall_retry(
            llm,
            &system_prompt,
            &history,
            &tool_defs,
            turn,
            Some(&stream),
            |ev| {
                if let Some(w) = trace.as_deref_mut() {
                    match ev {
                        agent::StallEvent::Retry { attempt, error } => {
                            w.llm_error(turn, &format!("transient stall, retry {attempt}: {error}"))
                        }
                        agent::StallEvent::GaveUp { error } => w.llm_error(turn, error),
                    }
                }
            },
        )
        .await?;
        let elapsed = t.elapsed().as_secs();
        if let Some(w) = trace.as_deref_mut() {
            w.llm_recv(turn, elapsed, &response);
        }
        trace!(
            turn,
            channel_id = scope.channel_id,
            secs = elapsed,
            "LLM: recv"
        );

        // Append the assistant's turn to `history` in output order — its text
        // message and tool calls, but NOT its reasoning — then dispatch the
        // calls. The `FunctionCallOutput`s pushed afterwards carry the same
        // `call_id`s, so the next turn replays a well-formed exchange.
        let mut function_calls = Vec::new();
        let mut text_parts = Vec::new();
        for item in response.output {
            match item {
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    history.push(InputItem::FunctionCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    function_calls.push((call_id, name, arguments));
                }
                OutputItem::Message { content } => {
                    let mut msg = String::new();
                    for c in content {
                        if let crate::llm::ContentItem::OutputText { text } = c {
                            msg.push_str(&text);
                        }
                    }
                    if !msg.is_empty() {
                        history.push(InputItem::Message {
                            role: Role::Assistant,
                            content: msg.clone(),
                        });
                        text_parts.push(msg);
                    }
                }
                OutputItem::Reasoning { .. } | OutputItem::Unknown => {}
            }
        }

        if function_calls.is_empty() {
            let last_text = text_parts.join("");
            let summary = if last_text.is_empty() {
                None
            } else {
                Some(last_text)
            };
            return Ok((actions_created, summary));
        }

        for (call_id, name, arguments) in function_calls {
            debug!(name = %name, call_id = %call_id, "dispatching tool");
            let out = tools::dispatch(
                pool,
                scope.tool_scope(),
                &mut fetch_budget,
                &name,
                &arguments,
            )
            .await;
            if out.recorded_action {
                actions_created += 1;
            }
            if let Some(w) = trace.as_deref_mut() {
                w.tool_dispatch(turn, &call_id, &name, &arguments, &out.output);
            }
            history.push(InputItem::FunctionCallOutput {
                call_id,
                output: out.output,
            });
        }
    }

    anyhow::bail!("extraction agent exceeded {MAX_AGENT_TURNS} turns")
}

#[allow(clippy::too_many_arguments)]
async fn record_run(
    pool: &SqlitePool,
    channel_id: i64,
    up_to_message_id: Option<i64>,
    model: &str,
    result: &str,
    _actions_created: usize,
    messages_pending_embed: i64,
    summary: Option<String>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO extraction_runs \
         (channel_id, ran_at, up_to_message_id, model, prompt_version, result, \
          embeddings_partial, messages_pending_embed, summary) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(channel_id)
    .bind(Utc::now().timestamp())
    .bind(up_to_message_id)
    .bind(model)
    .bind(PROMPT_VERSION)
    .bind(result)
    .bind(if messages_pending_embed > 0 { 1 } else { 0 })
    .bind(messages_pending_embed)
    .bind(summary)
    .execute(pool)
    .await
    .context("inserting extraction_runs")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::ingest::ingest_batch;
    use crate::llm::ResponsesResponse;
    use crate::source::{Cursor, ImportedAuthor, ImportedMessage, PollBatch, SourceId};
    use anyhow::bail;
    use async_trait::async_trait;
    use chrono::Utc;
    use tempfile::TempDir;

    /// An LlmTransport that always fails — for pinning the error-propagation
    /// path. Mirrors what omlx returns when the prompt blows the context
    /// window or the server is down.
    struct FailingLlm(&'static str);

    #[async_trait]
    impl crate::llm::LlmTransport for FailingLlm {
        async fn send(
            &self,
            _instructions: &str,
            _input: Vec<crate::llm::InputItem>,
            _tools: &[crate::llm::ToolDef],
            _previous_response_id: Option<&str>,
        ) -> Result<ResponsesResponse> {
            bail!("{}", self.0)
        }
    }

    /// A transport whose per-`send` results are scripted in order: `Err(msg)`
    /// bails with that message, `Ok(resp)` returns it. Lets a test interleave
    /// a context-overflow failure with subsequent successes to exercise the
    /// split-and-retry path.
    struct SequencedLlm {
        steps: std::sync::Mutex<std::collections::VecDeque<Result<ResponsesResponse, String>>>,
    }

    impl SequencedLlm {
        fn new(steps: Vec<Result<ResponsesResponse, String>>) -> Self {
            Self {
                steps: std::sync::Mutex::new(steps.into()),
            }
        }
    }

    #[async_trait]
    impl crate::llm::LlmTransport for SequencedLlm {
        async fn send(
            &self,
            _instructions: &str,
            _input: Vec<crate::llm::InputItem>,
            _tools: &[crate::llm::ToolDef],
            _previous_response_id: Option<&str>,
        ) -> Result<ResponsesResponse> {
            let step = self
                .steps
                .lock()
                .expect("SequencedLlm poisoned")
                .pop_front()
                .expect("SequencedLlm: send() called more times than scripted");
            step.map_err(|m| anyhow::anyhow!(m))
        }
    }

    /// Records the `input` and `previous_response_id` of every `send`, replaying
    /// scripted responses in order. Lets a test assert how the agent loop builds
    /// each request — e.g. that it reconstructs history client-side and never
    /// threads via `previous_response_id`.
    struct CapturingLlm {
        steps: std::sync::Mutex<std::collections::VecDeque<ResponsesResponse>>,
        seen: std::sync::Mutex<Vec<(Vec<crate::llm::InputItem>, Option<String>)>>,
    }

    impl CapturingLlm {
        fn new(steps: Vec<ResponsesResponse>) -> Self {
            Self {
                steps: std::sync::Mutex::new(steps.into()),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl crate::llm::LlmTransport for CapturingLlm {
        async fn send(
            &self,
            _instructions: &str,
            input: Vec<crate::llm::InputItem>,
            _tools: &[crate::llm::ToolDef],
            previous_response_id: Option<&str>,
        ) -> Result<ResponsesResponse> {
            self.seen
                .lock()
                .expect("CapturingLlm poisoned")
                .push((input, previous_response_id.map(str::to_string)));
            self.steps
                .lock()
                .expect("CapturingLlm poisoned")
                .pop_front()
                .map(Ok)
                .expect("CapturingLlm: send() called more times than scripted")
        }
    }

    async fn setup() -> Result<(TempDir, SqlitePool, i64, i64)> {
        let tmp = TempDir::new()?;
        let path = tmp.path().join("test.db");
        let pool = db::open(&path).await?;
        db::migrate(&pool).await?;

        let (source_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'test', 'kc/test', ?) RETURNING id",
        )
        .bind(Utc::now().timestamp())
        .fetch_one(&pool)
        .await?;
        let (channel_id,): (i64,) = sqlx::query_as(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, 'INBOX', 'INBOX', 'mailbox') RETURNING id",
        )
        .bind(source_id)
        .fetch_one(&pool)
        .await?;

        let msg = ImportedMessage {
            external_id: "msg-1".to_string(),
            parent_external_id: None,
            author: Some(ImportedAuthor {
                external_id: "ana@example.com".to_string(),
                display_name: Some("Ana".to_string()),
                handle: None,
            }),
            posted_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            subject: Some("Hello".to_string()),
            body: "Please take a look".to_string(),
            body_format: "text".to_string(),
            recipients: Vec::new(),
            raw_json: None,
            flags: 0,
        };
        ingest_batch(
            &pool,
            SourceId(source_id),
            channel_id,
            &PollBatch {
                messages: vec![msg],
                next_cursor: Cursor("1:2".to_string()),
                more_available: false,
            },
        )
        .await?;

        Ok((tmp, pool, source_id, channel_id))
    }

    #[tokio::test]
    async fn record_action_persists_action_evidence_and_event() -> Result<()> {
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        let args = serde_json::json!({
            "title": "Take a look at the doc",
            "details": "Ana asked you to review.",
            "confidence": "high",
            "rationale": "Direct ask from Ana",
            "evidence_external_ids": ["msg-1"]
        })
        .to_string();

        let scope = ExtractionScope {
            source_id,
            channel_id,
        };
        let out = tools::dispatch(
            &pool,
            scope.tool_scope(),
            &mut tools::FetchBudget::unlimited(),
            "record_action",
            &args,
        )
        .await;
        assert!(out.recorded_action);

        let actions: (i64, String, String, String) =
            sqlx::query_as("SELECT id, title, confidence, status FROM actions LIMIT 1")
                .fetch_one(&pool)
                .await?;
        assert_eq!(actions.1, "Take a look at the doc");
        assert_eq!(actions.2, "high");
        assert_eq!(actions.3, "auto_claimed");

        let (evidence_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM action_evidence WHERE action_id = ?")
                .bind(actions.0)
                .fetch_one(&pool)
                .await?;
        assert_eq!(evidence_count, 1);

        let (event_kind, actor): (String, String) =
            sqlx::query_as("SELECT event_kind, actor FROM action_events WHERE action_id = ?")
                .bind(actions.0)
                .fetch_one(&pool)
                .await?;
        assert_eq!(event_kind, "created");
        assert_eq!(actor, "agent_auto");

        let (queued,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embed_queue WHERE target_kind = 'action'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(queued, 1);

        Ok(())
    }

    #[tokio::test]
    async fn record_action_rolls_back_when_no_evidence_resolves() -> Result<()> {
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        let args = serde_json::json!({
            "title": "Imaginary",
            "details": "Refers to a message that doesn't exist.",
            "confidence": "medium",
            "rationale": "Test",
            "evidence_external_ids": ["does-not-exist"]
        })
        .to_string();

        let scope = ExtractionScope {
            source_id,
            channel_id,
        };
        let out = tools::dispatch(
            &pool,
            scope.tool_scope(),
            &mut tools::FetchBudget::unlimited(),
            "record_action",
            &args,
        )
        .await;
        assert!(!out.recorded_action);
        assert!(out.output.contains("error"));

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM actions")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);

        Ok(())
    }

    async fn ingest_extra_message(
        pool: &SqlitePool,
        source_id: i64,
        channel_id: i64,
        external_id: &str,
    ) -> Result<()> {
        let msg = ImportedMessage {
            external_id: external_id.to_string(),
            parent_external_id: None,
            author: Some(ImportedAuthor {
                external_id: "ana@example.com".to_string(),
                display_name: Some("Ana".to_string()),
                handle: None,
            }),
            posted_at: DateTime::<Utc>::from_timestamp(1_700_000_100, 0).unwrap(),
            subject: Some("Follow-up".to_string()),
            body: "And one more thing".to_string(),
            body_format: "text".to_string(),
            recipients: Vec::new(),
            raw_json: None,
            flags: 0,
        };
        ingest_batch(
            pool,
            SourceId(source_id),
            channel_id,
            &PollBatch {
                messages: vec![msg],
                next_cursor: Cursor("2:3".to_string()),
                more_available: false,
            },
        )
        .await?;
        Ok(())
    }

    fn window_row(id: i64, body_len: usize) -> WindowRow {
        WindowRow {
            id,
            external_id: format!("m{id}"),
            posted_at: 1_700_000_000 + id,
            subject: None,
            body: "x".repeat(body_len),
            author_display: None,
            recipients: Vec::new(),
        }
    }

    #[test]
    fn split_into_batches_packs_greedily_under_budget() {
        // Three 40-char messages, budget 100 → [m1,m2] then [m3].
        let window = vec![window_row(1, 40), window_row(2, 40), window_row(3, 40)];
        let batches = split_into_batches(window, 100);
        let shape: Vec<Vec<i64>> = batches
            .iter()
            .map(|b| b.iter().map(|m| m.id).collect())
            .collect();
        assert_eq!(shape, vec![vec![1, 2], vec![3]]);
    }

    #[test]
    fn split_into_batches_gives_oversize_message_its_own_batch() {
        // A single message bigger than the whole budget must not stall or be
        // dropped — it gets a batch to itself.
        let window = vec![window_row(1, 10), window_row(2, 500), window_row(3, 10)];
        let batches = split_into_batches(window, 100);
        let shape: Vec<Vec<i64>> = batches
            .iter()
            .map(|b| b.iter().map(|m| m.id).collect())
            .collect();
        assert_eq!(shape, vec![vec![1], vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn extract_for_channel_splits_window_into_batches() {
        // With a tiny budget each message lands in its own batch; every batch
        // is a self-contained extractor session that records its own
        // extraction_runs row and advances the watermark. Pin: N messages →
        // N runs, watermark ends at the last message.
        let (_tmp, pool, source_id, channel_id) = setup().await.unwrap();
        ingest_extra_message(&pool, source_id, channel_id, "msg-2")
            .await
            .unwrap();
        ingest_extra_message(&pool, source_id, channel_id, "msg-3")
            .await
            .unwrap();

        let last_id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM messages WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_one(&pool)
            .await
            .unwrap();

        // One scripted "no tool calls, just finish" turn per batch session.
        let llm = crate::test_util::MockLlm::new(vec![
            crate::test_util::mock::no_tools("nothing here"),
            crate::test_util::mock::no_tools("nothing here"),
            crate::test_util::mock::no_tools("nothing here"),
        ]);

        let outcome = extract_for_channel(
            &pool,
            &llm,
            channel_id,
            "test-model",
            1,
            Duration::from_secs(60),
            None,
        )
        .await
        .expect("multi-batch extraction should succeed");
        assert_eq!(outcome.result, "ok");
        assert_eq!(outcome.up_to_message_id, Some(last_id));

        let (run_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM extraction_runs WHERE channel_id = ? AND result = 'ok'",
        )
        .bind(channel_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(run_count, 3, "expected one extraction_runs row per batch");

        // Next sync sees an empty window — the watermark drained everything.
        let watermark = load_watermark(&pool, channel_id).await.unwrap();
        assert_eq!(watermark, Some(last_id));
    }

    #[tokio::test]
    async fn extract_for_channel_stops_at_failing_batch_and_keeps_prior_watermark() {
        // If a middle batch fails, earlier batches stay committed (their
        // watermark holds) and the failing batch + remainder are left for the
        // next sync. Here the first batch's session fails on turn 0, so the
        // watermark must NOT advance past the original (None).
        let (_tmp, pool, source_id, channel_id) = setup().await.unwrap();
        ingest_extra_message(&pool, source_id, channel_id, "msg-2")
            .await
            .unwrap();

        let llm =
            FailingLlm("error sending request for url: connection closed before message completed");
        let result = extract_for_channel(
            &pool,
            &llm,
            channel_id,
            "test-model",
            1,
            Duration::from_secs(60),
            None,
        )
        .await;
        assert!(result.is_err(), "a failing batch should bubble an error");

        // No successful run recorded → watermark stays unset, so the next
        // sync retries from the top.
        let watermark = load_watermark(&pool, channel_id).await.unwrap();
        assert_eq!(watermark, None);

        let (err_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM extraction_runs WHERE channel_id = ? AND result = 'error'",
        )
        .bind(channel_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(err_count, 1, "the failing batch should record an error run");
    }

    #[test]
    fn window_char_budget_reserves_headroom_below_context() {
        // The window gets a quarter of the context (in tokens), converted to
        // chars — leaving the rest for scaffolding + multi-turn growth.
        assert_eq!(window_char_budget_for(32_768), 32_768);
        assert_eq!(window_char_budget_for(8_000), 8_000);
        // Always strictly under the full context in token terms.
        assert!(window_char_budget_for(32_768) / CHARS_PER_TOKEN < 32_768);
        // The documented default matches the derivation for the default ctx.
        assert_eq!(
            DEFAULT_WINDOW_CHAR_BUDGET,
            window_char_budget_for(DEFAULT_MAX_CONTEXT_TOKENS)
        );
    }

    #[tokio::test]
    async fn extract_for_channel_retries_smaller_on_context_overflow() {
        // The char budget only estimates where the *full* prompt lands, so a
        // batch can still overflow at the server. When it does, the batch is
        // halved and the pieces re-run — turning one over-context failure into
        // two successful sub-batches, with no error run left behind.
        let (_tmp, pool, source_id, channel_id) = setup().await.unwrap();
        ingest_extra_message(&pool, source_id, channel_id, "msg-2")
            .await
            .unwrap();
        let last_id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM messages WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_one(&pool)
            .await
            .unwrap();

        // Big budget → both messages start in one batch. First send is the
        // server rejecting it as over-context; the two halves then succeed.
        let llm = SequencedLlm::new(vec![
            Err(
                "LLM API error (HTTP 400 Bad Request): Prompt too long: 37172 tokens \
                 exceeds max context window of 32768 tokens"
                    .to_string(),
            ),
            Ok(crate::test_util::mock::no_tools("ok")),
            Ok(crate::test_util::mock::no_tools("ok")),
        ]);

        let outcome = extract_for_channel(
            &pool,
            &llm,
            channel_id,
            "test-model",
            DEFAULT_WINDOW_CHAR_BUDGET,
            Duration::from_secs(60),
            None,
        )
        .await
        .expect("split-and-retry should recover");
        assert_eq!(outcome.result, "ok");
        assert_eq!(outcome.up_to_message_id, Some(last_id));

        let (ok_runs,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM extraction_runs WHERE channel_id = ? AND result = 'ok'",
        )
        .bind(channel_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ok_runs, 2, "the two halves should each record an ok run");

        let (err_runs,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM extraction_runs WHERE channel_id = ? AND result = 'error'",
        )
        .bind(channel_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            err_runs, 0,
            "the discarded over-context attempt must not leave an error run"
        );

        assert_eq!(
            load_watermark(&pool, channel_id).await.unwrap(),
            Some(last_id)
        );
    }

    #[tokio::test]
    async fn extract_for_channel_retries_through_a_transient_server_stall() {
        // omlx under memory pressure accepts the request then stalls, which the
        // transport surfaces as an empty body. That's transient, so the turn is
        // retried in place and the channel still extracts — no error run is left
        // behind and the watermark advances. (Contrast the overflow test above,
        // where a *deterministic* over-context failure splits the batch instead.)
        let (_tmp, pool, _source_id, channel_id) = setup().await.unwrap();
        let last_id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM messages WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_one(&pool)
            .await
            .unwrap();

        // First send stalls (empty body); the retry succeeds.
        let llm = SequencedLlm::new(vec![
            Err(
                "LLM returned an empty response body (HTTP 200 OK); the server accepted \
                 the request then produced no output"
                    .to_string(),
            ),
            Ok(crate::test_util::mock::no_tools("ok")),
        ]);

        let outcome = extract_for_channel(
            &pool,
            &llm,
            channel_id,
            "test-model",
            DEFAULT_WINDOW_CHAR_BUDGET,
            Duration::from_secs(60),
            None,
        )
        .await
        .expect("a transient stall should be retried, not surfaced");
        assert_eq!(outcome.result, "ok");
        assert_eq!(outcome.up_to_message_id, Some(last_id));

        let (err_runs,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM extraction_runs WHERE channel_id = ? AND result = 'error'",
        )
        .bind(channel_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(err_runs, 0, "a retried stall must not leave an error run");
        assert_eq!(
            load_watermark(&pool, channel_id).await.unwrap(),
            Some(last_id)
        );
    }

    #[tokio::test]
    async fn agent_loop_reconstructs_history_client_side_without_threading() {
        // Extraction rebuilds the conversation itself instead of threading via
        // previous_response_id: every send carries the full history (no response
        // id), replaying the assistant's tool calls and their outputs but never
        // its reasoning. Pins that contract so a future refactor can't silently
        // reintroduce server-side threading or replay reasoning.
        let (_tmp, pool, _source_id, channel_id) = setup().await.unwrap();

        let llm = CapturingLlm::new(vec![
            // Turn 0: a reasoning item (must NOT be replayed) + a fetch call.
            crate::test_util::mock::turn(vec![
                OutputItem::Reasoning {
                    summary: vec![crate::llm::ReasoningSummary {
                        text: "should never be replayed".to_string(),
                    }],
                },
                OutputItem::FunctionCall {
                    call_id: "c1".to_string(),
                    name: "fetch_messages".to_string(),
                    arguments: serde_json::json!({ "external_ids": ["m1"] }).to_string(),
                },
            ]),
            // Turn 1: no calls → the loop ends.
            crate::test_util::mock::no_tools("done"),
        ]);

        let outcome = extract_for_channel(
            &pool,
            &llm,
            channel_id,
            "test-model",
            DEFAULT_WINDOW_CHAR_BUDGET,
            Duration::from_secs(60),
            None,
        )
        .await
        .expect("loop should finish");
        assert_eq!(outcome.result, "ok");

        let seen = llm.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "two turns sent");
        assert!(
            seen.iter().all(|(_, prev)| prev.is_none()),
            "must never thread via previous_response_id"
        );
        // Turn 0 is just the kickoff user message.
        assert_eq!(seen[0].0.len(), 1, "turn 0 is the kickoff message only");
        // Turn 1 replays the prior exchange — user msg + the assistant's
        // fetch_messages call + its output — and nothing else (reasoning dropped).
        let t1 = &seen[1].0;
        assert_eq!(
            t1.len(),
            3,
            "turn 1 should be exactly [user, function_call, function_call_output]; got {t1:?}"
        );
        assert!(matches!(
            &t1[0],
            InputItem::Message {
                role: Role::User,
                ..
            }
        ));
        assert!(
            matches!(&t1[1], InputItem::FunctionCall { name, .. } if name == "fetch_messages"),
            "the assistant's tool call must be replayed"
        );
        assert!(
            matches!(&t1[2], InputItem::FunctionCallOutput { call_id, .. } if call_id == "c1"),
            "the tool output must follow with the matching call_id"
        );
    }

    #[tokio::test]
    async fn extract_for_channel_surfaces_unsplittable_overflow() {
        // A single message that overflows the context can't be split further,
        // so the error is recorded and surfaced rather than looping.
        let (_tmp, pool, _source_id, channel_id) = setup().await.unwrap();
        let llm = SequencedLlm::new(vec![Err("LLM API error (HTTP 400 Bad Request): \
             Prompt too long: 40000 tokens exceeds max context window of 32768 tokens"
            .to_string())]);

        let result = extract_for_channel(
            &pool,
            &llm,
            channel_id,
            "test-model",
            DEFAULT_WINDOW_CHAR_BUDGET,
            Duration::from_secs(60),
            None,
        )
        .await;
        assert!(result.is_err(), "an unsplittable overflow should surface");

        let (err_runs,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM extraction_runs WHERE channel_id = ? AND result = 'error'",
        )
        .bind(channel_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(err_runs, 1);
        assert_eq!(load_watermark(&pool, channel_id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn extract_for_channel_propagates_agent_failure() -> Result<()> {
        // Pin: when the LLM call fails (e.g. context window exceeded), the
        // failure has to bubble back to the orchestrator so the toast can
        // surface it. Previously we caught the error inside
        // extract_for_channel, wrote result='error' to extraction_runs, and
        // returned Ok — so per-channel failures were invisible in the UI.
        let (_tmp, pool, _source_id, channel_id) = setup().await?;
        let llm = FailingLlm(
            "LLM API error (HTTP 400 Bad Request): Prompt too long: 154806 \
             tokens exceeds max context window of 131072 tokens",
        );
        let result = extract_for_channel(
            &pool,
            &llm,
            channel_id,
            "test-model",
            DEFAULT_WINDOW_CHAR_BUDGET,
            Duration::from_secs(60),
            None,
        )
        .await;
        let err = result.expect_err("extract_for_channel should return Err when the agent fails");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("context window"),
            "error chain should contain transport detail: {msg}"
        );

        // It still records the run so re-extraction can see what happened.
        let (recorded_result, summary): (String, Option<String>) = sqlx::query_as(
            "SELECT result, summary FROM extraction_runs WHERE channel_id = ? ORDER BY ran_at DESC LIMIT 1",
        )
        .bind(channel_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(recorded_result, "error");
        assert!(summary.unwrap_or_default().contains("context window"));
        Ok(())
    }

    #[tokio::test]
    async fn update_action_amends_fields_and_appends_evidence() -> Result<()> {
        // Pin: the extractor needs a way to amend an action it (or a prior
        // run) already created — without that, the only way to "revise" is
        // to call record_action again, which produces near-duplicates.
        // update_action takes the A-N id returned by record_action, patches
        // only the provided fields, appends new evidence rather than
        // replacing it, and logs an 'amended' event.
        let (_tmp, pool, source_id, channel_id) = setup().await?;
        ingest_extra_message(&pool, source_id, channel_id, "msg-2").await?;
        let scope = ExtractionScope {
            source_id,
            channel_id,
        };

        // First, create an action and capture its id from the tool output.
        let create_args = serde_json::json!({
            "title": "Take a look",
            "details": "Ana asked you to review.",
            "confidence": "medium",
            "rationale": "Implied ask",
            "evidence_external_ids": ["msg-1"]
        })
        .to_string();
        let created = tools::dispatch(
            &pool,
            scope.tool_scope(),
            &mut tools::FetchBudget::unlimited(),
            "record_action",
            &create_args,
        )
        .await;
        assert!(created.recorded_action);
        let created_json: serde_json::Value = serde_json::from_str(&created.output)?;
        let action_ref = created_json
            .get("action_id")
            .and_then(|v| v.as_str())
            .expect("record_action should return action_id")
            .to_string();

        // Amend: tighten title, bump confidence, add a second evidence msg.
        let update_args = serde_json::json!({
            "action_id": action_ref,
            "title": "Review Ana's draft and reply",
            "confidence": "high",
            "evidence_external_ids": ["msg-2"]
        })
        .to_string();
        let updated = tools::dispatch(
            &pool,
            scope.tool_scope(),
            &mut tools::FetchBudget::unlimited(),
            "update_action",
            &update_args,
        )
        .await;
        assert!(
            !updated.output.contains("error"),
            "update_action returned an error: {}",
            updated.output
        );
        assert!(
            !updated.recorded_action,
            "update_action should not be counted as a new recording"
        );

        // Action state reflects the amend.
        let (title, confidence, status, details): (String, String, String, Option<String>) =
            sqlx::query_as("SELECT title, confidence, status, details FROM actions LIMIT 1")
                .fetch_one(&pool)
                .await?;
        assert_eq!(title, "Review Ana's draft and reply");
        assert_eq!(confidence, "high");
        // Bumping medium→high should promote pending→auto_claimed.
        assert_eq!(status, "auto_claimed");
        // Untouched field stays put.
        assert_eq!(details.as_deref(), Some("Ana asked you to review."));

        // Evidence appended, not replaced.
        let (evidence_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM action_evidence")
            .fetch_one(&pool)
            .await?;
        assert_eq!(evidence_count, 2);

        // Audit trail: 'created' + 'amended'.
        let kinds: Vec<(String,)> =
            sqlx::query_as("SELECT event_kind FROM action_events ORDER BY id")
                .fetch_all(&pool)
                .await?;
        let kinds: Vec<String> = kinds.into_iter().map(|(k,)| k).collect();
        assert_eq!(kinds, vec!["created".to_string(), "amended".to_string()]);

        Ok(())
    }

    /// Helper: insert a pending action linked to msg-1 from `setup()`. Returns
    /// (action_id, A-N reference string) so resolve_action tests can target it.
    async fn seed_pending_action(pool: &SqlitePool, title: &str) -> Result<(i64, String)> {
        let now = Utc::now().timestamp();
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO actions (title, details, confidence, status, extracted_at) \
             VALUES (?, 'd', 'medium', 'pending', ?) RETURNING id",
        )
        .bind(title)
        .bind(now)
        .fetch_one(pool)
        .await?;
        let (msg_id,): (i64,) =
            sqlx::query_as("SELECT id FROM messages WHERE external_id = 'msg-1'")
                .fetch_one(pool)
                .await?;
        sqlx::query(
            "INSERT INTO action_evidence (action_id, message_id, kind, is_primary) \
             VALUES (?, ?, 'source', 1)",
        )
        .bind(id)
        .bind(msg_id)
        .execute(pool)
        .await?;
        Ok((id, format!("A-{id}")))
    }

    #[tokio::test]
    async fn resolve_action_high_confidence_auto_applies_done() -> Result<()> {
        // Pin: when the extractor calls resolve_action with confidence='high'
        // the action transitions to 'done' immediately, resolved_at is set,
        // and the audit log records 'resolved' with actor='agent_auto'. The
        // evidence messages attach with kind='resolution' so the UI can
        // distinguish "what created this action" from "what resolved it".
        let (_tmp, pool, source_id, channel_id) = setup().await?;
        let (action_id, action_ref) = seed_pending_action(&pool, "Ship the draft").await?;

        let scope = ExtractionScope {
            source_id,
            channel_id,
        };
        let args = serde_json::json!({
            "action_id": action_ref,
            "status": "done",
            "confidence": "high",
            "rationale": "Ana confirmed the draft landed.",
            "evidence_external_ids": ["msg-1"],
        })
        .to_string();
        let out = tools::dispatch(
            &pool,
            scope.tool_scope(),
            &mut tools::FetchBudget::unlimited(),
            "resolve_action",
            &args,
        )
        .await;
        assert!(
            !out.output.contains("error"),
            "resolve_action errored: {}",
            out.output
        );

        let (status, resolved_at): (String, Option<i64>) =
            sqlx::query_as("SELECT status, resolved_at FROM actions WHERE id = ?")
                .bind(action_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(status, "done");
        assert!(resolved_at.is_some(), "resolved_at must be set");

        let (resolution_evidence,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM action_evidence \
             WHERE action_id = ? AND kind = 'resolution'",
        )
        .bind(action_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            resolution_evidence, 1,
            "expected the evidence_external_ids to attach as a 'resolution' evidence row"
        );

        let (event_kind, actor): (String, String) = sqlx::query_as(
            "SELECT event_kind, actor FROM action_events WHERE action_id = ? \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(action_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(event_kind, "resolved");
        assert_eq!(actor, "agent_auto");
        Ok(())
    }

    #[tokio::test]
    async fn resolve_action_medium_confidence_queues_as_suggestion() -> Result<()> {
        // Medium/low confidence MUST NOT auto-resolve — the user has to
        // confirm. The audit log gets a 'suggested_resolution' event with
        // actor='agent_queued' so the UI can surface it in the suggestions
        // panel; the action itself stays in its prior state.
        let (_tmp, pool, source_id, channel_id) = setup().await?;
        let (action_id, action_ref) = seed_pending_action(&pool, "Maybe ship the draft").await?;

        let scope = ExtractionScope {
            source_id,
            channel_id,
        };
        let args = serde_json::json!({
            "action_id": action_ref,
            "status": "done",
            "confidence": "medium",
            "rationale": "Looks like Ana mentioned it shipped.",
            "evidence_external_ids": ["msg-1"],
        })
        .to_string();
        let out = tools::dispatch(
            &pool,
            scope.tool_scope(),
            &mut tools::FetchBudget::unlimited(),
            "resolve_action",
            &args,
        )
        .await;
        assert!(
            !out.output.contains("\"error\""),
            "unexpected error: {}",
            out.output
        );
        assert!(
            out.output.contains("\"applied\":false"),
            "medium-conf should report applied=false. got: {}",
            out.output
        );

        // Action state untouched.
        let (status, resolved_at): (String, Option<i64>) =
            sqlx::query_as("SELECT status, resolved_at FROM actions WHERE id = ?")
                .bind(action_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(status, "pending");
        assert!(
            resolved_at.is_none(),
            "queued suggestion must not set resolved_at"
        );

        // Suggestion event present.
        let (event_kind, actor, data_json): (String, String, String) = sqlx::query_as(
            "SELECT event_kind, actor, data_json FROM action_events \
             WHERE action_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(action_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(event_kind, "suggested_resolution");
        assert_eq!(actor, "agent_queued");
        assert!(
            data_json.contains("\"status\":\"done\""),
            "data_json should carry the proposed status. got: {data_json}"
        );
        assert!(
            data_json.contains("\"confidence\":\"medium\""),
            "data_json should carry the confidence. got: {data_json}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn load_feedback_for_pulls_channel_source_and_global_capped() -> Result<()> {
        // Pin the scoping rule + cap: union of channel + source + global,
        // ordered by most-recent, capped at 10. The extractor renders these
        // as negative examples; a too-loose cap or wrong scoping makes the
        // section worse than useless.
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        // Seed another source + channel that should NOT contribute.
        let (other_source,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'other', 'kc/other', ?) RETURNING id",
        )
        .bind(Utc::now().timestamp())
        .fetch_one(&pool)
        .await?;
        let (other_channel,): (i64,) = sqlx::query_as(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, 'INBOX', 'INBOX', 'mailbox') RETURNING id",
        )
        .bind(other_source)
        .fetch_one(&pool)
        .await?;

        let now = Utc::now().timestamp();
        let insert = |scope_kind: &'static str,
                      scope_id: Option<i64>,
                      kind: &'static str,
                      example: String,
                      ts: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO dismissal_feedback \
                     (scope_kind, scope_id, example_text, reason, created_at, kind) \
                     VALUES (?, ?, ?, '', ?, ?)",
                )
                .bind(scope_kind)
                .bind(scope_id)
                .bind(example)
                .bind(ts)
                .bind(kind)
                .execute(&pool)
                .await
                .map(|_| ())
            }
        };
        // In-scope (channel + source + global).
        insert(
            "channel",
            Some(channel_id),
            "dismissed",
            "ch-row".into(),
            now,
        )
        .await?;
        insert(
            "source",
            Some(source_id),
            "wrong_auto_claim",
            "src-row".into(),
            now - 1,
        )
        .await?;
        insert("global", None, "dismissed", "glob-row".into(), now - 2).await?;
        // Out-of-scope (other source + other channel) must be excluded.
        insert(
            "channel",
            Some(other_channel),
            "dismissed",
            "other-ch".into(),
            now,
        )
        .await?;
        insert(
            "source",
            Some(other_source),
            "dismissed",
            "other-src".into(),
            now,
        )
        .await?;
        // Cap: insert 12 more in-scope rows and assert the loader returns 10.
        for i in 0..12 {
            insert(
                "channel",
                Some(channel_id),
                "dismissed",
                format!("noise-{i}"),
                now + 100 + i,
            )
            .await?;
        }

        let rows = load_feedback_for(&pool, source_id, channel_id).await?;
        assert_eq!(rows.len(), 10, "cap not honored");
        // No leakage from the unrelated source/channel.
        for f in &rows {
            assert!(
                !f.example_text.starts_with("other-"),
                "leaked out-of-scope feedback: {}",
                f.example_text
            );
        }
        // Kinds round-trip.
        assert!(
            rows.iter()
                .any(|f| matches!(f.kind, FeedbackKind::Dismissed)),
            "no dismissed row returned"
        );
        Ok(())
    }

    #[tokio::test]
    async fn medium_confidence_stays_pending() -> Result<()> {
        let (_tmp, pool, source_id, channel_id) = setup().await?;

        let args = serde_json::json!({
            "title": "Maybe do this",
            "details": "Not sure if for you.",
            "confidence": "medium",
            "rationale": "Implied",
            "evidence_external_ids": ["msg-1"]
        })
        .to_string();

        let scope = ExtractionScope {
            source_id,
            channel_id,
        };
        tools::dispatch(
            &pool,
            scope.tool_scope(),
            &mut tools::FetchBudget::unlimited(),
            "record_action",
            &args,
        )
        .await;

        let (status, actor): (String, String) = sqlx::query_as(
            "SELECT a.status, ae.actor FROM actions a \
             JOIN action_events ae ON ae.action_id = a.id LIMIT 1",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(status, "pending");
        assert_eq!(actor, "agent_queued");

        Ok(())
    }
}
