use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Default total request timeout for an LLM call. A degraded local server can
/// accept a request, emit an HTTP 200, then stall mid-generation — omlx does
/// this when it throttles under memory pressure (it eventually aborts the
/// request). With no timeout (reqwest's default) a single `send()` wedges the
/// whole sync indefinitely.
///
/// Under metadata-first extraction the prompt is tiny (metadata + snippets),
/// so prefill is fast and a healthy turn completes in well under a minute —
/// measured against a live omlx server the slowest *legitimate* generation was
/// ~42s, so a request still running at 60s is stalled, not slow. The agent
/// loop treats that timeout as a *transient stall* and retries (see
/// `STALL_RETRIES` in `extract`), so a short timeout that fails fast and
/// retries beats one long dead wait per channel.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 60;

/// Default idle timeout for *streaming* chat sends: the longest the client
/// waits between SSE events before treating the server as wedged. Generous
/// because a large chat prompt's prefill can legitimately produce no output for
/// a while (the original 60s *total* timeout cut exactly those off), and the
/// Stop button is the user's manual override for anything slower.
pub const DEFAULT_CHAT_IDLE_TIMEOUT_SECS: u64 = 180;

/// Connect timeout for the *streaming* client. The streaming send is bounded by
/// an idle timeout (max gap between SSE events), not a total-request timeout —
/// so a slow-but-progressing answer is never cut — but the initial TCP/TLS
/// connect still needs a bound so a dead host fails fast rather than hanging.
const STREAM_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Build the HTTP client with a request timeout. Falls back to a default
/// client (no timeout) only if the TLS/backend init fails — never panics
/// during app startup.
fn build_http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Build the client used for streaming sends: a connect timeout but **no**
/// total-request timeout (reqwest's `.timeout()` would cap the whole stream,
/// killing a legitimately long answer). The per-event idle timeout in
/// [`drive_sse`] is what catches a wedged server instead.
fn build_stream_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(STREAM_CONNECT_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Abstraction over the LLM transport so tests can swap in a scripted mock
/// while production uses the real omlx HTTP client.
#[async_trait]
pub trait LlmTransport: Send + Sync {
    async fn send(
        &self,
        instructions: &str,
        input: Vec<InputItem>,
        tools: &[ToolDef],
        previous_response_id: Option<&str>,
    ) -> Result<ResponsesResponse>;

    /// Streaming variant of [`Self::send`]: the same request with `stream:
    /// true`, but bounded by an **idle** timeout (`idle_timeout` — the max gap
    /// between SSE events) rather than a total-request timeout, so a slow but
    /// steadily-streaming answer is never cut. Abortable mid-flight via
    /// `cancel` (the user's Stop button); on cancel it returns whatever
    /// assistant text streamed so far as a `cancelled` response rather than an
    /// error. Assistant-visible output-text deltas are forwarded to `deltas`
    /// (when `Some`) for live rendering.
    ///
    /// The default delegates to [`Self::send`] (no streaming, idle/cancel/delta
    /// ignored) so mock transports don't have to implement it; the real
    /// [`LlmClient`] overrides it with the SSE path.
    #[allow(clippy::too_many_arguments)]
    async fn send_stream(
        &self,
        instructions: &str,
        input: Vec<InputItem>,
        tools: &[ToolDef],
        previous_response_id: Option<&str>,
        idle_timeout: Duration,
        cancel: CancellationToken,
        deltas: Option<UnboundedSender<String>>,
    ) -> Result<ResponsesResponse> {
        let _ = (idle_timeout, &cancel, &deltas);
        self.send(instructions, input, tools, previous_response_id)
            .await
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    System,
    Assistant,
}

/// A single input item sent to the Responses API.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum InputItem {
    #[serde(rename = "message")]
    Message { role: Role, content: String },
    /// A prior assistant tool call, replayed when we reconstruct the
    /// conversation client-side (see `extract::run_agent_loop`). Its `call_id`
    /// must match the `FunctionCallOutput` that follows it.
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    type_: ToolType,
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
enum ToolType {
    #[serde(rename = "function")]
    Function,
}

impl ToolDef {
    pub fn function(name: String, description: String, parameters: serde_json::Value) -> Self {
        Self {
            type_: ToolType::Function,
            name,
            description,
            parameters,
        }
    }
}

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: Vec<InputItem>,
    tools: &'a [ToolDef],
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<&'a str>,
    /// Ask the server for an SSE event stream. Omitted (not `false`) for the
    /// non-streaming path so that request is byte-for-byte what it was before.
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentItem {
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(other)]
    Unknown,
}

/// Treat an explicit `null` (or an absent field) as `T::default()`. omlx
/// returns *every* output-item field on *every* item, set to `null` when it
/// doesn't apply — so a `Vec` field we expect can arrive as `null`, which a
/// plain `Vec` deserialize rejects with "invalid type: null, expected a
/// sequence". This maps that to an empty collection.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// One entry of a `reasoning` item's `summary` array. omlx emits
/// `{type:"summary_text", text:"…"}`; we only need the text. Unknown fields
/// (like `type`) are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningSummary {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    #[serde(rename = "message")]
    Message {
        #[serde(default, deserialize_with = "null_as_default")]
        content: Vec<ContentItem>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    // The model's chain-of-thought. omlx returns it as a `reasoning` item with
    // `content: null` — the human-readable text lives under a `summary` array
    // of `{type:"summary_text", text}` items, which we capture for the chat UI
    // to display. The agent loops NEVER replay this into model input (the
    // reasoning-replay rule: Qwen/Gemma mishandle a replayed `<think>`). A
    // null/absent summary degrades to an empty Vec.
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default, deserialize_with = "null_as_default")]
        summary: Vec<ReasoningSummary>,
    },
    #[serde(other)]
    Unknown,
}

impl OutputItem {
    /// Join a reasoning item's summary entries into a single display string.
    /// Returns `None` for non-reasoning items or empty summaries.
    pub fn reasoning_text(&self) -> Option<String> {
        match self {
            OutputItem::Reasoning { summary } if !summary.is_empty() => Some(
                summary
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponsesResponse {
    pub id: String,
    pub status: String,
    pub output: Vec<OutputItem>,
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    /// Separate client for streaming sends — no total-request timeout (see
    /// [`build_stream_client`]).
    stream_http: reqwest::Client,
    base_url: String,
    model: String,
    bearer_token: Option<String>,
    timeout_secs: u64,
}

impl LlmClient {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http: build_http_client(DEFAULT_REQUEST_TIMEOUT_SECS),
            stream_http: build_stream_client(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            bearer_token: None,
            timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }

    /// Override the non-streaming request timeout (seconds). Rebuilds the
    /// non-streaming HTTP client; the streaming client is unaffected (it has no
    /// total timeout — it's bounded by a per-call idle timeout instead).
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.http = build_http_client(secs);
        self.timeout_secs = secs;
        self
    }

    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }
}

#[async_trait]
impl LlmTransport for LlmClient {
    async fn send(
        &self,
        instructions: &str,
        input: Vec<InputItem>,
        tools: &[ToolDef],
        previous_response_id: Option<&str>,
    ) -> Result<ResponsesResponse> {
        let url = format!("{}/responses", self.base_url);

        let body = ResponsesRequest {
            model: &self.model,
            instructions,
            input,
            tools,
            previous_response_id,
            stream: false,
        };

        let mut req = self.http.post(&url).json(&body);
        if let Some(token) = &self.bearer_token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                anyhow::anyhow!(
                    "LLM request timed out after {}s before any response (server stalled): {e}",
                    self.timeout_secs
                )
            } else {
                anyhow::Error::new(e).context("LLM request failed")
            }
        })?;
        let status = resp.status();
        let text = match resp.text().await {
            Ok(t) => t,
            // The headers arrived (so `status` is set) but the body stalled and
            // the read timed out — a half-sent response from a server that
            // wedged mid-generation (omlx does this under memory pressure, but
            // also on the occasional stuck `previous_response_id` continuation
            // even when memory is fine).
            Err(e) if e.is_timeout() => bail!(
                "LLM response stalled mid-body and timed out after {}s \
                 (server wedged mid-generation): {e}",
                self.timeout_secs
            ),
            Err(e) => return Err(anyhow::Error::new(e).context("reading LLM response body")),
        };
        if !status.is_success() {
            bail!("LLM API error (HTTP {status}): {text}");
        }
        // A 2xx with an empty body is the server accepting the request and then
        // producing nothing — it stalled/aborted generation (omlx does this
        // under memory pressure, or on a wedged continuation). Name it plainly
        // so it isn't mistaken for a parse bug (`serde_json::from_str("")`
        // reports a cryptic "EOF at line 1").
        if text.trim().is_empty() {
            bail!(
                "LLM returned an empty response body (HTTP {status}); the server accepted \
                 the request then produced no output — the server wedged mid-generation"
            );
        }

        serde_json::from_str(&text).with_context(|| format!("failed to parse LLM response: {text}"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_stream(
        &self,
        instructions: &str,
        input: Vec<InputItem>,
        tools: &[ToolDef],
        previous_response_id: Option<&str>,
        idle_timeout: Duration,
        cancel: CancellationToken,
        deltas: Option<UnboundedSender<String>>,
    ) -> Result<ResponsesResponse> {
        let url = format!("{}/responses", self.base_url);
        let body = ResponsesRequest {
            model: &self.model,
            instructions,
            input,
            tools,
            previous_response_id,
            stream: true,
        };
        let mut req = self.stream_http.post(&url).json(&body);
        if let Some(token) = &self.bearer_token {
            req = req.bearer_auth(token);
        }

        // Open the stream. A Stop pressed before the headers arrive returns an
        // empty cancelled response rather than racing the connect.
        let resp = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(cancelled_response(String::new())),
            r = req.send() => r.context("LLM streaming request failed")?,
        };
        let status = resp.status();
        if !status.is_success() {
            // An error reply isn't an SSE stream — read its (small) body so the
            // overflow/`Prompt too long` classifier in `agent` still sees the
            // server's message.
            let text = resp.text().await.unwrap_or_default();
            bail!("LLM API error (HTTP {status}): {text}");
        }
        let stream = resp
            .bytes_stream()
            .map(|r| r.map(|b| b.to_vec()).map_err(anyhow::Error::from));
        drive_sse(stream, idle_timeout, cancel, deltas).await
    }
}

/// A synthetic response carrying whatever assistant text had streamed when the
/// user pressed Stop. Status `cancelled`; empty output when nothing had been
/// produced yet. The chat loop persists the partial (if any) and ends the turn
/// cleanly — no error, no retry.
fn cancelled_response(text: String) -> ResponsesResponse {
    ResponsesResponse {
        id: String::new(),
        status: "cancelled".to_string(),
        output: if text.is_empty() {
            Vec::new()
        } else {
            vec![OutputItem::Message {
                content: vec![ContentItem::OutputText { text }],
            }]
        },
    }
}

/// Drive a Responses SSE byte stream to a final [`ResponsesResponse`].
///
/// The authoritative result is the `response.completed` event's `response`
/// object (same shape as a non-streamed body); intermediate
/// `response.output_text.delta` events are forwarded to `deltas` for live
/// rendering and accumulated so a Stop can return the partial. Each read is
/// bounded by `idle_timeout`: a gap longer than that (a server wedged
/// mid-generation, or stuck in an over-long prefill) surfaces as a transient
/// stall the agent loop retries. `cancel` aborts between reads.
async fn drive_sse<S>(
    mut stream: S,
    idle_timeout: Duration,
    cancel: CancellationToken,
    deltas: Option<UnboundedSender<String>>,
) -> Result<ResponsesResponse>
where
    S: futures::Stream<Item = Result<Vec<u8>>> + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut acc = String::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(cancelled_response(acc)),
            n = tokio::time::timeout(idle_timeout, stream.next()) => n,
        };
        let chunk = match next {
            Err(_elapsed) => bail!(
                "LLM response stalled mid-stream: no data for {}s (server wedged mid-generation)",
                idle_timeout.as_secs()
            ),
            Ok(None) => bail!(
                "LLM stream ended before a completion event (the server closed the connection)"
            ),
            Ok(Some(Err(e))) => return Err(e).context("reading LLM response stream"),
            Ok(Some(Ok(bytes))) => bytes,
        };
        buf.extend_from_slice(&chunk);

        // Process every complete line we now have. SSE frames are
        // `event:`/`data:` lines terminated by `\n`; we only need the `data:`
        // JSON (it carries its own `type`).
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(payload) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue; // tolerate keepalive/comment frames
            };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("response.output_text.delta") => {
                    if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                        acc.push_str(d);
                        if let Some(tx) = &deltas {
                            let _ = tx.send(d.to_string());
                        }
                    }
                }
                Some("response.completed") => {
                    let resp = v
                        .get("response")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    return serde_json::from_value(resp)
                        .context("parsing the streamed response.completed event");
                }
                Some("response.failed") | Some("error") => {
                    bail!("LLM streaming error: {payload}")
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_omlx_response_with_null_valued_fields() {
        // Shape omlx (Qwen3.6 on this server) actually returns: every item
        // carries every field, `null` where it doesn't apply. The reasoning
        // item has `content: null` (text lives under `summary`), and items
        // carry null `call_id`/`name`/`summary`. Before the null-tolerant
        // fix this failed with "invalid type: null, expected a sequence" and
        // the whole (valid) response was discarded.
        let body = r#"{
          "id": "resp_abc",
          "status": "completed",
          "output": [
            {"type":"reasoning","id":"rs_1","status":"completed","role":null,
             "content":null,"call_id":null,"name":null,"arguments":null,
             "summary":[{"type":"summary_text","text":"thinking..."}]},
            {"type":"message","id":"msg_1","status":"completed","role":"assistant",
             "content":[{"type":"output_text","text":"No actions found."}],
             "call_id":null,"name":null,"arguments":null,"summary":null}
          ]
        }"#;
        let resp: ResponsesResponse =
            serde_json::from_str(body).expect("null-valued fields must parse");
        assert_eq!(resp.status, "completed");
        assert_eq!(resp.output.len(), 2);
        // Reasoning is captured (for the chat UI), not discarded: the
        // `summary` text survives parsing even though `content` is null.
        match &resp.output[0] {
            OutputItem::Reasoning { summary } => {
                assert_eq!(summary[0].text, "thinking...");
                assert_eq!(
                    resp.output[0].reasoning_text().as_deref(),
                    Some("thinking...")
                );
            }
            other => panic!("expected reasoning, got {other:?}"),
        }
        match &resp.output[1] {
            OutputItem::Message { content } => match &content[0] {
                ContentItem::OutputText { text } => assert_eq!(text, "No actions found."),
                other => panic!("expected output_text, got {other:?}"),
            },
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn parses_function_call_and_null_message_content() {
        // A tool-call turn: a function_call item plus a message item whose
        // content the server set to null. Both must survive.
        let body = r#"{
          "id": "resp_def",
          "status": "completed",
          "output": [
            {"type":"function_call","call_id":"call_1","name":"record_action","arguments":"{}"},
            {"type":"message","id":"m","status":"completed","role":"assistant","content":null}
          ]
        }"#;
        let resp: ResponsesResponse = serde_json::from_str(body).expect("must parse");
        assert!(matches!(resp.output[0], OutputItem::FunctionCall { .. }));
        // null message content degrades to an empty Vec, not a parse failure.
        match &resp.output[1] {
            OutputItem::Message { content } => assert!(content.is_empty()),
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_sse_assembles_response_and_forwards_deltas() {
        // A realistic omlx event stream: two text deltas then the authoritative
        // `response.completed`. Chunked at an awkward boundary to exercise the
        // line buffering across reads.
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\", world\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello, world\"}]}]}}\n",
            "\n",
        );
        let chunks: Vec<Result<Vec<u8>>> =
            sse.as_bytes().chunks(7).map(|c| Ok(c.to_vec())).collect();
        let stream = futures::stream::iter(chunks);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let resp = drive_sse(
            stream,
            Duration::from_secs(5),
            CancellationToken::new(),
            Some(tx),
        )
        .await
        .expect("stream should assemble a response");

        assert_eq!(resp.id, "resp_1");
        assert_eq!(resp.status, "completed");
        match &resp.output[0] {
            OutputItem::Message { content } => match &content[0] {
                ContentItem::OutputText { text } => assert_eq!(text, "Hello, world"),
                other => panic!("expected output_text, got {other:?}"),
            },
            other => panic!("expected message, got {other:?}"),
        }
        // Deltas were forwarded in order for live rendering.
        let mut got = String::new();
        while let Ok(d) = rx.try_recv() {
            got.push_str(&d);
        }
        assert_eq!(got, "Hello, world");
    }

    #[tokio::test]
    async fn drive_sse_returns_empty_when_already_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        // The biased select checks cancellation first, so the (unconsumed)
        // stream content is irrelevant.
        let stream = futures::stream::iter(vec![Ok(b"data: ignored\n".to_vec())]);
        let resp = drive_sse(stream, Duration::from_secs(5), cancel, None)
            .await
            .expect("a cancel is a clean stop, not an error");
        assert_eq!(resp.status, "cancelled");
        assert!(resp.output.is_empty());
    }

    #[tokio::test]
    async fn drive_sse_returns_partial_text_on_cancel() {
        let cancel = CancellationToken::new();
        let gate = cancel.clone();
        // First a delta, then a frame that never arrives until the token is
        // cancelled — so the partial is provably accumulated before Stop.
        // Boxed because a `once(async {…})` future isn't `Unpin`.
        let stream = Box::pin(
            futures::stream::once(async {
                Ok(
                    b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n"
                        .to_vec(),
                )
            })
            .chain(futures::stream::once(async move {
                gate.cancelled().await;
                futures::future::pending::<Result<Vec<u8>>>().await
            })),
        );

        let cancel2 = cancel.clone();
        let handle =
            tokio::spawn(
                async move { drive_sse(stream, Duration::from_secs(30), cancel2, None).await },
            );
        // Let the driver consume the first delta and park on the second read.
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();

        let resp = handle.await.unwrap().expect("cancel is a clean stop");
        assert_eq!(resp.status, "cancelled");
        match &resp.output[0] {
            OutputItem::Message { content } => match &content[0] {
                ContentItem::OutputText { text } => assert_eq!(text, "partial"),
                other => panic!("expected output_text, got {other:?}"),
            },
            other => panic!("expected message, got {other:?}"),
        }
    }

    /// End-to-end over a real socket: the live `LlmClient::send_stream` (reqwest
    /// + SSE parse) must abort an in-flight request when the token is cancelled
    /// and return the partial — the Stop button's engine half.
    #[tokio::test]
    async fn send_stream_cancels_a_live_request_and_keeps_the_partial() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Serve headers + one delta, then hold the connection open without ever
        // completing — only a client-side cancel can end it.
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await; // consume the request
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            sock.write_all(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            )
            .await
            .unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let client = LlmClient::new(format!("http://{addr}"), "m");
        let cancel = CancellationToken::new();
        let cancel_for_send = cancel.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let handle = tokio::spawn(async move {
            client
                .send_stream(
                    "sys",
                    vec![],
                    &[],
                    None,
                    Duration::from_secs(30),
                    cancel_for_send,
                    Some(tx),
                )
                .await
        });

        // Once the delta has been forwarded the request is provably in flight.
        assert_eq!(rx.recv().await.as_deref(), Some("partial"));
        cancel.cancel();

        let resp = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("cancel must return promptly, not hang")
            .unwrap()
            .expect("cancel is a clean stop, not an error");
        assert_eq!(resp.status, "cancelled");
        match &resp.output[0] {
            OutputItem::Message { content } => match &content[0] {
                ContentItem::OutputText { text } => assert_eq!(text, "partial"),
                other => panic!("expected output_text, got {other:?}"),
            },
            other => panic!("expected message, got {other:?}"),
        }
        server.abort();
    }
}
