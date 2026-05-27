use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::AssertSqlSafe;
use sqlx::SqlitePool;
use tracing::warn;

/// Embeds text into vectors. Trait-shaped so the worker can be tested
/// without an omlx server.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

#[derive(Debug, Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

/// omlx (OpenAI-compatible) `/v1/embeddings` client.
pub struct OmlxEmbedder {
    http: reqwest::Client,
    base_url: String,
    model: String,
    bearer_token: Option<String>,
}

impl OmlxEmbedder {
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
impl Embedder for OmlxEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/embeddings", self.base_url);
        let body = EmbeddingsRequest {
            model: &self.model,
            input: text,
        };
        let mut req = self.http.post(&url).json(&body);
        if let Some(token) = &self.bearer_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("embeddings API error (HTTP {status}): {body}");
        }
        let parsed: EmbeddingsResponse = serde_json::from_str(&body)
            .with_context(|| format!("failed to parse embeddings response: {body}"))?;
        let mut data = parsed.data;
        let first = data.pop().context("empty embeddings response")?;
        Ok(first.embedding)
    }
}

const MAX_ATTEMPTS: i64 = 5;
const BATCH_SIZE: i64 = 100;

/// Drain up to BATCH_SIZE queue entries once and return the count successfully embedded.
pub async fn drain_once(pool: &SqlitePool, embedder: &dyn Embedder) -> Result<usize> {
    let queue: Vec<(i64, String, i64)> = sqlx::query_as(
        "SELECT id, target_kind, target_id FROM embed_queue \
         WHERE attempts < ? ORDER BY enqueued_at LIMIT ?",
    )
    .bind(MAX_ATTEMPTS)
    .bind(BATCH_SIZE)
    .fetch_all(pool)
    .await?;

    let mut processed = 0;
    for (queue_id, target_kind, target_id) in queue {
        match process_one(pool, embedder, &target_kind, target_id).await {
            Ok(()) => {
                sqlx::query("DELETE FROM embed_queue WHERE id = ?")
                    .bind(queue_id)
                    .execute(pool)
                    .await?;
                processed += 1;
            }
            Err(e) => {
                warn!(error = %e, kind = %target_kind, id = target_id, "embed failed");
                sqlx::query(
                    "UPDATE embed_queue SET attempts = attempts + 1, last_error = ? WHERE id = ?",
                )
                .bind(e.to_string())
                .bind(queue_id)
                .execute(pool)
                .await?;
            }
        }
    }
    Ok(processed)
}

async fn process_one(
    pool: &SqlitePool,
    embedder: &dyn Embedder,
    target_kind: &str,
    target_id: i64,
) -> Result<()> {
    let (text, vec_table) = load_target_text(pool, target_kind, target_id).await?;
    let embedding = embedder.embed(&text).await?;
    write_embedding(pool, vec_table, target_id, &embedding).await?;
    Ok(())
}

async fn load_target_text(
    pool: &SqlitePool,
    target_kind: &str,
    target_id: i64,
) -> Result<(String, &'static str)> {
    match target_kind {
        "message" => {
            let (subject, body): (Option<String>, String) =
                sqlx::query_as("SELECT subject, body FROM messages WHERE id = ?")
                    .bind(target_id)
                    .fetch_one(pool)
                    .await
                    .context("loading message text")?;
            let text = match subject {
                Some(s) if !s.is_empty() => format!("{s}\n\n{body}"),
                _ => body,
            };
            Ok((text, "messages_vec"))
        }
        "memory_note" => {
            let (key, content): (String, String) =
                sqlx::query_as("SELECT key, content FROM memory_notes WHERE id = ?")
                    .bind(target_id)
                    .fetch_one(pool)
                    .await
                    .context("loading memory note text")?;
            Ok((format!("{key}\n\n{content}"), "memory_notes_vec"))
        }
        // `action` and `contact` kinds land with the extractor and contacts UI.
        other => bail!("unsupported embed target kind: {other}"),
    }
}

async fn write_embedding(
    pool: &SqlitePool,
    vec_table: &str,
    rowid: i64,
    embedding: &[f32],
) -> Result<()> {
    let json = serde_json::to_string(embedding)?;
    // vec_table is from a hard-coded match in load_target_text — never user input.
    let delete_sql = AssertSqlSafe(format!("DELETE FROM {vec_table} WHERE rowid = ?"));
    let insert_sql = AssertSqlSafe(format!(
        "INSERT INTO {vec_table}(rowid, embedding) VALUES (?, vec_f32(?))"
    ));
    let mut tx = pool.begin().await?;
    sqlx::query(delete_sql)
        .bind(rowid)
        .execute(&mut *tx)
        .await?;
    sqlx::query(insert_sql)
        .bind(rowid)
        .bind(&json)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::ingest::ingest_batch;
    use crate::source::{Cursor, ImportedAuthor, ImportedMessage, PollBatch, SourceId};
    use chrono::{DateTime, Utc};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    struct FixedEmbedder {
        value: Vec<f32>,
        calls: AtomicUsize,
    }

    impl FixedEmbedder {
        fn new(value: Vec<f32>) -> Self {
            Self {
                value,
                calls: AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Embedder for FixedEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.value.clone())
        }
    }

    struct FailingEmbedder;

    #[async_trait]
    impl Embedder for FailingEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            bail!("simulated failure")
        }
    }

    async fn setup_with_messages() -> Result<(TempDir, SqlitePool)> {
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
            external_id: "m1".to_string(),
            parent_external_id: None,
            author: Some(ImportedAuthor {
                external_id: "a@b.com".to_string(),
                display_name: None,
                handle: None,
            }),
            posted_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            subject: Some("S".to_string()),
            body: "hello world".to_string(),
            body_format: "text".to_string(),
            raw_json: None,
            flags: 0,
        };

        let batch = PollBatch {
            messages: vec![msg],
            next_cursor: Cursor("1:2".to_string()),
            more_available: false,
        };
        ingest_batch(&pool, SourceId(source_id), channel_id, &batch).await?;
        Ok((tmp, pool))
    }

    #[tokio::test]
    async fn drains_queue_and_writes_vectors() -> Result<()> {
        let (_tmp, pool) = setup_with_messages().await?;
        let embedder = FixedEmbedder::new(vec![0.5_f32; 768]);

        let processed = drain_once(&pool, &embedder).await?;
        assert_eq!(processed, 1);
        assert_eq!(embedder.call_count(), 1);

        let q: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM embed_queue")
            .fetch_one(&pool)
            .await?;
        assert_eq!(q.0, 0);

        let v: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages_vec")
            .fetch_one(&pool)
            .await?;
        assert_eq!(v.0, 1);

        Ok(())
    }

    #[tokio::test]
    async fn failed_embed_increments_attempts_and_keeps_in_queue() -> Result<()> {
        let (_tmp, pool) = setup_with_messages().await?;
        let embedder = FailingEmbedder;

        let processed = drain_once(&pool, &embedder).await?;
        assert_eq!(processed, 0);

        let (attempts, err): (i64, Option<String>) =
            sqlx::query_as("SELECT attempts, last_error FROM embed_queue LIMIT 1")
                .fetch_one(&pool)
                .await?;
        assert_eq!(attempts, 1);
        assert!(err.unwrap().contains("simulated failure"));

        Ok(())
    }

    #[tokio::test]
    async fn skips_entries_past_max_attempts() -> Result<()> {
        let (_tmp, pool) = setup_with_messages().await?;
        sqlx::query("UPDATE embed_queue SET attempts = ?")
            .bind(MAX_ATTEMPTS)
            .execute(&pool)
            .await?;

        let embedder = FixedEmbedder::new(vec![0.5_f32; 768]);
        let processed = drain_once(&pool, &embedder).await?;
        assert_eq!(processed, 0);
        assert_eq!(embedder.call_count(), 0);
        Ok(())
    }
}
