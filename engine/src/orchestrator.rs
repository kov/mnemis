//! End-to-end sync: for each enabled source, poll → ingest → embed →
//! extract. Used by both the CLI's `sync` command and the desktop app's
//! manual "Sync now" button.
//!
//! The implementation is deliberately blocking-per-source so callers get a
//! single aggregate `SyncOutcome` back. Per-channel progress streaming (for
//! a live spinner) can be layered on later via a `tokio::sync::mpsc` if the
//! UI needs it — current design intent is "click button → wait → refresh"
//! (see project memory v2-redesign).

use anyhow::{Context, Result};
use chrono::Utc;
use mnemis_types::SyncOutcome;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, trace, warn};

use crate::embed::{Embedder, drain_once};
use crate::extract::extract_for_channel;
use crate::ingest::ingest_batch;
use crate::llm::LlmTransport;
use crate::secrets;
use crate::source::imap::{ImapConfig, ImapSource};
use crate::source::{Cursor, Source, SourceId};

/// Run one polling + extraction cycle across every non-disabled source.
///
/// `model_name` is recorded in `extraction_runs.model` so the eventual
/// re-extraction tooling can spot prompt/model changes.
pub async fn sync_now(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    embedder: Arc<dyn Embedder>,
    model_name: &str,
) -> Result<SyncOutcome> {
    let mut out = SyncOutcome::default();

    let sources: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM sources WHERE status != 'disabled' ORDER BY id")
            .fetch_all(pool)
            .await
            .context("listing sources")?;

    info!(count = sources.len(), "sync starting");
    let sync_started = Instant::now();

    for (source_id, source_name) in sources {
        let started = Instant::now();
        info!(source_id, source_name = %source_name, "source: start");
        match sync_one_source(pool, source_id, &source_name, llm, &embedder, model_name).await {
            Ok(counts) => {
                info!(
                    source_id,
                    source_name = %source_name,
                    channels = counts.channels_polled,
                    messages = counts.messages_ingested,
                    embeddings = counts.embeddings_drained,
                    actions = counts.actions_created,
                    errors = counts.errors.len(),
                    secs = started.elapsed().as_secs(),
                    "source: done"
                );
                out.sources_synced += 1;
                out.channels_polled += counts.channels_polled;
                out.messages_ingested += counts.messages_ingested;
                out.embeddings_drained += counts.embeddings_drained;
                out.actions_created += counts.actions_created;
                out.errors.extend(counts.errors);
                mark_source_ok(pool, source_id).await.ok();
            }
            Err(e) => {
                let msg = format!("source '{source_name}' (id={source_id}): {e:#}");
                // {e:#} expands the anyhow chain — the outer "source sync
                // failed" alone is rarely enough to tell what actually broke.
                warn!(
                    error = format!("{e:#}"),
                    source_id,
                    source_name = %source_name,
                    secs = started.elapsed().as_secs(),
                    "source: failed"
                );
                out.sources_failed += 1;
                out.errors.push(msg.clone());
                mark_source_failure(pool, source_id, &format!("{e:#}"))
                    .await
                    .ok();
            }
        }
    }

    // Final embed pass picks up actions/notes enqueued during this cycle.
    let drain_started = Instant::now();
    match drain_once(pool, embedder.as_ref()).await {
        Ok(n) => {
            out.embeddings_drained += n as i64;
            if n > 0 {
                info!(
                    count = n,
                    secs = drain_started.elapsed().as_secs(),
                    "final embed drain"
                );
            }
        }
        Err(e) => warn!(error = format!("{e:#}"), "final embed drain failed"),
    }

    info!(
        sources_synced = out.sources_synced,
        sources_failed = out.sources_failed,
        channels = out.channels_polled,
        messages = out.messages_ingested,
        actions = out.actions_created,
        embeddings = out.embeddings_drained,
        errors = out.errors.len(),
        secs = sync_started.elapsed().as_secs(),
        "sync done"
    );

    Ok(out)
}

#[derive(Default)]
struct SourceCounts {
    channels_polled: i64,
    messages_ingested: i64,
    embeddings_drained: i64,
    actions_created: i64,
    errors: Vec<String>,
}

async fn sync_one_source(
    pool: &SqlitePool,
    source_id: i64,
    source_name: &str,
    llm: &dyn LlmTransport,
    embedder: &Arc<dyn Embedder>,
    model_name: &str,
) -> Result<SourceCounts> {
    let source = build_imap_source(pool, SourceId(source_id)).await?;
    let channels: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, external_id, cursor FROM channels WHERE source_id = ? AND muted = 0",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await
    .context("listing channels")?;

    let total_channels = channels.len();
    let mut counts = SourceCounts::default();
    for (idx, (channel_id, external_id, cursor)) in channels.into_iter().enumerate() {
        counts.channels_polled += 1;
        let ch_started = Instant::now();
        info!(
            channel_id,
            channel = %external_id,
            progress = format!("{}/{total_channels}", idx + 1),
            "channel: start"
        );
        let cursor = cursor.map(Cursor);
        let batch = match source.poll(&external_id, cursor.as_ref()).await {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    channel_id,
                    channel = %external_id,
                    "channel: poll failed"
                );
                counts.errors.push(format!(
                    "source '{source_name}' channel '{external_id}': poll failed: {e:#}"
                ));
                continue;
            }
        };

        let inserted = ingest_batch(pool, SourceId(source_id), channel_id, &batch).await?;
        counts.messages_ingested += inserted as i64;
        trace!(channel_id, ingested = inserted, "channel: ingested");

        if inserted == 0 {
            info!(
                channel_id,
                channel = %external_id,
                ingested = 0,
                secs = ch_started.elapsed().as_secs(),
                "channel: done"
            );
            continue;
        }

        let mut ch_embedded = 0i64;
        let mut ch_embed_err: Option<String> = None;
        match drain_once(pool, embedder.as_ref()).await {
            Ok(n) => {
                ch_embedded = n as i64;
                counts.embeddings_drained += n as i64;
                trace!(channel_id, embedded = n, "channel: embedded");
            }
            Err(e) => {
                ch_embed_err = Some(format!("{e:#}"));
                warn!(
                    error = format!("{e:#}"),
                    channel_id,
                    channel = %external_id,
                    "channel: embed failed"
                );
                counts.errors.push(format!(
                    "source '{source_name}' channel '{external_id}': embed failed: {e:#}"
                ));
            }
        }

        let ex_started = Instant::now();
        trace!(channel_id, "channel: extracting");
        let mut ch_actions = 0usize;
        let mut ch_extract_err: Option<String> = None;
        match extract_for_channel(pool, llm, channel_id, model_name).await {
            Ok(o) => {
                ch_actions = o.actions_created;
                counts.actions_created += o.actions_created as i64;
                trace!(
                    channel_id,
                    actions = o.actions_created,
                    result = o.result,
                    secs = ex_started.elapsed().as_secs(),
                    "channel: extracted"
                );
            }
            Err(e) => {
                ch_extract_err = Some(format!("{e:#}"));
                warn!(
                    error = format!("{e:#}"),
                    channel_id,
                    channel = %external_id,
                    secs = ex_started.elapsed().as_secs(),
                    "channel: extract failed"
                );
                counts.errors.push(format!(
                    "source '{source_name}' channel '{external_id}': extract failed: {e:#}"
                ));
            }
        }

        info!(
            channel_id,
            channel = %external_id,
            ingested = inserted,
            embedded = ch_embedded,
            actions = ch_actions,
            embed_err = ch_embed_err.is_some(),
            extract_err = ch_extract_err.is_some(),
            secs = ch_started.elapsed().as_secs(),
            "channel: done"
        );
    }

    Ok(counts)
}

/// Build a runnable `ImapSource` from the persisted settings + the keychain
/// entry. Moved here from `cli/src/commands.rs` so the desktop app can use
/// the same construction path.
pub async fn build_imap_source(pool: &SqlitePool, source_id: SourceId) -> Result<ImapSource> {
    let (kind, config_ref): (String, String) =
        sqlx::query_as("SELECT kind, config_ref FROM sources WHERE id = ?")
            .bind(source_id.0)
            .fetch_one(pool)
            .await?;
    if kind != "imap" {
        anyhow::bail!("source {} is not IMAP (kind={kind})", source_id.0);
    }

    let conn_json: (String,) = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
        .bind(format!("source/{}/imap", source_id.0))
        .fetch_one(pool)
        .await
        .context("missing IMAP connection settings")?;
    let parsed: serde_json::Value = serde_json::from_str(&conn_json.0)?;
    let server = parsed["server"]
        .as_str()
        .context("missing server")?
        .to_string();
    let port = parsed["port"].as_u64().context("missing port")? as u16;
    let username = parsed["username"]
        .as_str()
        .context("missing username")?
        .to_string();

    let password = secrets::fetch(&config_ref)
        .await
        .with_context(|| format!("fetching keychain entry {config_ref}"))?;

    Ok(ImapSource::new(
        source_id,
        ImapConfig {
            server,
            port,
            username,
            password,
        },
    ))
}

pub async fn mark_source_ok(pool: &SqlitePool, source_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE sources SET status = 'ok', last_synced_at = ?, last_error = NULL, \
         consecutive_failures = 0 WHERE id = ?",
    )
    .bind(Utc::now().timestamp())
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_source_failure(pool: &SqlitePool, source_id: i64, error: &str) -> Result<()> {
    sqlx::query(
        "UPDATE sources SET status = CASE WHEN consecutive_failures + 1 >= 6 THEN 'failed' \
         ELSE 'warning' END, last_error = ?, consecutive_failures = consecutive_failures + 1 \
         WHERE id = ?",
    )
    .bind(error)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::test_util::{MockLlm, mock};
    use tempfile::TempDir;

    /// With no sources configured, sync_now returns an empty outcome cleanly.
    #[tokio::test]
    async fn sync_now_no_sources_is_a_noop() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;

        let llm = MockLlm::new(vec![mock::no_tools("noop")]);
        let embedder: Arc<dyn Embedder> = Arc::new(crate::embed::OmlxEmbedder::new(
            "http://0.0.0.0".to_string(),
            "noop".to_string(),
        ));
        let out = sync_now(&pool, &llm, embedder, "test-model").await?;
        assert_eq!(out.sources_synced, 0);
        assert_eq!(out.sources_failed, 0);
        assert_eq!(out.messages_ingested, 0);
        assert!(out.errors.is_empty());
        Ok(())
    }
}
