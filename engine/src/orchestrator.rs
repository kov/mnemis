//! End-to-end sync. For each enabled source we phase the work: poll + ingest
//! every channel, then embed all the new messages in one pass, then extract
//! every channel. Embedding and extraction use *different* models on the omlx
//! server; interleaving them per-channel kept both the embedding model and the
//! large chat model resident the whole sync, pinning the server at its memory
//! ceiling and throttling generation (see the omlx memory-enforcer note in the
//! `v1-gotchas` memory). Phasing lets the long extract pass run chat-only.
//! Used by both the CLI's `sync` command and the desktop app's manual "Sync
//! now" button.
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
use std::time::{Duration, Instant};
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
/// re-extraction tooling can spot prompt/model changes. `window_char_budget`
/// caps how much message text a single extraction call carries; larger
/// windows are split into sequential batches (see
/// [`crate::extract::extract_for_channel`]).
#[allow(clippy::too_many_arguments)]
pub async fn sync_now(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    embedder: Arc<dyn Embedder>,
    model_name: &str,
    window_char_budget: usize,
    idle_timeout: Duration,
    traces_dir: Option<&std::path::Path>,
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
        match sync_one_source(
            pool,
            source_id,
            &source_name,
            llm,
            &embedder,
            model_name,
            window_char_budget,
            idle_timeout,
            traces_dir,
        )
        .await
        {
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

    // Push newly-due actions + pull calendar-side changes. Best-effort: a
    // CalDAV failure (or no account configured) must not fail the whole sync.
    match sync_calendar_if_configured(pool).await {
        Ok(Some(s)) => {
            info!(
                created = s.created,
                pushed = s.pushed,
                pulled = s.pulled,
                removed = s.removed,
                conflicts = s.conflicts,
                "caldav sync"
            );
            out.errors
                .extend(s.errors.into_iter().map(|e| format!("caldav: {e}")));
        }
        Ok(None) => {}
        Err(e) => {
            warn!(error = format!("{e:#}"), "caldav sync failed");
            out.errors.push(format!("caldav sync: {e:#}"));
        }
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

#[allow(clippy::too_many_arguments)]
async fn sync_one_source(
    pool: &SqlitePool,
    source_id: i64,
    source_name: &str,
    llm: &dyn LlmTransport,
    embedder: &Arc<dyn Embedder>,
    model_name: &str,
    window_char_budget: usize,
    idle_timeout: Duration,
    traces_dir: Option<&std::path::Path>,
) -> Result<SourceCounts> {
    let source = build_imap_source(pool, SourceId(source_id)).await?;
    sync_one_source_with(
        pool,
        source_id,
        source_name,
        &source,
        llm,
        embedder,
        model_name,
        window_char_budget,
        idle_timeout,
        traces_dir,
    )
    .await
}

/// Inner loop, split out so tests can inject a fake `Source` without
/// touching the keychain or the IMAP transport.
#[allow(clippy::too_many_arguments)]
async fn sync_one_source_with(
    pool: &SqlitePool,
    source_id: i64,
    source_name: &str,
    source: &dyn Source,
    llm: &dyn LlmTransport,
    embedder: &Arc<dyn Embedder>,
    model_name: &str,
    window_char_budget: usize,
    idle_timeout: Duration,
    traces_dir: Option<&std::path::Path>,
) -> Result<SourceCounts> {
    // Refresh the folder list from the server first, so a freshly added source
    // (or a newly created server-side folder) is polled this run instead of
    // only after some later sync. Best-effort: a discovery failure shouldn't
    // stop us polling the folders we already know.
    if let Err(e) = discover_channels(pool, source, source_id).await {
        warn!(
            error = %format!("{e:#}"),
            source_id, "channel discovery failed; polling known channels only"
        );
    }

    let channels: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, external_id, cursor FROM channels WHERE source_id = ? AND muted = 0",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await
    .context("listing channels")?;

    let total_channels = channels.len();
    let mut counts = SourceCounts::default();

    // Phase 1 — poll + ingest every channel, remembering which ones got new
    // messages so we only extract those. No embedding or extraction inline
    // here: keeping them out of this loop is what avoids holding both the
    // embed and chat models resident at once (see the module doc).
    let mut to_extract: Vec<(i64, String)> = Vec::new();
    for (idx, (channel_id, external_id, cursor)) in channels.into_iter().enumerate() {
        counts.channels_polled += 1;
        let ch_started = Instant::now();
        info!(
            channel_id,
            channel = %external_id,
            progress = format!("{}/{total_channels}", idx + 1),
            "channel: poll"
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

        if inserted == 0 {
            info!(
                channel_id,
                channel = %external_id,
                ingested = 0,
                secs = ch_started.elapsed().as_secs(),
                "channel: no new messages"
            );
            continue;
        }
        trace!(channel_id, ingested = inserted, "channel: ingested");
        to_extract.push((channel_id, external_id));
    }

    // Phase 2 — embed everything ingested this cycle in a single pass, while
    // the embedding model is the only one that needs to be hot. `drain_once`
    // records per-entry failures and keeps them queued (returning Ok), so a
    // single transport error surfaces once at the source level rather than
    // once per channel.
    match drain_once(pool, embedder.as_ref()).await {
        Ok(n) => {
            counts.embeddings_drained += n as i64;
            if n > 0 {
                trace!(source_id, embedded = n, "source: embedded new messages");
            }
        }
        Err(e) => {
            warn!(
                error = format!("{e:#}"),
                source_id, "source: embed pass failed"
            );
            counts
                .errors
                .push(format!("source '{source_name}': embed failed: {e:#}"));
        }
    }

    // Phase 3 — extract each channel that ingested new messages. Only the chat
    // model is exercised now, so the embed model can stay evicted.
    for (channel_id, external_id) in to_extract {
        let ex_started = Instant::now();
        trace!(channel_id, channel = %external_id, "channel: extracting");
        match extract_for_channel(
            pool,
            llm,
            channel_id,
            model_name,
            window_char_budget,
            idle_timeout,
            traces_dir,
        )
        .await
        {
            Ok(o) => {
                counts.actions_created += o.actions_created as i64;
                info!(
                    channel_id,
                    channel = %external_id,
                    actions = o.actions_created,
                    result = o.result,
                    secs = ex_started.elapsed().as_secs(),
                    "channel: extracted"
                );
            }
            Err(e) => {
                let chain = format!("{e:#}");
                // A missing/misconfigured chat model 404s identically on every
                // channel. Report it once at the source level and stop hammering
                // the rest — there's nothing per-channel to retry.
                if mnemis_types::is_model_not_found(&chain) {
                    warn!(source = %source_name, error = %chain, "source: configured model not found; skipping remaining channels");
                    counts
                        .errors
                        .push(format!("source '{source_name}': {chain}"));
                    break;
                }
                warn!(
                    error = %chain,
                    channel_id,
                    channel = %external_id,
                    secs = ex_started.elapsed().as_secs(),
                    "channel: extract failed"
                );
                counts.errors.push(format!(
                    "source '{source_name}' channel '{external_id}': extract failed: {chain}"
                ));
            }
        }
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

/// Discover the source's channels (IMAP folders) and upsert them into the
/// `channels` table, returning how many the server reported. Idempotent
/// (`ON CONFLICT DO NOTHING`), so it's safe to run on every sync and whenever
/// the settings view asks to refresh: newly created server-side folders appear,
/// while the per-channel mute/cursor state on folders we already know is left
/// untouched. Best-effort callers should log-and-continue on error — a
/// discovery failure (offline, transient) must not block polling known folders.
pub async fn discover_channels(
    pool: &SqlitePool,
    source: &dyn Source,
    source_id: i64,
) -> Result<usize> {
    let channels = source.list_channels().await.context("listing channels")?;
    for ch in &channels {
        sqlx::query(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(source_id)
        .bind(&ch.external_id)
        .bind(&ch.name)
        .bind(&ch.kind)
        .execute(pool)
        .await
        .with_context(|| format!("upserting channel {}", ch.external_id))?;
    }
    Ok(channels.len())
}

/// Run a CalDAV reminder sync iff an account *and* a task collection are
/// configured. Returns `None` when CalDAV isn't set up (account absent, or no
/// collection picked yet) so callers can stay silent; otherwise the reconcile
/// summary. Builds the backend from the persisted account + keychain password,
/// mirroring [`build_imap_source`].
pub async fn sync_calendar_if_configured(
    pool: &SqlitePool,
) -> Result<Option<crate::sync::reconcile::SyncSummary>> {
    let Some(account) = crate::settings::load_caldav_account(pool).await? else {
        return Ok(None);
    };
    let Some(collection_url) = account.collection_url.as_deref() else {
        return Ok(None);
    };
    let password = secrets::fetch(&account.keychain_ref)
        .await
        .with_context(|| format!("fetching keychain entry {}", account.keychain_ref))?;
    let backend =
        crate::sync::caldav::CaldavBackend::new(collection_url, &account.username, &password)?;
    let summary = crate::sync::reconcile::sync_caldav(pool, &backend).await?;
    Ok(Some(summary))
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
    use crate::llm::{InputItem, ResponsesResponse, ToolDef};
    use crate::source::{ChannelInfo, ImportedAuthor, ImportedMessage, PollBatch, SourceKind};
    use crate::test_util::{MockLlm, mock};
    use anyhow::bail;
    use async_trait::async_trait;
    use chrono::Utc;
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
        let out = sync_now(
            &pool,
            &llm,
            embedder,
            "test-model",
            crate::extract::DEFAULT_WINDOW_CHAR_BUDGET,
            Duration::from_secs(60),
            None,
        )
        .await?;
        assert_eq!(out.sources_synced, 0);
        assert_eq!(out.sources_failed, 0);
        assert_eq!(out.messages_ingested, 0);
        assert!(out.errors.is_empty());
        Ok(())
    }

    /// Stand-in Source that returns one message on poll, no matter what's
    /// asked. We only need `poll` for sync_one_source_with; the other
    /// methods aren't on the sync path.
    struct OneMessageSource(SourceId);

    #[async_trait]
    impl Source for OneMessageSource {
        fn id(&self) -> SourceId {
            self.0
        }
        fn kind(&self) -> SourceKind {
            SourceKind::Imap
        }
        async fn list_channels(&self) -> Result<Vec<ChannelInfo>> {
            // Discovery now runs on the sync path. These tests seed channels
            // directly, so report none here (the upsert is a no-op) and let the
            // seeded rows drive the poll loop.
            Ok(Vec::new())
        }
        async fn poll(&self, _channel: &str, _cursor: Option<&Cursor>) -> Result<PollBatch> {
            Ok(PollBatch {
                messages: vec![ImportedMessage {
                    external_id: "m1".to_string(),
                    parent_external_id: None,
                    author: Some(ImportedAuthor {
                        external_id: "ana@example.com".to_string(),
                        display_name: Some("Ana".to_string()),
                        handle: None,
                    }),
                    posted_at: Utc::now(),
                    subject: Some("Hello".to_string()),
                    body: "Please take a look".to_string(),
                    body_format: "text".to_string(),
                    recipients: Vec::new(),
                    raw_json: None,
                    flags: 0,
                }],
                next_cursor: Cursor("1:2".to_string()),
                more_available: false,
            })
        }
        async fn fetch(&self, _channel: &str, _msg: &str) -> Result<ImportedMessage> {
            unimplemented!("not used in sync path")
        }
    }

    struct ContextWindowLlm;

    #[async_trait]
    impl LlmTransport for ContextWindowLlm {
        async fn send(
            &self,
            _instructions: &str,
            _input: Vec<InputItem>,
            _tools: &[ToolDef],
            _previous_response_id: Option<&str>,
        ) -> Result<ResponsesResponse> {
            bail!(
                "LLM API error (HTTP 400 Bad Request): Prompt too long: \
                 154806 tokens exceeds max context window of 131072 tokens"
            )
        }
    }

    /// Always 404s with the omlx "model not found" body, like a typo'd
    /// chat_model would on every channel.
    struct ModelNotFoundLlm;

    #[async_trait]
    impl LlmTransport for ModelNotFoundLlm {
        async fn send(
            &self,
            _instructions: &str,
            _input: Vec<InputItem>,
            _tools: &[ToolDef],
            _previous_response_id: Option<&str>,
        ) -> Result<ResponsesResponse> {
            bail!(
                "LLM API error (HTTP 404 Not Found): \
                 {{\"error\":{{\"type\":\"not_found_error\",\"message\":\"Model 'test-model' not \
                 found. Available models: real-a, real-b\"}}}}"
            )
        }
    }

    /// End-to-end: a per-channel extract failure must land in the
    /// SourceCounts.errors so sync_now relays it into SyncOutcome.errors →
    /// toast. Previously the failure was swallowed inside
    /// extract_for_channel and never reached the orchestrator.
    #[tokio::test]
    async fn extract_failure_lands_in_source_counts_errors() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;

        let (source_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'fastmail', 'fake/ref', ?) RETURNING id",
        )
        .bind(Utc::now().timestamp())
        .fetch_one(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, 'INBOX/Lembrar', 'INBOX/Lembrar', 'mailbox')",
        )
        .bind(source_id)
        .execute(&pool)
        .await?;

        let source = OneMessageSource(SourceId(source_id));
        let embedder: Arc<dyn Embedder> = Arc::new(crate::embed::OmlxEmbedder::new(
            "http://0.0.0.0".to_string(),
            "noop".to_string(),
        ));

        let counts = sync_one_source_with(
            &pool,
            source_id,
            "fastmail",
            &source,
            &ContextWindowLlm,
            &embedder,
            "test-model",
            crate::extract::DEFAULT_WINDOW_CHAR_BUDGET,
            Duration::from_secs(60),
            None,
        )
        .await?;

        assert_eq!(counts.channels_polled, 1);
        assert_eq!(counts.messages_ingested, 1);
        assert_eq!(counts.actions_created, 0);
        assert_eq!(
            counts.errors.len(),
            1,
            "expected one channel error in counts: {:?}",
            counts.errors
        );
        let err = &counts.errors[0];
        assert!(
            err.contains("fastmail"),
            "error should name the source: {err}"
        );
        assert!(
            err.contains("INBOX/Lembrar"),
            "error should name the channel: {err}"
        );
        assert!(
            err.contains("context window"),
            "error should preserve the transport message for the classifier: {err}"
        );
        Ok(())
    }

    /// A missing model 404s on every channel; the orchestrator must collapse
    /// that to a single source-level error rather than one per channel.
    #[tokio::test]
    async fn model_not_found_is_reported_once_per_source() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;

        let (source_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'fastmail', 'fake/ref', ?) RETURNING id",
        )
        .bind(Utc::now().timestamp())
        .fetch_one(&pool)
        .await?;
        // Two channels: without the collapse this would log two identical errors.
        for ch in ["INBOX", "INBOX/Lembrar"] {
            sqlx::query(
                "INSERT INTO channels (source_id, external_id, name, kind) \
                 VALUES (?, ?, ?, 'mailbox')",
            )
            .bind(source_id)
            .bind(ch)
            .bind(ch)
            .execute(&pool)
            .await?;
        }

        let source = OneMessageSource(SourceId(source_id));
        let embedder: Arc<dyn Embedder> = Arc::new(crate::embed::OmlxEmbedder::new(
            "http://0.0.0.0".to_string(),
            "noop".to_string(),
        ));

        let counts = sync_one_source_with(
            &pool,
            source_id,
            "fastmail",
            &source,
            &ModelNotFoundLlm,
            &embedder,
            "test-model",
            crate::extract::DEFAULT_WINDOW_CHAR_BUDGET,
            Duration::from_secs(60),
            None,
        )
        .await?;

        assert_eq!(counts.channels_polled, 2);
        assert_eq!(
            counts.errors.len(),
            1,
            "a missing model should collapse to one source-level error: {:?}",
            counts.errors
        );
        let err = &counts.errors[0];
        assert!(err.contains("fastmail"), "names the source: {err}");
        assert!(
            mnemis_types::is_model_not_found(err),
            "preserves the body so the toast classifier names the model: {err}"
        );
        Ok(())
    }

    /// Source that reports two folders, to exercise `discover_channels`.
    struct TwoFolderSource(SourceId);

    #[async_trait]
    impl Source for TwoFolderSource {
        fn id(&self) -> SourceId {
            self.0
        }
        fn kind(&self) -> SourceKind {
            SourceKind::Imap
        }
        async fn list_channels(&self) -> Result<Vec<ChannelInfo>> {
            Ok(vec![
                ChannelInfo {
                    external_id: "INBOX".to_string(),
                    name: "INBOX".to_string(),
                    kind: "mailbox".to_string(),
                },
                ChannelInfo {
                    external_id: "INBOX/Lembrar".to_string(),
                    name: "INBOX/Lembrar".to_string(),
                    kind: "mailbox".to_string(),
                },
            ])
        }
        async fn poll(&self, _channel: &str, _cursor: Option<&Cursor>) -> Result<PollBatch> {
            unimplemented!("not used in this test")
        }
        async fn fetch(&self, _channel: &str, _msg: &str) -> Result<ImportedMessage> {
            unimplemented!("not used in this test")
        }
    }

    /// Discovery upserts the server's folders and is idempotent — a second run
    /// adds no duplicates and preserves the mute state of folders we already
    /// knew (the part that makes "discover on every sync / settings open" safe).
    #[tokio::test]
    async fn discover_channels_upserts_idempotently() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;

        let (source_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'fastmail', 'fake/ref', ?) RETURNING id",
        )
        .bind(Utc::now().timestamp())
        .fetch_one(&pool)
        .await?;

        let source = TwoFolderSource(SourceId(source_id));
        let discovered = discover_channels(&pool, &source, source_id).await?;
        assert_eq!(discovered, 2);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM channels WHERE source_id = ?")
            .bind(source_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 2, "both folders inserted");

        // Mute one, then re-discover: no duplicates, mute preserved.
        sqlx::query("UPDATE channels SET muted = 1 WHERE external_id = 'INBOX/Lembrar'")
            .execute(&pool)
            .await?;
        discover_channels(&pool, &source, source_id).await?;

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM channels WHERE source_id = ?")
            .bind(source_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 2, "re-discovery adds no duplicates");
        let (muted,): (i64,) =
            sqlx::query_as("SELECT muted FROM channels WHERE external_id = 'INBOX/Lembrar'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(muted, 1, "re-discovery preserves mute state");
        Ok(())
    }
}
