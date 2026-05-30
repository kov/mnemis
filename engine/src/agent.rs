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

/// What happened on a send attempt that didn't immediately succeed — handed to
/// the caller's hook so it can record the attempt (e.g. into an extraction
/// trace) without this module depending on any particular tracer.
pub enum StallEvent<'a> {
    /// A transient stall that will be retried after a backoff.
    Retry { attempt: usize, error: &'a str },
    /// The send failed terminally (non-transient, or retries exhausted).
    GaveUp { error: &'a str },
}

/// Send one turn to the LLM, retrying transient server stalls with backoff.
///
/// `history` is cloned per attempt (the transport consumes it), so the caller
/// keeps ownership. `on_event` is invoked for each retry and on terminal
/// failure — a no-op closure is fine when the caller doesn't trace. On final
/// failure the error is annotated with the turn number. Both agent loops route
/// every send through here so the stall handling stays identical.
pub(crate) async fn send_with_stall_retry(
    llm: &dyn LlmTransport,
    system_prompt: &str,
    history: &[InputItem],
    tools: &[ToolDef],
    turn: usize,
    mut on_event: impl FnMut(StallEvent<'_>),
) -> Result<ResponsesResponse> {
    let mut attempt = 0usize;
    loop {
        match llm.send(system_prompt, history.to_vec(), tools, None).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                let chain = format!("{e:#}");
                if is_transient_stall(&chain) && attempt < STALL_RETRIES {
                    attempt += 1;
                    warn!(
                        turn,
                        attempt, "LLM stalled (empty body / timeout); retrying after backoff"
                    );
                    on_event(StallEvent::Retry {
                        attempt,
                        error: &chain,
                    });
                    tokio::time::sleep(STALL_BACKOFF * attempt as u32).await;
                    continue;
                }
                on_event(StallEvent::GaveUp { error: &chain });
                return Err(e).with_context(|| format!("LLM send failed on turn {turn}"));
            }
        }
    }
}
