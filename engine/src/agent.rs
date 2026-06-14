//! Primitives shared by the agent loops (extraction and chat).
//!
//! Both loops talk to omlx over the Responses API, reconstruct the
//! conversation client-side each turn, and have to cope with the same failure
//! mode: a degraded server accepts the request, emits an HTTP 200, then stalls
//! mid-generation (it throttles under memory pressure, or wedges on the
//! occasional continuation). That surfaces as an empty body or a read timeout
//! and usually clears on its own once the server recovers — so a transient
//! stall is worth retrying, while a deterministic failure (context overflow,
//! malformed request) is not. This module owns that classification and the
//! retry-with-backoff so the two loops can't drift apart.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::llm::{InputItem, LlmTransport, ResponsesResponse, ToolDef};

/// How many times a turn's LLM send is retried on a *transient* stall — 2
/// retries = 3 attempts total. omlx stalls when it throttles under memory
/// pressure and clears once it evicts a model, so the same request usually
/// succeeds within a retry or two.
pub(crate) const STALL_RETRIES: usize = 2;

/// Base backoff between stall retries, multiplied by the attempt number so the
/// second retry waits longer — giving omlx time to evict a model and recover.
pub(crate) const STALL_BACKOFF: Duration = Duration::from_secs(3);

/// True when an error chain is a *transient* server stall rather than a
/// deterministic failure: omlx accepts the request, emits an HTTP 200, then
/// stalls/aborts generation when it throttles under memory pressure —
/// surfacing as an empty response body or a request/body-read timeout. These
/// clear once the server recovers (often after it evicts a model), so the turn
/// is safe to retry. Deterministic failures (context overflow, malformed
/// request) do not match and bubble straight to the caller. See the
/// `v1-gotchas` memory.
pub(crate) fn is_transient_stall(err_chain: &str) -> bool {
    err_chain.contains("empty response body")
        || err_chain.contains("stalled")
        || err_chain.contains("timed out")
}

/// True when an error chain is the server rejecting a prompt that's too big to
/// process — either by token count or by memory. Two shapes:
/// - **token window** (omlx 400: `Prompt too long: N tokens exceeds max context
///   window of M`) — the prompt is over the model's context window.
/// - **memory** — on a memory-constrained box omlx can accept a prompt that's
///   *within* the token window yet still fail to prefill it: a streaming
///   `Memory limit exceeded during prefill`, or a 507 whose projected memory
///   `would exceed the memory ceiling`. Here the effective limit is RAM, not the
///   token window, so the token-count check alone misses it (this is what let
///   long chats surface a raw error instead of compacting).
///
/// All are deterministic — retrying the identical request can't help — so both
/// agent loops treat them as a signal to *shrink* the input (chat compacts;
/// extraction splits the batch) rather than to retry as-is.
pub(crate) fn is_context_overflow(err_chain: &str) -> bool {
    err_chain.contains("max context window")
        || err_chain.contains("Prompt too long")
        || err_chain.contains("Memory limit exceeded")
        || err_chain.contains("memory ceiling")
}

/// What happened on a send attempt that didn't immediately succeed — handed to
/// the caller's hook so it can record the attempt (e.g. into an extraction
/// trace) without this module depending on any particular tracer.
pub enum StallEvent<'a> {
    /// A transient stall that will be retried after a backoff.
    Retry { attempt: usize, error: &'a str },
    /// The send failed terminally (non-transient, or retries exhausted).
    GaveUp { error: &'a str },
}

/// Streaming controls for a send. `idle_timeout` bounds the gap between SSE
/// events (a longer silence is treated as a stall and retried); `cancel` is the
/// user's Stop button (fires once to abort, returning the partial answer rather
/// than an error); `deltas`, when `Some`, receives assistant output-text chunks
/// for live rendering.
///
/// Built by the interactive chat loop. The compaction summarizer reuses the
/// chat's Stop token with `deltas: None` (idle timeout + Stop, no live
/// rendering). Background callers with neither a Stop button nor live rendering
/// — extraction's agent loop, whose batch prefill can outlast the non-streaming
/// total timeout — use [`StreamCtx::headless`]: a never-fired token, no deltas.
pub(crate) struct StreamCtx {
    pub idle_timeout: Duration,
    pub cancel: CancellationToken,
    pub deltas: Option<UnboundedSender<String>>,
}

impl StreamCtx {
    /// Streaming purely for its idle timeout: no Stop button (a never-fired
    /// cancel token) and no live delta sink. For non-interactive callers like
    /// extraction, where the win is replacing the short total timeout with the
    /// per-chunk idle timeout so a slow-but-progressing batch isn't cut.
    pub(crate) fn headless(idle_timeout: Duration) -> Self {
        Self {
            idle_timeout,
            cancel: CancellationToken::new(),
            deltas: None,
        }
    }
}

/// Send one turn to the LLM, retrying transient server stalls with backoff.
///
/// `history` is cloned per attempt (the transport consumes it), so the caller
/// keeps ownership. When `stream` is `Some`, the send goes over the SSE
/// streaming path ([`LlmTransport::send_stream`]) — idle-timeout bounded,
/// cancellable, delta-forwarding; when `None`, the legacy non-streaming
/// [`LlmTransport::send`] (total-timeout) is used. `on_event` is invoked for
/// each retry and on terminal failure — a no-op closure is fine when the caller
/// doesn't trace. On final failure the error is annotated with the loop `step`.
/// Both agent loops route every send through here so the stall handling stays
/// identical.
pub(crate) async fn send_with_stall_retry(
    llm: &dyn LlmTransport,
    system_prompt: &str,
    history: &[InputItem],
    tools: &[ToolDef],
    step: usize,
    stream: Option<&StreamCtx>,
    mut on_event: impl FnMut(StallEvent<'_>),
) -> Result<ResponsesResponse> {
    let mut attempt = 0usize;
    loop {
        let result = match stream {
            Some(ctx) => {
                llm.send_stream(
                    system_prompt,
                    history.to_vec(),
                    tools,
                    None,
                    ctx.idle_timeout,
                    ctx.cancel.clone(),
                    ctx.deltas.clone(),
                )
                .await
            }
            None => llm.send(system_prompt, history.to_vec(), tools, None).await,
        };
        match result {
            Ok(r) => return Ok(r),
            Err(e) => {
                let chain = format!("{e:#}");
                if is_transient_stall(&chain) && attempt < STALL_RETRIES {
                    attempt += 1;
                    warn!(
                        step,
                        attempt, "LLM stalled (empty body / timeout); retrying after backoff"
                    );
                    on_event(StallEvent::Retry {
                        attempt,
                        error: &chain,
                    });
                    // Back off before retrying, but wake immediately if the user
                    // pressed Stop during the stall — the next attempt then
                    // returns the cancelled (empty) response right away.
                    let backoff = STALL_BACKOFF * attempt as u32;
                    match stream {
                        Some(ctx) => {
                            tokio::select! {
                                _ = tokio::time::sleep(backoff) => {}
                                _ = ctx.cancel.cancelled() => {}
                            }
                        }
                        None => tokio::time::sleep(backoff).await,
                    }
                    continue;
                }
                on_event(StallEvent::GaveUp { error: &chain });
                // "step" (not "turn"): this is the Nth model call while
                // answering one user message, which resets per message — not a
                // count of conversation turns.
                return Err(e).with_context(|| format!("LLM send failed on step {step}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_context_overflow;

    #[test]
    fn token_window_rejections_are_overflow() {
        assert!(is_context_overflow(
            "LLM API error (HTTP 400 Bad Request): Prompt too long: 40000 tokens \
             exceeds max context window of 32768 tokens"
        ));
        assert!(is_context_overflow(
            "... exceeds max context window of 32768"
        ));
    }

    #[test]
    fn memory_pressure_failures_are_overflow() {
        // Streaming prefill OOM: omlx accepts a prompt within the token window
        // but can't prefill it on a memory-constrained box. Previously slipped
        // through as a raw error instead of triggering compaction.
        assert!(is_context_overflow(
            "LLM streaming error: {\"error\":\"Memory limit exceeded during prefill\"}"
        ));
        // 507 projected-memory rejection.
        assert!(is_context_overflow(
            "LLM API error (HTTP 507): projected memory 32.73GB would exceed the \
             memory ceiling 24.00GB"
        ));
    }

    #[test]
    fn transient_and_unrelated_errors_are_not_overflow() {
        assert!(!is_context_overflow("connection reset by peer"));
        assert!(!is_context_overflow("LLM streaming error: timed out"));
        assert!(!is_context_overflow("HTTP 500: internal server error"));
    }
}
