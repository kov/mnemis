//! Test utilities for mnemis-engine.
//!
//! Always exported (not `#[cfg(test)]`) so integration tests in `tests/` and
//! downstream crates (CLI, app) can use the same mocks and seed helpers.
//!
//! The `MockLlm` is a scripted queue of pre-built `ResponsesResponse`s — call
//! [`make_test_llm`] in tests to get back either a `MockLlm` (default) or a
//! real `LlmClient` pointed at omlx, depending on `MNEMIS_TEST_LLM` env.

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Mutex;

use crate::llm::{
    ContentItem, InputItem, LlmClient, LlmTransport, OutputItem, ResponsesResponse, ToolDef,
};

// ---------- MockLlm ------------------------------------------------------

/// Returns scripted responses in FIFO order. Panics on unscripted call.
pub struct MockLlm {
    queue: Mutex<Vec<ResponsesResponse>>,
}

impl MockLlm {
    pub fn new(responses: Vec<ResponsesResponse>) -> Self {
        Self {
            queue: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmTransport for MockLlm {
    async fn send(
        &self,
        _instructions: &str,
        _input: Vec<InputItem>,
        _tools: &[ToolDef],
        _previous_response_id: Option<&str>,
    ) -> Result<ResponsesResponse> {
        let mut q = self.queue.lock().expect("MockLlm queue poisoned");
        if q.is_empty() {
            panic!("MockLlm: agent loop called send() with empty script");
        }
        Ok(q.remove(0))
    }
}

// ---------- Mock response builders ---------------------------------------

pub mod mock {
    use super::*;

    /// A turn that issues a single `record_action` tool call.
    pub fn record_action(title: &str, confidence: &str, evidence: &[&str]) -> ResponsesResponse {
        let args = json!({
            "title": title,
            "details": title,
            "confidence": confidence,
            "rationale": "scripted by mock",
            "evidence_external_ids": evidence,
        })
        .to_string();
        turn(vec![OutputItem::FunctionCall {
            call_id: "mock-call".to_string(),
            name: "record_action".to_string(),
            arguments: args,
        }])
    }

    /// A turn that issues a single `update_action` tool call.
    pub fn update_action(action_id: i64, changes: serde_json::Value) -> ResponsesResponse {
        let args = json!({
            "action_id": format!("A-{action_id}"),
            "changes": changes,
            "evidence_external_ids": [],
        })
        .to_string();
        turn(vec![OutputItem::FunctionCall {
            call_id: "mock-call".to_string(),
            name: "update_action".to_string(),
            arguments: args,
        }])
    }

    /// A turn that issues a single `resolve_action` tool call.
    pub fn resolve_action(
        action_id: i64,
        status: &str,
        confidence: &str,
        evidence: &[&str],
    ) -> ResponsesResponse {
        let args = json!({
            "action_id": format!("A-{action_id}"),
            "status": status,
            "confidence": confidence,
            "rationale": "scripted by mock",
            "evidence_external_ids": evidence,
        })
        .to_string();
        turn(vec![OutputItem::FunctionCall {
            call_id: "mock-call".to_string(),
            name: "resolve_action".to_string(),
            arguments: args,
        }])
    }

    /// A turn that returns only an assistant text message — ends the agent loop.
    pub fn no_tools(text: &str) -> ResponsesResponse {
        turn(vec![OutputItem::Message {
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
        }])
    }

    /// Build a turn from arbitrary output items.
    pub fn turn(items: Vec<OutputItem>) -> ResponsesResponse {
        ResponsesResponse {
            id: "mock".to_string(),
            status: "completed".to_string(),
            output: items,
        }
    }
}

// ---------- make_test_llm: mock vs live switch ---------------------------

/// Returns a mock LLM driven by `mock_script` by default. When the env var
/// `MNEMIS_TEST_LLM=live` is set (along with `MNEMIS_TEST_LLM_URL` and
/// `MNEMIS_TEST_LLM_MODEL`), returns a real `LlmClient` pointed at omlx and
/// the script is dropped.
pub fn make_test_llm(mock_script: Vec<ResponsesResponse>) -> Box<dyn LlmTransport> {
    if std::env::var("MNEMIS_TEST_LLM").as_deref() == Ok("live") {
        let url = std::env::var("MNEMIS_TEST_LLM_URL")
            .expect("MNEMIS_TEST_LLM=live requires MNEMIS_TEST_LLM_URL");
        let model = std::env::var("MNEMIS_TEST_LLM_MODEL")
            .expect("MNEMIS_TEST_LLM=live requires MNEMIS_TEST_LLM_MODEL");
        let mut client = LlmClient::new(url, model);
        if let Ok(token) = std::env::var("MNEMIS_TEST_LLM_TOKEN") {
            client = client.with_bearer_token(token);
        }
        Box::new(client)
    } else {
        Box::new(MockLlm::new(mock_script))
    }
}

/// The model name extract_for_channel should record. Live tests use the
/// real model name (so extraction_runs entries are accurate); mock tests
/// use a sentinel.
pub fn test_model_name() -> String {
    std::env::var("MNEMIS_TEST_LLM_MODEL").unwrap_or_else(|_| "mock-model".to_string())
}

// ---------- DB seed helpers ----------------------------------------------

pub struct SeedCtx {
    pub source_id: i64,
    pub channel_id: i64,
    pub self_email: String,
    pub model: String,
}

/// Seed the minimum to run an extraction: user_profile, self-contact + email,
/// one IMAP source, one channel.
pub async fn seed_minimal(pool: &SqlitePool) -> Result<SeedCtx> {
    let now = Utc::now().timestamp();
    let self_email = "test@example.com".to_string();

    sqlx::query(
        "INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, 'Test User', ?) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO contacts (display_name, relationship, created_at, updated_at) \
         VALUES ('Test User', 'self', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO contact_identifiers (contact_id, kind, value) \
         SELECT id, 'email', ? FROM contacts WHERE relationship = 'self' LIMIT 1",
    )
    .bind(&self_email)
    .execute(pool)
    .await?;

    let (source_id,): (i64,) = sqlx::query_as(
        "INSERT INTO sources (kind, name, config_ref, created_at) \
         VALUES ('imap', 'test', 'test/ref', ?) RETURNING id",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;

    let (channel_id,): (i64,) = sqlx::query_as(
        "INSERT INTO channels (source_id, external_id, name, kind) \
         VALUES (?, 'INBOX', 'INBOX', 'mailbox') RETURNING id",
    )
    .bind(source_id)
    .fetch_one(pool)
    .await?;

    Ok(SeedCtx {
        source_id,
        channel_id,
        self_email,
        model: test_model_name(),
    })
}

pub struct SeedMessage<'a> {
    pub external_id: &'a str,
    pub author_email: &'a str,
    pub author_name: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
}

/// Insert messages into the channel, upserting authors as needed. Messages
/// are spaced one second apart in posted_at order.
pub async fn seed_messages(
    pool: &SqlitePool,
    source_id: i64,
    channel_id: i64,
    messages: &[SeedMessage<'_>],
) -> Result<()> {
    let base = Utc::now().timestamp();
    for (idx, m) in messages.iter().enumerate() {
        let author_id: i64 = match sqlx::query_as::<_, (i64,)>(
            "SELECT id FROM people WHERE source_id = ? AND external_id = ?",
        )
        .bind(source_id)
        .bind(m.author_email)
        .fetch_optional(pool)
        .await?
        {
            Some((id,)) => id,
            None => {
                sqlx::query_as::<_, (i64,)>(
                    "INSERT INTO people (source_id, external_id, display_name) \
                     VALUES (?, ?, ?) RETURNING id",
                )
                .bind(source_id)
                .bind(m.author_email)
                .bind(m.author_name)
                .fetch_one(pool)
                .await?
                .0
            }
        };

        sqlx::query(
            "INSERT INTO messages \
             (channel_id, external_id, author_id, posted_at, subject, body, body_format, ingested_at, flags) \
             VALUES (?, ?, ?, ?, ?, ?, 'text', ?, 0)",
        )
        .bind(channel_id)
        .bind(m.external_id)
        .bind(author_id)
        .bind(base + idx as i64)
        .bind(m.subject)
        .bind(m.body)
        .bind(base)
        .execute(pool)
        .await?;
    }
    Ok(())
}

// ---------- Assertion helpers --------------------------------------------

pub struct ActionRow {
    pub id: i64,
    pub title: String,
    pub details: Option<String>,
    pub confidence: String,
    pub status: String,
}

pub async fn fetch_actions(pool: &SqlitePool) -> Result<Vec<ActionRow>> {
    let rows: Vec<(i64, String, Option<String>, String, String)> =
        sqlx::query_as("SELECT id, title, details, confidence, status FROM actions ORDER BY id")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, title, details, confidence, status)| ActionRow {
            id,
            title,
            details,
            confidence,
            status,
        })
        .collect())
}

pub async fn assert_evidence_contains(
    pool: &SqlitePool,
    action_id: i64,
    external_id: &str,
) -> Result<()> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM action_evidence ae JOIN messages m ON m.id = ae.message_id \
         WHERE ae.action_id = ? AND m.external_id = ?",
    )
    .bind(action_id)
    .bind(external_id)
    .fetch_one(pool)
    .await?;
    if count == 0 {
        anyhow::bail!("action {action_id} does not list {external_id} as evidence");
    }
    Ok(())
}
