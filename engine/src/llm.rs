use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ReasoningContentItem {
    #[serde(rename = "reasoning_text")]
    ReasoningText { text: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    #[serde(rename = "message")]
    Message { content: Vec<ContentItem> },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "reasoning")]
    Reasoning { content: Vec<ReasoningContentItem> },
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
}

impl LlmClient {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            bearer_token: None,
        }
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

        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("LLM API error (HTTP {status}): {text}");
        }

        serde_json::from_str(&text).with_context(|| format!("failed to parse LLM response: {text}"))
    }
}
