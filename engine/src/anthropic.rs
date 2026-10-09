//! Claude (Anthropic Messages API) transport.
//!
//! Speaks `POST /v1/messages` over raw reqwest (there is no official Rust SDK)
//! and adapts it to the Responses-shaped [`InputItem`]/[`OutputItem`] the agent
//! loops are written against, so neither loop knows which provider it runs on:
//!
//! - **Input:** the flat item list is regrouped into Claude's nested messages —
//!   an assistant turn's thinking, text and `tool_use` blocks go into one
//!   assistant message, and every `tool_result` of a round into one user
//!   message. `instructions` becomes the top-level `system`.
//! - **Output:** content blocks map back to items in order. A `thinking` block
//!   yields its display summary as [`OutputItem::Reasoning`] *and* the signed
//!   block as [`OutputItem::Thinking`], which the loops echo back within the
//!   tool round (Claude rejects a tool result whose round lost its thinking).
//!
//! Every send streams, even [`LlmTransport::send`]: a high-effort turn can
//! legitimately take minutes, so the bound is the per-event idle timeout, not a
//! total-request timeout.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::llm::{
    ContentItem, InputItem, LlmTransport, OutputItem, ReasoningSummary, ResponsesResponse, Role,
    ToolDef, cancelled_response,
};
use mnemis_types::ThinkingLevel;

/// Default API root; `base_url` in config overrides it (e.g. a proxy).
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";

/// Keychain reference the API key is stored under.
pub const API_KEY_REF: &str = "anthropic/api-key";

const API_VERSION: &str = "2023-06-01";

/// Output cap per response. Thinking counts toward it, so it is sized for the
/// reasoning as well as the reply; streaming keeps a large cap safe.
const MAX_TOKENS: u32 = 64_000;

/// Connect timeout; the stream itself is bounded by the idle timeout.
const CONNECT_TIMEOUT_SECS: u64 = 15;

/// Marker the agent loop's transient-stall classifier keys on (see
/// `agent::is_transient_stall`) for retryable API conditions.
pub(crate) const TEMPORARILY_UNAVAILABLE: &str = "temporarily unavailable";

#[derive(Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
    effort: &'static str,
    /// Idle timeout for [`LlmTransport::send`] (the streaming path gets one
    /// per call).
    idle_timeout: Duration,
}

impl AnthropicClient {
    pub fn new(
        base_url: Option<&str>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let base_url = base_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string();
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url,
            model: model.into(),
            api_key: api_key.into(),
            effort: effort_for(ThinkingLevel::default()),
            idle_timeout: Duration::from_secs(crate::llm::DEFAULT_CHAT_IDLE_TIMEOUT_SECS),
        }
    }

    pub fn with_thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.effort = effort_for(level);
        self
    }

    pub fn with_idle_timeout(mut self, secs: u64) -> Self {
        self.idle_timeout = Duration::from_secs(secs);
        self
    }

    async fn stream(
        &self,
        instructions: &str,
        input: Vec<InputItem>,
        tools: &[ToolDef],
        idle_timeout: Duration,
        cancel: CancellationToken,
        deltas: Option<UnboundedSender<String>>,
    ) -> Result<ResponsesResponse> {
        let body = build_request(&self.model, self.effort, instructions, input, tools);
        let req = self
            .http
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body);

        let resp = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(cancelled_response(String::new())),
            r = tokio::time::timeout(idle_timeout, req.send()) => match r {
                Err(_) => bail!(
                    "LLM response stalled: no response headers for {}s",
                    idle_timeout.as_secs()
                ),
                Ok(r) => r.context("LLM request failed")?,
            },
        };
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(api_error(status.as_u16(), &text));
        }
        let stream = resp
            .bytes_stream()
            .map(|r| r.map(|b| b.to_vec()).map_err(anyhow::Error::from));
        drive_sse(stream, idle_timeout, cancel, deltas).await
    }
}

#[async_trait]
impl LlmTransport for AnthropicClient {
    async fn send(
        &self,
        instructions: &str,
        input: Vec<InputItem>,
        tools: &[ToolDef],
        _previous_response_id: Option<&str>,
    ) -> Result<ResponsesResponse> {
        self.stream(
            instructions,
            input,
            tools,
            self.idle_timeout,
            CancellationToken::new(),
            None,
        )
        .await
    }

    async fn send_stream(
        &self,
        instructions: &str,
        input: Vec<InputItem>,
        tools: &[ToolDef],
        _previous_response_id: Option<&str>,
        idle_timeout: Duration,
        cancel: CancellationToken,
        deltas: Option<UnboundedSender<String>>,
    ) -> Result<ResponsesResponse> {
        self.stream(instructions, input, tools, idle_timeout, cancel, deltas)
            .await
    }
}

/// mnemis's coarse thinking knob maps onto Claude's `effort` levels. (A token
/// budget is rejected by current models; effort is the only control.)
fn effort_for(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::ExtraHigh => "xhigh",
    }
}

/// Classify a non-2xx reply. Retryable conditions (rate limit, overload,
/// server error) carry [`TEMPORARILY_UNAVAILABLE`] so the agent loop retries
/// them; a prompt over the context window keeps Claude's own wording, which
/// `agent::is_context_overflow` matches. Everything else is terminal.
fn api_error(status: u16, body: &str) -> anyhow::Error {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let kind = parsed
        .as_ref()
        .and_then(|v| v.pointer("/error/type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = parsed
        .as_ref()
        .and_then(|v| v.pointer("/error/message"))
        .and_then(Value::as_str)
        .unwrap_or(body);
    let retryable = matches!(kind, "rate_limit_error" | "overloaded_error" | "api_error")
        || status == 429
        || status >= 500;
    if retryable {
        anyhow::anyhow!("Claude API {TEMPORARILY_UNAVAILABLE} (HTTP {status} {kind}): {message}")
    } else {
        anyhow::anyhow!("Claude API error (HTTP {status} {kind}): {message}")
    }
}

/// Build the `/v1/messages` request body.
fn build_request(
    model: &str,
    effort: &str,
    instructions: &str,
    input: Vec<InputItem>,
    tools: &[ToolDef],
) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        // Caches the longest stable prefix (tools → system → messages); the
        // loops resend the whole conversation every step.
        "cache_control": {"type": "ephemeral"},
        "thinking": {"type": "adaptive", "display": "summarized"},
        "output_config": {"effort": effort},
        "messages": to_messages(input),
    });
    if !instructions.is_empty() {
        body["system"] = json!(instructions);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect(),
        );
    }
    body
}

/// Regroup flat items into Claude messages. Consecutive items of the same role
/// merge into one message, which is exactly what Claude needs: an assistant
/// response's thinking, text and tool calls form one assistant message, and the
/// tool results that follow form one user message.
fn to_messages(input: Vec<InputItem>) -> Vec<Value> {
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let mut push = |role: &'static str, block: Value| match out.last_mut() {
        Some((r, blocks)) if *r == role => blocks.push(block),
        _ => out.push((role, vec![block])),
    };
    for item in input {
        match item {
            InputItem::Message { role, content } => {
                if content.trim().is_empty() {
                    continue; // Claude rejects empty text blocks
                }
                let role = match role {
                    Role::Assistant => "assistant",
                    // Mid-conversation system messages exist, but have
                    // placement rules; a user-role note is always valid.
                    Role::User | Role::System => "user",
                };
                push(role, json!({"type": "text", "text": content}));
            }
            InputItem::Thinking { block } => push("assistant", block),
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let input: Value = serde_json::from_str(&arguments)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                push(
                    "assistant",
                    json!({"type": "tool_use", "id": call_id, "name": name, "input": input}),
                );
            }
            InputItem::FunctionCallOutput { call_id, output } => push(
                "user",
                json!({"type": "tool_result", "tool_use_id": call_id, "content": output}),
            ),
        }
    }
    out.into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect()
}

/// One content block being assembled from stream events.
#[derive(Default)]
struct Block {
    kind: String,
    text: String,
    thinking: String,
    signature: String,
    data: String,
    tool_id: String,
    tool_name: String,
    tool_json: String,
}

/// Drive a Messages SSE stream to a [`ResponsesResponse`]. Text deltas are
/// forwarded to `deltas` and accumulated so a Stop returns the partial. Each
/// read is bounded by `idle_timeout`; `cancel` aborts between reads.
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
    let mut partial = String::new();
    let mut id = String::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut stop_reason = String::new();
    let mut stop_details = Value::Null;
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(cancelled_response(partial)),
            n = tokio::time::timeout(idle_timeout, stream.next()) => n,
        };
        let chunk = match next {
            Err(_elapsed) => bail!(
                "LLM response stalled mid-stream: no data for {}s",
                idle_timeout.as_secs()
            ),
            Ok(None) => bail!(
                "LLM stream ended before a completion event (the server closed the connection)"
            ),
            Ok(Some(Err(e))) => return Err(e).context("reading LLM response stream"),
            Ok(Some(Ok(bytes))) => bytes,
        };
        buf.extend_from_slice(&chunk);

        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(payload) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(payload.trim()) else {
                continue;
            };
            let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            match v.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    if let Some(s) = v.pointer("/message/id").and_then(Value::as_str) {
                        id = s.to_string();
                    }
                }
                Some("content_block_start") => {
                    let cb = &v["content_block"];
                    if blocks.len() <= index {
                        blocks.resize_with(index + 1, Block::default);
                    }
                    let b = &mut blocks[index];
                    b.kind = cb["type"].as_str().unwrap_or_default().to_string();
                    b.tool_id = cb["id"].as_str().unwrap_or_default().to_string();
                    b.tool_name = cb["name"].as_str().unwrap_or_default().to_string();
                    b.data = cb["data"].as_str().unwrap_or_default().to_string();
                    b.text = cb["text"].as_str().unwrap_or_default().to_string();
                }
                Some("content_block_delta") => {
                    let Some(b) = blocks.get_mut(index) else {
                        continue;
                    };
                    let d = &v["delta"];
                    match d["type"].as_str() {
                        Some("text_delta") => {
                            let t = d["text"].as_str().unwrap_or_default();
                            b.text.push_str(t);
                            partial.push_str(t);
                            if let Some(tx) = &deltas {
                                let _ = tx.send(t.to_string());
                            }
                        }
                        Some("thinking_delta") => {
                            b.thinking
                                .push_str(d["thinking"].as_str().unwrap_or_default());
                        }
                        Some("signature_delta") => {
                            b.signature
                                .push_str(d["signature"].as_str().unwrap_or_default());
                        }
                        Some("input_json_delta") => {
                            b.tool_json
                                .push_str(d["partial_json"].as_str().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
                Some("message_delta") => {
                    if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                        stop_reason = r.to_string();
                    }
                    if let Some(d) = v.pointer("/delta/stop_details") {
                        stop_details = d.clone();
                    }
                }
                Some("message_stop") => {
                    return finish(id, blocks, &stop_reason, &stop_details);
                }
                Some("error") => {
                    let kind = v.pointer("/error/type").and_then(Value::as_str);
                    let message = v
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or(payload);
                    return Err(match kind {
                        Some("overloaded_error" | "rate_limit_error" | "api_error") => {
                            anyhow::anyhow!(
                                "Claude API {TEMPORARILY_UNAVAILABLE} mid-stream ({}): {message}",
                                kind.unwrap_or_default()
                            )
                        }
                        _ => anyhow::anyhow!("Claude streaming error: {payload}"),
                    });
                }
                _ => {}
            }
        }
    }
}

/// Turn the assembled blocks into output items, honouring the stop reason.
fn finish(
    id: String,
    blocks: Vec<Block>,
    stop_reason: &str,
    stop_details: &Value,
) -> Result<ResponsesResponse> {
    match stop_reason {
        "refusal" => {
            let category = stop_details
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("unspecified");
            bail!("Claude declined this request (refusal; category: {category})")
        }
        "model_context_window_exceeded" => {
            bail!("prompt is too long: the conversation exceeded the model's context window")
        }
        "max_tokens" => {
            bail!("Claude stopped at the {MAX_TOKENS}-token output limit before finishing")
        }
        _ => {}
    }
    let mut output = Vec::new();
    for b in blocks {
        match b.kind.as_str() {
            "thinking" => {
                if !b.thinking.trim().is_empty() {
                    output.push(OutputItem::Reasoning {
                        summary: vec![ReasoningSummary {
                            text: b.thinking.clone(),
                        }],
                    });
                }
                output.push(OutputItem::Thinking {
                    block: json!({
                        "type": "thinking",
                        "thinking": b.thinking,
                        "signature": b.signature,
                    }),
                });
            }
            "redacted_thinking" => output.push(OutputItem::Thinking {
                block: json!({"type": "redacted_thinking", "data": b.data}),
            }),
            "text" if !b.text.is_empty() => output.push(OutputItem::Message {
                content: vec![ContentItem::OutputText { text: b.text }],
            }),
            "tool_use" => {
                let arguments = if b.tool_json.trim().is_empty() {
                    "{}".to_string()
                } else {
                    b.tool_json
                };
                output.push(OutputItem::FunctionCall {
                    call_id: b.tool_id,
                    name: b.tool_name,
                    arguments,
                });
            }
            _ => {}
        }
    }
    Ok(ResponsesResponse {
        id,
        status: "completed".to_string(),
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ToolDef {
        ToolDef::function(
            "get_action".into(),
            "Fetch an action".into(),
            json!({"type": "object", "properties": {"id": {"type": "string"}}}),
        )
    }

    #[test]
    fn regroups_a_tool_round_into_nested_messages() {
        let input = vec![
            InputItem::Message {
                role: Role::User,
                content: "why flagged?".into(),
            },
            InputItem::Thinking {
                block: json!({"type": "thinking", "thinking": "", "signature": "sig"}),
            },
            InputItem::FunctionCall {
                call_id: "t1".into(),
                name: "get_action".into(),
                arguments: r#"{"id":"A-1"}"#.into(),
            },
            InputItem::FunctionCall {
                call_id: "t2".into(),
                name: "get_action".into(),
                arguments: "not json".into(),
            },
            InputItem::FunctionCallOutput {
                call_id: "t1".into(),
                output: "one".into(),
            },
            InputItem::FunctionCallOutput {
                call_id: "t2".into(),
                output: "two".into(),
            },
        ];
        let body = build_request("m", "medium", "be brief", input, &[tool()]);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(
            msgs.len(),
            3,
            "user, assistant (thinking+calls), user (results)"
        );
        assert_eq!(msgs[1]["role"], "assistant");
        let a = msgs[1]["content"].as_array().unwrap();
        assert_eq!(
            a[0]["signature"], "sig",
            "thinking replayed first, verbatim"
        );
        assert_eq!(a[1]["input"]["id"], "A-1", "arguments become a JSON object");
        assert_eq!(
            a[2]["input"],
            json!({}),
            "unparseable arguments degrade to {{}}"
        );
        let r = msgs[2]["content"].as_array().unwrap();
        assert_eq!(r.len(), 2, "all of a round's results in one user message");
        assert_eq!(r[1]["tool_use_id"], "t2");
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["output_config"]["effort"], "medium");
        assert!(body.get("thinking_budget").is_none());
    }

    #[test]
    fn skips_empty_text() {
        let msgs = to_messages(vec![
            InputItem::Message {
                role: Role::User,
                content: "hi".into(),
            },
            InputItem::Message {
                role: Role::Assistant,
                content: "  ".into(),
            },
        ]);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn effort_tracks_thinking_level() {
        assert_eq!(effort_for(ThinkingLevel::Low), "low");
        assert_eq!(effort_for(ThinkingLevel::ExtraHigh), "xhigh");
    }

    #[test]
    fn classifies_api_errors() {
        let overloaded = format!(
            "{:#}",
            api_error(
                529,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            )
        );
        assert!(crate::agent::is_transient_stall(&overloaded));
        let too_long = format!(
            "{:#}",
            api_error(
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1000001 tokens > 1000000 maximum"}}"#
            )
        );
        assert!(crate::agent::is_context_overflow(&too_long));
        assert!(!crate::agent::is_transient_stall(&too_long));
        let auth = format!(
            "{:#}",
            api_error(
                401,
                r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
            )
        );
        assert!(!crate::agent::is_transient_stall(&auth));
    }

    fn sse(events: &[Value]) -> Vec<Result<Vec<u8>>> {
        let text: String = events
            .iter()
            .map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e))
            .collect();
        text.as_bytes().chunks(11).map(|c| Ok(c.to_vec())).collect()
    }

    #[tokio::test]
    async fn assembles_thinking_text_and_tool_use_from_the_stream() {
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1"}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Look it up."}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Checking"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_action","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"id\":"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"A-1\"}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let resp = drive_sse(
            futures::stream::iter(sse(&events)),
            Duration::from_secs(5),
            CancellationToken::new(),
            Some(tx),
        )
        .await
        .unwrap();
        assert_eq!(resp.id, "msg_1");
        assert_eq!(
            resp.output[0].reasoning_text().as_deref(),
            Some("Look it up.")
        );
        assert!(
            matches!(&resp.output[1], OutputItem::Thinking { block } if block["signature"] == "SIG")
        );
        assert!(matches!(&resp.output[2], OutputItem::Message { .. }));
        assert!(matches!(
            &resp.output[3],
            OutputItem::FunctionCall { call_id, arguments, .. }
                if call_id == "toolu_1" && arguments == r#"{"id":"A-1"}"#
        ));
        assert_eq!(rx.recv().await.as_deref(), Some("Checking"));
    }

    #[tokio::test]
    async fn refusal_and_stream_errors_surface_as_errors() {
        let refusal = [
            json!({"type":"message_start","message":{"id":"m"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"}}}),
            json!({"type":"message_stop"}),
        ];
        let err = drive_sse(
            futures::stream::iter(sse(&refusal)),
            Duration::from_secs(5),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("cyber"));

        let overloaded =
            [json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})];
        let err = drive_sse(
            futures::stream::iter(sse(&overloaded)),
            Duration::from_secs(5),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(crate::agent::is_transient_stall(&format!("{err:#}")));
    }
}
