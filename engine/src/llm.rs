use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

/// Build the HTTP client with a request timeout. Falls back to a default
/// client (no timeout) only if the TLS/backend init fails — never panics
/// during app startup.
fn build_http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
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
    // The model's chain-of-thought. omlx returns it as a `reasoning` item
    // with `content: null` (the text lives under a `summary` array); we don't
    // consume it either way, so accept the tag and ignore every other field.
    // A unit variant tolerates whatever shape the server sends.
    #[serde(rename = "reasoning")]
    Reasoning,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponsesResponse {
    pub id: String,
    pub status: String,
    pub output: Vec<OutputItem>,
}

pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    bearer_token: Option<String>,
    timeout_secs: u64,
}

impl LlmClient {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http: build_http_client(DEFAULT_REQUEST_TIMEOUT_SECS),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            bearer_token: None,
            timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }

    /// Override the request timeout (seconds). Rebuilds the HTTP client.
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
        assert!(matches!(resp.output[0], OutputItem::Reasoning));
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
}
