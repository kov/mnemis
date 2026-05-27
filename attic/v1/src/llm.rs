use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::LlmConfig;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    #[allow(dead_code)]
    System,
    #[allow(dead_code)]
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

/// Tool definition for the Responses API.
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

/// Request body for POST /v1/responses.
#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: Vec<InputItem>,
    tools: &'a [ToolDef],
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<&'a str>,
}

/// A content item inside a message output.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ContentItem {
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(other)]
    Unknown,
}

/// A content item inside a reasoning output.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ReasoningContentItem {
    #[serde(rename = "reasoning_text")]
    ReasoningText { text: String },
    #[serde(other)]
    Unknown,
}

/// An item in the response output array.
#[derive(Debug, Clone, Deserialize)]
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

/// Response body from POST /v1/responses.
#[derive(Debug, Deserialize)]
pub struct ResponsesResponse {
    pub id: String,
    #[allow(dead_code)]
    pub status: String,
    pub output: Vec<OutputItem>,
}

/// HTTP client for the LLM Responses API.
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    bearer_token: Option<String>,
}

impl LlmClient {
    pub fn new(config: &LlmConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            bearer_token: config.bearer_token.clone(),
        }
    }

    pub async fn send(
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

        let parsed: ResponsesResponse = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse LLM response: {text}"))?;
        Ok(parsed)
    }
}
