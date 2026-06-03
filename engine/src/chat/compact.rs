//! Context-window compaction for chat conversations.
//!
//! Chat replays the *whole* conversation to the model each step
//! ([`store::build_history`]), so a long chat — or one that fetched several big
//! message bodies — eventually exceeds the server's context window. This module
//! keeps that bounded: when the reconstructed input approaches the window, the
//! oldest turns are folded into a running summary (an [`crate::chat::store`]
//! checkpoint) and only a recent verbatim *tail* is replayed. `chat_turns` is
//! never touched, so the UI still shows the full conversation, and the model
//! can pull exact earlier wording back with the `search_conversation` /
//! `recall_turns` tools.
//!
//! Two correctness rules drive the boundary choice:
//! - **Never split a tool round-trip.** A `function_call` and its
//!   `function_call_output` must both be on the same side of the cut, or the
//!   Responses API rejects the orphan. We only cut where the running
//!   call/output balance is zero.
//! - **One irreducible unit can't be rescued.** If the most-recent indivisible
//!   unit (a single message, or one call+output pair) alone won't fit alongside
//!   the system prompt + a reply, no amount of summarizing the rest helps — the
//!   caller surfaces a clear error instead of looping. That's the ~80%-of-window
//!   guard.

use std::time::Duration;

use anyhow::Result;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use mnemis_types::ChatEvent;

use crate::agent;
use crate::chat::ChatSink;
use crate::chat::store::{self, TurnRow};
use crate::extract::trace::TraceWriter;
use crate::llm::{
    ContentItem, InputItem, LlmTransport, OutputItem, ResponsesResponse, Role, ToolDef,
};

/// Rough chars-per-token, mirroring `extract::CHARS_PER_TOKEN` (English ~4).
/// On the low side so the estimate errs toward *over*-counting tokens — i.e.
/// toward compacting a little early rather than overflowing.
const CHARS_PER_TOKEN: usize = 4;

const COMPACT_SYSTEM: &str = "You compress conversation history for an assistant's working \
    memory. You are given the earlier part of a conversation (and possibly a prior summary of \
    even-earlier turns). Produce a concise summary that preserves everything load-bearing: what \
    the user asked for or wants, decisions and conclusions reached, any action items \
    created/updated/resolved with their A-N ids, key facts learned from messages with their \
    external_ids, and any open or unfinished threads. Be factual and terse — this summary \
    replaces the raw turns in the assistant's context, so omit nothing important but don't pad. \
    The assistant can retrieve exact earlier wording on demand, so you needn't quote verbatim. \
    Reply with the summary only.";

/// Budgets derived from the server's context window (in tokens). Proportions
/// are fixed for now (configurable later); the constructor floors a silly-small
/// window so the math can't divide a conversation into nothing.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Budgets {
    max_context_tokens: usize,
}

impl Budgets {
    pub(crate) fn new(max_context_tokens: usize) -> Self {
        Self {
            max_context_tokens: max_context_tokens.max(2048),
        }
    }

    /// Compact proactively once the estimated input crosses this.
    fn compact_trigger_tokens(&self) -> usize {
        self.max_context_tokens * 70 / 100
    }

    /// Target size of the verbatim recent tail kept after compaction.
    fn tail_tokens(&self) -> usize {
        self.max_context_tokens * 20 / 100
    }

    /// Hard cap on the stored summary's own length.
    fn summary_cap_tokens(&self) -> usize {
        self.max_context_tokens * 15 / 100
    }

    /// A single indivisible unit at/above this can't be made to fit — error
    /// out. (The user's "~80% of the context" rule.)
    fn single_unit_error_tokens(&self) -> usize {
        self.max_context_tokens * 80 / 100
    }

    /// Room reserved for the model's reply on top of the input.
    fn gen_reserve_tokens(&self) -> usize {
        (self.max_context_tokens / 10).max(1024)
    }
}

/// Outcome of a compaction attempt.
pub(crate) enum Compacted {
    /// Older turns were folded into the summary; the next send is smaller.
    Folded,
    /// Nothing left to fold (the tail is already the whole conversation).
    NothingToFold,
    /// The minimal tail is itself too big to fit — compaction can't help.
    Stuck,
    /// The user pressed Stop mid-summary; no checkpoint was written. The caller
    /// ends the turn cleanly (the same Stop also cancels the pending send).
    Cancelled,
}

fn chars_to_tokens(chars: usize) -> usize {
    chars / CHARS_PER_TOKEN + 1
}

fn tool_chars(t: &ToolDef) -> usize {
    t.name.len() + t.description.len() + t.parameters.to_string().len() + 16
}

fn item_chars(item: &InputItem) -> usize {
    match item {
        InputItem::Message { content, .. } => content.len() + 16,
        InputItem::FunctionCall {
            name, arguments, ..
        } => name.len() + arguments.len() + 32,
        InputItem::FunctionCallOutput { output, .. } => output.len() + 32,
    }
}

/// Estimate the token cost of a full request: instructions + tool schemas +
/// the conversation. Chars/4 — cheap and tokenizer-free, deliberately a slight
/// over-estimate (see [`CHARS_PER_TOKEN`]).
pub(crate) fn estimate_input_tokens(
    system_prompt: &str,
    tools: &[ToolDef],
    history: &[InputItem],
) -> usize {
    let tool_chars: usize = tools.iter().map(tool_chars).sum();
    let hist_chars: usize = history.iter().map(item_chars).sum();
    chars_to_tokens(system_prompt.len() + tool_chars + hist_chars)
}

fn row_tokens(row: &TurnRow) -> usize {
    store::row_to_input(row)
        .map(|i| chars_to_tokens(item_chars(&i)))
        .unwrap_or(0)
}

/// The fixed overhead a send always carries, regardless of the conversation:
/// system prompt + tool schemas + the reply reserve.
fn reserve_tokens(system_prompt: &str, tools: &[ToolDef], budgets: Budgets) -> usize {
    estimate_input_tokens(system_prompt, tools, &[]) + budgets.gen_reserve_tokens()
}

enum PlanOutcome {
    Fold { boundary: usize },
    NothingToFold,
    Stuck,
}

/// Choose where to cut `rows` (the turns past the current checkpoint) into a
/// folded prefix `rows[..boundary]` and a verbatim tail `rows[boundary..]`.
fn plan_compaction(rows: &[TurnRow], budgets: Budgets, reserve: usize) -> PlanOutcome {
    let n = rows.len();
    if n == 0 {
        return PlanOutcome::NothingToFold;
    }

    // Per-row token sizes and suffix sums (suffix[i] = tokens of rows[i..]).
    let sizes: Vec<usize> = rows.iter().map(row_tokens).collect();
    let mut suffix = vec![0usize; n + 1];
    for i in (0..n).rev() {
        suffix[i] = suffix[i + 1] + sizes[i];
    }

    // Running call/output balance: a cut at i (tail = rows[i..]) is safe only
    // when every function_call before i already has its output before i, i.e.
    // open[i] == 0 — that's what guarantees we never orphan a tool pair.
    let mut open = vec![0i64; n + 1];
    for i in 0..n {
        let delta = match (
            rows[i].role.as_str(),
            rows[i].tool_name.is_some(),
            rows[i].tool_call_id.is_some(),
        ) {
            ("assistant", true, true) => 1, // a function_call
            ("tool", _, true) => -1,        // its matching output
            _ => 0,
        };
        open[i + 1] = open[i] + delta;
    }
    let is_safe = |i: usize| open[i] == 0;

    // Smallest non-empty tail = the latest complete unit = largest safe i < n.
    let b_min = (0..n).rev().find(|&i| is_safe(i)).unwrap_or(0);
    let tail_min = suffix[b_min];
    if tail_min >= budgets.single_unit_error_tokens()
        || tail_min + reserve >= budgets.max_context_tokens
    {
        return PlanOutcome::Stuck;
    }

    // Grow the tail back to the smallest safe boundary still within the tail
    // budget (suffix is monotonically decreasing in i, so the first fit is the
    // largest tail that fits). Falls back to b_min when even one unit exceeds
    // the tail budget — we must still keep the latest unit.
    let boundary = (0..=b_min)
        .find(|&i| is_safe(i) && suffix[i] <= budgets.tail_tokens())
        .unwrap_or(b_min);

    if boundary == 0 {
        PlanOutcome::NothingToFold
    } else {
        PlanOutcome::Fold { boundary }
    }
}

/// Proactive compaction: if the reconstructed input is over the trigger, fold
/// the oldest turns into the summary. Returns whether it actually folded.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn maybe_compact(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    chat_id: i64,
    system_prompt: &str,
    tools: &[ToolDef],
    budgets: Budgets,
    idle_timeout: Duration,
    cancel: &CancellationToken,
    sink: ChatSink<'_>,
    trace: Option<&mut TraceWriter>,
) -> Result<Compacted> {
    let history = store::build_history(pool, chat_id).await?;
    if estimate_input_tokens(system_prompt, tools, &history) <= budgets.compact_trigger_tokens() {
        return Ok(Compacted::NothingToFold);
    }
    compact(
        pool,
        llm,
        chat_id,
        system_prompt,
        tools,
        budgets,
        idle_timeout,
        cancel,
        sink,
        trace,
    )
    .await
}

/// Fold the oldest foldable turns into the summary checkpoint (one or more
/// summarizer calls). Emits [`ChatEvent::Compacting`] only when it will do real
/// work. Idempotent-ish: callable again to fold further (the watermark
/// advances each time).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn compact(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    chat_id: i64,
    system_prompt: &str,
    tools: &[ToolDef],
    budgets: Budgets,
    idle_timeout: Duration,
    cancel: &CancellationToken,
    sink: ChatSink<'_>,
    trace: Option<&mut TraceWriter>,
) -> Result<Compacted> {
    let checkpoint = store::latest_summary(pool, chat_id).await?;
    let watermark = checkpoint.as_ref().map(|c| c.up_to_turn_id).unwrap_or(0);
    let rows = store::load_turn_rows(pool, chat_id, watermark).await?;
    let reserve = reserve_tokens(system_prompt, tools, budgets);

    let boundary = match plan_compaction(&rows, budgets, reserve) {
        PlanOutcome::Fold { boundary } => boundary,
        PlanOutcome::NothingToFold => return Ok(Compacted::NothingToFold),
        PlanOutcome::Stuck => return Ok(Compacted::Stuck),
    };

    sink(ChatEvent::Compacting);
    let prefix = &rows[..boundary];
    let new_watermark = rows[boundary - 1].id;
    if let Some(w) = trace {
        w.compaction(prefix.len(), new_watermark);
    }

    let summary = match summarize_prefix(
        llm,
        budgets,
        idle_timeout,
        cancel,
        checkpoint.as_ref().map(|c| c.summary.as_str()),
        prefix,
    )
    .await?
    {
        Some(s) => s,
        // Stop pressed mid-summary: leave the checkpoint untouched so a retry
        // re-folds the same span from scratch.
        None => return Ok(Compacted::Cancelled),
    };
    store::insert_summary(pool, chat_id, new_watermark, &summary).await?;
    info!(
        chat_id,
        folded = prefix.len(),
        new_watermark,
        summary_chars = summary.len(),
        "chat: compacted conversation"
    );
    Ok(Compacted::Folded)
}

/// Build the history to actually send, dropping the leading summary message
/// when the verbatim tail is itself big enough that also carrying the recap
/// would overflow. The summary stays in the DB either way — the recall tools
/// can still reach the original turns.
pub(crate) async fn history_for_send(
    pool: &SqlitePool,
    chat_id: i64,
    system_prompt: &str,
    tools: &[ToolDef],
    budgets: Budgets,
) -> Result<Vec<InputItem>> {
    let checkpoint = store::latest_summary(pool, chat_id).await?;
    let after = checkpoint.as_ref().map(|c| c.up_to_turn_id).unwrap_or(0);
    let rows = store::load_turn_rows(pool, chat_id, after).await?;
    let tail: Vec<InputItem> = rows.iter().filter_map(store::row_to_input).collect();

    if let Some(c) = &checkpoint {
        let mut with = Vec::with_capacity(tail.len() + 1);
        with.push(store::summary_input_item(&c.summary));
        with.extend(tail.iter().cloned());
        let ceiling = budgets
            .max_context_tokens
            .saturating_sub(budgets.gen_reserve_tokens());
        if estimate_input_tokens(system_prompt, tools, &with) <= ceiling {
            return Ok(with);
        }
        warn!(
            chat_id,
            "chat: tail too large to also send the summary; sending tail only"
        );
    }
    Ok(tail)
}

/// Fold `prefix` (plus any prior summary) into a fresh summary, in chunks that
/// each fit the window — so even a reactive compaction of an already-oversize
/// conversation never overflows the summarizer call itself.
async fn summarize_prefix(
    llm: &dyn LlmTransport,
    budgets: Budgets,
    idle_timeout: Duration,
    cancel: &CancellationToken,
    prev_summary: Option<&str>,
    prefix: &[TurnRow],
) -> Result<Option<String>> {
    // Leave room in each summarizer call for the prior summary + system prompt.
    let chunk_budget = budgets.max_context_tokens * 45 / 100;
    let mut summary = prev_summary.map(str::to_string);
    let mut chunk = String::new();
    let mut chunk_tokens = summary
        .as_deref()
        .map(|s| chars_to_tokens(s.len()))
        .unwrap_or(0);

    for row in prefix {
        let rendered = render_turn(row);
        let t = chars_to_tokens(rendered.len());
        if !chunk.is_empty() && chunk_tokens + t > chunk_budget {
            match summarize_call(
                llm,
                budgets,
                idle_timeout,
                cancel,
                summary.as_deref(),
                &chunk,
            )
            .await?
            {
                Some(s) => summary = Some(s),
                None => return Ok(None),
            }
            chunk.clear();
            chunk_tokens = summary
                .as_deref()
                .map(|s| chars_to_tokens(s.len()))
                .unwrap_or(0);
        }
        chunk.push_str(&rendered);
        chunk.push('\n');
        chunk_tokens += t;
    }
    if !chunk.trim().is_empty() {
        match summarize_call(
            llm,
            budgets,
            idle_timeout,
            cancel,
            summary.as_deref(),
            &chunk,
        )
        .await?
        {
            Some(s) => summary = Some(s),
            None => return Ok(None),
        }
    }
    Ok(Some(summary.unwrap_or_default()))
}

/// One summarizer round-trip (tool-less). Caps the result to the summary
/// budget so the running summary can't grow unbounded across folds.
async fn summarize_call(
    llm: &dyn LlmTransport,
    budgets: Budgets,
    idle_timeout: Duration,
    cancel: &CancellationToken,
    prev_summary: Option<&str>,
    chunk: &str,
) -> Result<Option<String>> {
    let mut input = String::new();
    if let Some(p) = prev_summary {
        input.push_str("Summary so far:\n");
        input.push_str(p);
        input.push_str("\n\n---\n");
    }
    input.push_str("Conversation to fold into the summary:\n");
    input.push_str(chunk);

    let history = vec![InputItem::Message {
        role: Role::User,
        content: input,
    }];
    // Stream the summarizer too: its prompt can be a large fraction of the
    // window, so the idle timeout (not the short non-streaming total timeout)
    // is what should bound it. No live rendering (deltas: None), but the real
    // cancel token is wired in so Stop interrupts the "Condensing…" phase too.
    let stream = agent::StreamCtx {
        idle_timeout,
        cancel: cancel.clone(),
        deltas: None,
    };
    let response =
        agent::send_with_stall_retry(llm, COMPACT_SYSTEM, &history, &[], 0, Some(&stream), |_| {})
            .await?;

    // Stop pressed mid-summary: the transport returns a cancelled response.
    // Don't treat its (possibly empty) text as a real summary.
    if cancel.is_cancelled() || response.status == "cancelled" {
        return Ok(None);
    }

    let mut text = collect_text(&response);
    if text.trim().is_empty() {
        anyhow::bail!("summarizer returned empty text");
    }
    let cap_chars = budgets.summary_cap_tokens() * CHARS_PER_TOKEN;
    if text.chars().count() > cap_chars {
        text = text.chars().take(cap_chars).collect();
    }
    Ok(Some(text))
}

/// Render a turn as one readable line for the summarizer's input.
fn render_turn(row: &TurnRow) -> String {
    let content = row.content.as_deref().unwrap_or("");
    match (row.role.as_str(), row.tool_name.as_deref()) {
        ("user", _) => format!("User: {content}"),
        ("assistant", Some(name)) => format!("Assistant called tool {name} with {content}"),
        ("assistant", None) => format!("Assistant: {content}"),
        ("tool", Some(name)) => format!("Tool {name} returned: {content}"),
        ("tool", None) => format!("Tool returned: {content}"),
        _ => content.to_string(),
    }
}

fn collect_text(response: &ResponsesResponse) -> String {
    let mut s = String::new();
    for item in &response.output {
        if let OutputItem::Message { content } = item {
            for c in content {
                if let ContentItem::OutputText { text } = c {
                    s.push_str(text);
                }
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_row(id: i64, chars: usize) -> TurnRow {
        TurnRow {
            id,
            role: "user".to_string(),
            content: Some("x".repeat(chars)),
            tool_name: None,
            tool_call_id: None,
        }
    }
    fn assistant_row(id: i64, chars: usize) -> TurnRow {
        TurnRow {
            id,
            role: "assistant".to_string(),
            content: Some("x".repeat(chars)),
            tool_name: None,
            tool_call_id: None,
        }
    }
    fn call_row(id: i64, call_id: &str) -> TurnRow {
        TurnRow {
            id,
            role: "assistant".to_string(),
            content: Some("{}".to_string()),
            tool_name: Some("fetch_messages".to_string()),
            tool_call_id: Some(call_id.to_string()),
        }
    }
    fn tool_row(id: i64, call_id: &str, chars: usize) -> TurnRow {
        TurnRow {
            id,
            role: "tool".to_string(),
            content: Some("x".repeat(chars)),
            tool_name: Some("fetch_messages".to_string()),
            tool_call_id: Some(call_id.to_string()),
        }
    }

    /// The call/output balance of a prefix — must be zero at the cut, else a
    /// tool pair is orphaned across it.
    fn open_balance(rows: &[TurnRow]) -> i64 {
        rows.iter()
            .map(|r| {
                match (
                    r.role.as_str(),
                    r.tool_name.is_some(),
                    r.tool_call_id.is_some(),
                ) {
                    ("assistant", true, true) => 1,
                    ("tool", _, true) => -1,
                    _ => 0,
                }
            })
            .sum()
    }

    #[test]
    fn nothing_to_fold_when_the_whole_chat_fits_the_tail() {
        let rows = vec![user_row(1, 100)];
        let b = Budgets::new(2048);
        assert!(matches!(
            plan_compaction(&rows, b, 200),
            PlanOutcome::NothingToFold
        ));
    }

    #[test]
    fn stuck_when_a_single_unit_exceeds_the_window() {
        let b = Budgets::new(2048);
        // One message bigger than the 80%-of-window single-unit limit.
        let huge = b.single_unit_error_tokens() * CHARS_PER_TOKEN + 4_000;
        let rows = vec![user_row(1, huge)];
        assert!(matches!(plan_compaction(&rows, b, 100), PlanOutcome::Stuck));
    }

    #[test]
    fn folds_oldest_turns_keeping_a_recent_tail_within_budget() {
        let b = Budgets::new(2048);
        let rows: Vec<TurnRow> = (1..=40)
            .map(|i| {
                if i % 2 == 0 {
                    assistant_row(i, 300)
                } else {
                    user_row(i, 300)
                }
            })
            .collect();
        let PlanOutcome::Fold { boundary } = plan_compaction(&rows, b, 200) else {
            panic!("expected Fold");
        };
        assert!(boundary > 0 && boundary < rows.len());
        let tail_tokens: usize = rows[boundary..].iter().map(row_tokens).sum();
        assert!(
            tail_tokens <= b.tail_tokens(),
            "tail {tail_tokens} exceeds budget {}",
            b.tail_tokens()
        );
        // The cut is at a safe boundary — no orphaned tool pair.
        assert_eq!(open_balance(&rows[..boundary]), 0);
    }

    #[test]
    fn never_cuts_between_a_tool_call_and_its_output() {
        let b = Budgets::new(2048);
        // Six exchanges with big tool outputs, so the tail budget forces a cut
        // that must avoid splitting any call+output pair.
        let mut rows = Vec::new();
        let mut id = 0;
        for k in 0..6 {
            id += 1;
            rows.push(user_row(id, 50));
            id += 1;
            rows.push(call_row(id, &format!("c{k}")));
            id += 1;
            rows.push(tool_row(id, &format!("c{k}"), 400));
            id += 1;
            rows.push(assistant_row(id, 50));
        }
        let PlanOutcome::Fold { boundary } = plan_compaction(&rows, b, 200) else {
            panic!("expected Fold");
        };
        assert_eq!(
            open_balance(&rows[..boundary]),
            0,
            "prefix must be balanced so no tool pair is orphaned"
        );
        assert_ne!(
            rows[boundary].role, "tool",
            "the tail must not begin with a dangling tool output"
        );
    }
}
