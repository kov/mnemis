//! End-to-end sync, in two phases so new mail shows without waiting on the
//! model. [`sync_mail`] brings every account's folders up to date (the
//! to-do poll's folders ingested as extraction candidates, the rest for
//! reading); [`find_to_dos`] then embeds what's new, extracts every folder
//! with mail not yet extracted, and syncs the calendar. Apps run the first,
//! refresh, and run the second in the background; [`sync_now`] runs both.
//!
//! Embedding and extraction use *different* models on the omlx server;
//! interleaving them per-channel kept both resident the whole sync, pinning
//! the server at its memory ceiling and throttling generation (see the omlx
//! memory-enforcer note in the `v1-gotchas` memory). So per account the
//! embed pass runs first, then the extract pass, chat model only.

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
use crate::source::imap::{ImapAuth, ImapConfig, ImapSource};
use crate::source::{Cursor, PollBatch, Source, SourceId};

/// A full sync: [`sync_mail`], then [`find_to_dos`]. `model_name` is
/// recorded in `extraction_runs.model`; `window_char_budget` caps how much
/// message text one extraction call carries (see
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
    let mut out = sync_mail_outcome(pool).await?;
    let found = find_to_dos(
        pool,
        llm,
        embedder,
        model_name,
        window_char_budget,
        idle_timeout,
        traces_dir,
    )
    .await?;
    out.embeddings_drained += found.embeddings_drained;
    out.actions_created += found.actions_created;
    out.errors.extend(found.errors);
    Ok(out)
}

/// Phase two of a sync: embed new mail and extract to-dos from every folder
/// with mail not yet extracted (whatever synced it), then sync the
/// calendar. One run at a time per database; a second waits its turn, then
/// finds only what arrived since.
#[allow(clippy::too_many_arguments)]
pub async fn find_to_dos(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    embedder: Arc<dyn Embedder>,
    model_name: &str,
    window_char_budget: usize,
    idle_timeout: Duration,
    traces_dir: Option<&std::path::Path>,
) -> Result<SyncOutcome> {
    let _serialized = extraction_lock(pool).await;
    let mut out = SyncOutcome::default();
    let started = Instant::now();
    // First the mail the user is waiting for, in every account, so channel
    // extraction leaves a match to its watch.
    let expected: Erased<'_, usize> = Box::pin(check_expected_mail(pool, llm, window_char_budget));
    match expected.await {
        Ok(n) => out.actions_created += n as i64,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "checking expected mail failed");
            out.errors.push(format!("expected mail: {e:#}"));
        }
    }
    let sources: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM sources WHERE status != 'disabled' ORDER BY id")
            .fetch_all(pool)
            .await
            .context("listing sources")?;
    for (source_id, source_name) in sources {
        let counts: Erased<'_, SourceCounts> = Box::pin(extract_source(
            pool,
            source_id,
            &source_name,
            llm,
            &embedder,
            model_name,
            window_char_budget,
            idle_timeout,
            traces_dir,
        ));
        let counts = counts.await?;
        out.embeddings_drained += counts.embeddings_drained;
        out.actions_created += counts.actions_created;
        out.errors.extend(counts.errors);
    }

    // Describe the new mail for the briefing, then rewrite the last day's
    // summary if it is stale. Best-effort, like the expected-mail check.
    let briefing: Erased<'_, ()> = Box::pin(update_briefing(pool, llm, window_char_budget));
    if let Err(e) = briefing.await {
        warn!(error = %format!("{e:#}"), "updating the briefing failed");
        out.errors.push(format!("briefing: {e:#}"));
    }

    // Final embed pass picks up actions/notes enqueued during this cycle.
    match drain_once(pool, embedder.as_ref()).await {
        Ok(n) => out.embeddings_drained += n as i64,
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
        actions = out.actions_created,
        embeddings = out.embeddings_drained,
        errors = out.errors.len(),
        secs = started.elapsed().as_secs(),
        "to-dos found"
    );
    Ok(out)
}

/// Expire the watches past their time, then check the new mail against the
/// rest (see `extract::expected`).
async fn check_expected_mail(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    window_char_budget: usize,
) -> Result<usize> {
    use crate::extract::expected;
    let expired = expected::expire(pool).await?;
    let started = Instant::now();
    let recorded = expected::check(pool, llm, window_char_budget).await?;
    if expired > 0 || recorded > 0 {
        info!(
            expired,
            recorded,
            secs = started.elapsed().as_secs(),
            "expected mail checked"
        );
    }
    Ok(recorded)
}

/// Tag the new mail (see `briefing::tag`), then refresh the last day's
/// written briefing when it is stale.
async fn update_briefing(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    window_char_budget: usize,
) -> Result<()> {
    use crate::briefing::{agent, tag};
    let started = Instant::now();
    let tagged = tag::tag_new_mail(pool, llm, window_char_budget).await?;
    let rewritten = agent::refresh_if_stale(pool, llm, window_char_budget).await?;
    if tagged > 0 || rewritten {
        info!(
            tagged,
            rewritten,
            secs = started.elapsed().as_secs(),
            "briefing updated"
        );
    }
    Ok(())
}

/// Only one [`find_to_dos`] per database at a time: two would extract the
/// same new mail twice.
async fn extraction_lock(pool: &SqlitePool) -> tokio::sync::OwnedMutexGuard<()> {
    type Locks = std::sync::Mutex<
        std::collections::HashMap<std::path::PathBuf, Arc<tokio::sync::Mutex<()>>>,
    >;
    static LOCKS: std::sync::OnceLock<Locks> = std::sync::OnceLock::new();
    let key = std::path::PathBuf::from(pool.connect_options().get_filename());
    let lock = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("extraction locks poisoned")
        .entry(key)
        .or_default()
        .clone();
    lock.lock_owned().await
}

type Erased<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + 'a>>;

#[derive(Default)]
struct SourceCounts {
    channels_polled: i64,
    messages_ingested: i64,
    embeddings_drained: i64,
    actions_created: i64,
    errors: Vec<String>,
}

/// The reader's view of every folder (history, flags, removals); returns the
/// errors, which never fail the source's sync.
async fn reader_pass(
    pool: &SqlitePool,
    session: &mut crate::source::imap::ImapMailSession,
    source_id: i64,
    source_name: &str,
) -> Vec<String> {
    let started = Instant::now();
    let result = async {
        let counts = crate::mail::sync::sync_folders(pool, &mut *session, source_id).await?;
        let now = Utc::now().timestamp();
        let fetched = if crate::mail::cache::prefetch_allowed() {
            crate::mail::cache::prefetch(pool, &mut *session, source_id, now).await?
        } else {
            0
        };
        let evicted = crate::mail::cache::evict(pool, now).await?;
        anyhow::Ok((counts, fetched, evicted))
    }
    .await;
    match result {
        Ok((c, fetched, evicted)) => {
            info!(
                source_id,
                folders = c.folders,
                new = c.new,
                flag_changes = c.flag_changes,
                removed = c.removed,
                unchanged = c.unchanged,
                fetched,
                evicted,
                secs = started.elapsed().as_secs(),
                "source: mail synced"
            );
            c.errors
                .into_iter()
                .map(|e| format!("source '{source_name}': mail sync: {e}"))
                .collect()
        }
        Err(e) => vec![format!("source '{source_name}': mail sync: {e:#}")],
    }
}

/// Fetch the messages of a thread the user is opening that aren't cached.
pub async fn fetch_thread(pool: &SqlitePool, thread_id: i64) -> Result<usize> {
    let source_id: i64 = sqlx::query_scalar("SELECT source_id FROM mail_threads WHERE id = ?")
        .bind(thread_id)
        .fetch_one(pool)
        .await
        .context("finding the thread")?;
    let source = build_imap_source(pool, SourceId(source_id)).await?;
    let mut session = source.mail_session().await?;
    let fetched = crate::mail::cache::fetch_thread(pool, &mut session, source_id, thread_id).await;
    session.logout().await;
    fetched
}

/// Give header-only messages among `ids` their bodies: from the cache, else
/// downloaded from their account's server.
pub async fn complete_messages(pool: &SqlitePool, ids: &[i64]) -> Result<()> {
    let missing = crate::mail::cache::complete_from_cache(pool, ids).await?;
    if missing.is_empty() {
        return Ok(());
    }
    let mut by_source: Vec<(i64, Vec<i64>)> = Vec::new();
    for id in missing {
        let source_id: i64 = sqlx::query_scalar(
            "SELECT c.source_id FROM messages m JOIN channels c ON c.id = m.channel_id \
             WHERE m.id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await?;
        match by_source.iter_mut().find(|s| s.0 == source_id) {
            Some(s) => s.1.push(id),
            None => by_source.push((source_id, vec![id])),
        }
    }
    for (source_id, ids) in by_source {
        let source = build_imap_source(pool, SourceId(source_id)).await?;
        let mut session = source.mail_session().await?;
        let fetched = crate::mail::cache::fetch_messages(pool, &mut session, source_id, &ids).await;
        session.logout().await;
        fetched?;
    }
    Ok(())
}

/// Search every mail account's server; returns the hits' row ids (newest
/// first, at most `limit` per account) and the accounts that failed.
pub async fn search_mail_server(
    pool: &SqlitePool,
    criteria: &crate::mail::search::Criteria,
    limit: usize,
) -> Result<(Vec<i64>, Vec<String>)> {
    let sources: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM sources WHERE status != 'disabled' AND kind = 'imap' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("listing sources")?;
    let mut ids = Vec::new();
    let mut errors = Vec::new();
    for (source_id, source_name) in sources {
        // Search stores what it finds, as the sync does.
        let _guard = crate::mail::sync::account_lock(pool, source_id).await;
        let result = async {
            let source = build_imap_source(pool, SourceId(source_id)).await?;
            let mut session = source.mail_session().await?;
            let found =
                crate::mail::search::search_server(pool, &mut session, source_id, criteria, limit)
                    .await;
            session.logout().await;
            found
        }
        .await;
        match result {
            Ok(found) => ids.extend(found),
            Err(e) => errors.push(format!("{source_name}: {e:#}")),
        }
    }
    Ok((ids, errors))
}

/// Phase one of a sync: bring every mail account's folders up to date,
/// without extracting. New mail in the folders polled for to-dos is
/// ingested as extraction candidates (for [`find_to_dos`]), the rest as
/// reading-only mail. Quick enough to run often. Returns the errors.
pub async fn sync_mail(pool: &SqlitePool) -> Result<Vec<String>> {
    Ok(sync_mail_outcome(pool).await?.errors)
}

/// [`sync_mail`], with counts; records each account's health.
pub async fn sync_mail_outcome(pool: &SqlitePool) -> Result<SyncOutcome> {
    let sources: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM sources WHERE status != 'disabled' AND kind = 'imap' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("listing sources")?;
    let started = Instant::now();
    let mut out = SyncOutcome::default();
    for (source_id, source_name) in sources {
        let _guard = crate::mail::sync::account_lock(pool, source_id).await;
        // Erased, as is each account's extraction: left concrete, the nested
        // futures overflow the compiler's `Send` check in callers.
        // One session for the whole account: folders, polls, the reader.
        let result: Erased<'_, SourceCounts> = Box::pin(async {
            let source = build_imap_source(pool, SourceId(source_id)).await?;
            let mut session = source.mail_session().await?;
            let counts = async {
                match session.channels().await {
                    Ok(channels) => {
                        store_channels(pool, &channels, source_id).await?;
                    }
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), source_id, "channel discovery failed");
                    }
                }
                let mut counts = SourceCounts::default();
                let statuses = crate::mail::sync::MailServer::statuses(&mut session)
                    .await
                    .unwrap_or_default();
                poll_channels(
                    pool,
                    &mut session,
                    source_id,
                    &source_name,
                    None,
                    &statuses,
                    &mut counts,
                )
                .await?;
                counts
                    .errors
                    .extend(reader_pass(pool, &mut session, source_id, &source_name).await);
                anyhow::Ok(counts)
            }
            .await;
            session.logout().await;
            counts
        });
        let result = result.await;
        match result {
            Ok(counts) => {
                out.sources_synced += 1;
                out.channels_polled += counts.channels_polled;
                out.messages_ingested += counts.messages_ingested;
                out.errors.extend(counts.errors);
                mark_source_ok(pool, source_id).await.ok();
            }
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    source_id,
                    source_name = %source_name,
                    "source: mail sync failed"
                );
                out.sources_failed += 1;
                out.errors
                    .push(format!("source '{source_name}' (id={source_id}): {e:#}"));
                mark_source_failure(pool, source_id, &format!("{e:#}"))
                    .await
                    .ok();
            }
        }
    }
    info!(
        sources_synced = out.sources_synced,
        sources_failed = out.sources_failed,
        messages = out.messages_ingested,
        errors = out.errors.len(),
        secs = started.elapsed().as_secs(),
        "mail synced"
    );
    Ok(out)
}

/// Poll then extract one source in one go, so tests can inject a fake
/// `Source` without touching the keychain or the IMAP transport.
#[cfg(test)]
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
    if let Err(e) = discover_channels(pool, source, source_id).await {
        warn!(error = %format!("{e:#}"), source_id, "channel discovery failed");
    }
    let mut counts = SourceCounts::default();
    poll_channels(
        pool,
        &mut ViaSource(source),
        source_id,
        source_name,
        None,
        &Default::default(),
        &mut counts,
    )
    .await?;
    let extracted = extract_source(
        pool,
        source_id,
        source_name,
        llm,
        embedder,
        model_name,
        window_char_budget,
        idle_timeout,
        traces_dir,
    )
    .await?;
    counts.embeddings_drained = extracted.embeddings_drained;
    counts.actions_created = extracted.actions_created;
    counts.errors.extend(extracted.errors);
    Ok(counts)
}

/// Follow up the account's threads of open to-dos with new mail (see
/// [`crate::extract::followup`]). Failures are counted; the thread is tried
/// again on the next sync.
async fn follow_up_threads(
    pool: &SqlitePool,
    source_id: i64,
    source_name: &str,
    llm: &dyn LlmTransport,
    window_char_budget: usize,
    counts: &mut SourceCounts,
) {
    use crate::extract::followup;
    let threads = match followup::pending(pool, source_id).await {
        Ok(t) => t,
        Err(e) => {
            counts
                .errors
                .push(format!("source '{source_name}': follow-ups: {e:#}"));
            return;
        }
    };
    for thread in threads {
        let started = Instant::now();
        // Mail synced for reading only has no body yet.
        let result = match complete_messages(pool, &thread.message_ids).await {
            Ok(()) => followup::run(pool, llm, &thread, window_char_budget).await,
            Err(e) => Err(e.context("downloading the thread's new mail")),
        };
        match result {
            Ok(recorded) => {
                counts.actions_created += recorded as i64;
                info!(
                    thread_id = thread.thread_id,
                    to_dos = thread.action_ids.len(),
                    messages = thread.message_ids.len(),
                    recorded,
                    secs = started.elapsed().as_secs(),
                    "to-do thread followed up"
                );
            }
            Err(e) => {
                warn!(error = %format!("{e:#}"), thread_id = thread.thread_id, "to-do follow-up failed");
                counts.errors.push(format!(
                    "source '{source_name}': following up thread {}: {e:#}",
                    thread.thread_id
                ));
            }
        }
    }
}

/// Embed then extract one source's folders with mail not yet extracted.
#[allow(clippy::too_many_arguments)]
async fn extract_source(
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
    let mut counts = SourceCounts::default();
    // First the threads of open to-dos: their new mail may be in any folder,
    // and channel extraction leaves it to them.
    follow_up_threads(
        pool,
        source_id,
        source_name,
        llm,
        window_char_budget,
        &mut counts,
    )
    .await;
    let to_extract = channels_pending_extraction(pool, source_id).await?;
    if to_extract.is_empty() {
        return Ok(counts);
    }

    // Embed everything new in a single pass, while the embedding model is
    // the only one that needs to be hot. `drain_once` records per-entry
    // failures and keeps them queued (returning Ok), so a single transport
    // error surfaces once at the source level rather than once per channel.
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

    // Then extract each folder; only the chat model is exercised now, so the
    // embed model can stay evicted.
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
            crate::extract::Strategy::for_budget(window_char_budget),
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

/// Send queued read/unread changes now, without waiting for a sync: the
/// apps call this right after the user's change. Returns the errors; the
/// changes stay queued for the next sync.
pub async fn push_mail_changes(pool: &SqlitePool) -> Result<Vec<String>> {
    let sources: Vec<(i64, String)> = sqlx::query_as(
        "SELECT DISTINCT s.id, s.name FROM sources s \
         JOIN channels c ON c.source_id = s.id JOIN mail_ops o ON o.channel_id = c.id \
         ORDER BY s.id",
    )
    .fetch_all(pool)
    .await
    .context("listing accounts with mail changes")?;
    let mut errors = Vec::new();
    for (source_id, source_name) in sources {
        let _guard = crate::mail::sync::account_lock(pool, source_id).await;
        let result = async {
            let source = build_imap_source(pool, SourceId(source_id)).await?;
            let mut session = source.mail_session().await?;
            let flushed = crate::mail::ops::flush(pool, &mut session, source_id).await;
            session.logout().await;
            flushed
        }
        .await;
        if let Err(e) = result {
            errors.push(format!("source '{source_name}': {e:#}"));
        }
    }
    Ok(errors)
}

/// Poll every folder polled for to-dos and ingest what's new, whole: the
/// extraction candidates.
async fn poll_channels(
    pool: &SqlitePool,
    poller: &mut dyn Poller,
    source_id: i64,
    source_name: &str,
    only: Option<i64>,
    statuses: &std::collections::HashMap<String, crate::mail::sync::FolderStatus>,
    counts: &mut SourceCounts,
) -> Result<()> {
    let channels: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, external_id, cursor FROM channels \
         WHERE source_id = ?1 AND muted = 0 AND (?2 IS NULL OR id = ?2)",
    )
    .bind(source_id)
    .bind(only)
    .fetch_all(pool)
    .await
    .context("listing channels")?;

    let total_channels = channels.len();
    for (idx, (channel_id, external_id, cursor)) in channels.into_iter().enumerate() {
        // Nothing arrived since the poll's position, as the server's status
        // shows: no need to open the folder.
        if let Some(status) = statuses.get(&external_id)
            && cursor.as_deref()
                == Some(format!("{}:{}", status.uidvalidity, status.uidnext).as_str())
        {
            continue;
        }
        counts.channels_polled += 1;
        let ch_started = Instant::now();
        info!(
            channel_id,
            channel = %external_id,
            progress = format!("{}/{total_channels}", idx + 1),
            "channel: poll"
        );
        let cursor = cursor.map(Cursor);
        let batch = match poller.poll(&external_id, cursor.as_ref()).await {
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
    }
    Ok(())
}

/// What polls a folder for to-dos: an open IMAP session or, in tests, any
/// [`Source`].
#[async_trait::async_trait]
trait Poller: Send {
    async fn poll(&mut self, folder: &str, cursor: Option<&Cursor>) -> Result<PollBatch>;
}

#[async_trait::async_trait]
impl Poller for crate::source::imap::ImapMailSession {
    async fn poll(&mut self, folder: &str, cursor: Option<&Cursor>) -> Result<PollBatch> {
        crate::source::imap::ImapMailSession::poll(self, folder, cursor).await
    }
}

#[cfg(test)]
struct ViaSource<'a>(&'a dyn Source);

#[cfg(test)]
#[async_trait::async_trait]
impl Poller for ViaSource<'_> {
    async fn poll(&mut self, folder: &str, cursor: Option<&Cursor>) -> Result<PollBatch> {
        self.0.poll(folder, cursor).await
    }
}

/// Sync only what the user is looking at, a folder or every account's
/// inbox, for reading and, where it's polled, for to-dos: what pulling to
/// refresh waits on, ahead of the full sync. Returns the errors.
pub async fn sync_scope(pool: &SqlitePool, scope: mnemis_types::MailScope) -> Result<Vec<String>> {
    let (all_inboxes, channel_id) = match scope {
        mnemis_types::MailScope::AllInboxes => (true, None),
        mnemis_types::MailScope::Mailbox { channel_id } => (false, Some(channel_id)),
    };
    let folders: Vec<(i64, i64, String, String)> = sqlx::query_as(
        "SELECT c.id, c.source_id, c.external_id, s.name FROM channels c \
         JOIN sources s ON s.id = c.source_id \
         WHERE s.status != 'disabled' AND s.kind = 'imap' AND c.kind = 'mailbox' \
           AND ((?1 AND upper(c.external_id) = 'INBOX') OR c.id = ?2) \
         ORDER BY c.source_id, c.id",
    )
    .bind(all_inboxes)
    .bind(channel_id)
    .fetch_all(pool)
    .await
    .context("listing the folders to sync")?;
    let started = Instant::now();
    let mut errors = Vec::new();
    let mut i = 0;
    while i < folders.len() {
        let source_id = folders[i].1;
        let source_name = folders[i].3.clone();
        let mine: Vec<(i64, String)> = folders[i..]
            .iter()
            .take_while(|f| f.1 == source_id)
            .map(|f| (f.0, f.2.clone()))
            .collect();
        i += mine.len();
        let _guard = crate::mail::sync::account_lock(pool, source_id).await;
        let result: Erased<'_, Vec<String>> = Box::pin(async {
            let source = build_imap_source(pool, SourceId(source_id)).await?;
            let mut session = source.mail_session().await?;
            let synced = async {
                let mut errors = Vec::new();
                // The user's changes go first, so the flags read back
                // include them.
                if let Err(e) = crate::mail::ops::flush(pool, &mut session, source_id).await {
                    errors.push(format!("sending mail changes: {e:#}"));
                }
                for (channel_id, folder) in &mine {
                    let mut counts = SourceCounts::default();
                    poll_channels(
                        pool,
                        &mut session,
                        source_id,
                        &source_name,
                        Some(*channel_id),
                        &Default::default(),
                        &mut counts,
                    )
                    .await?;
                    errors.extend(counts.errors);
                    crate::mail::sync::sync_folder(
                        pool,
                        &mut session,
                        source_id,
                        *channel_id,
                        folder,
                        Utc::now(),
                    )
                    .await?;
                }
                anyhow::Ok(errors)
            }
            .await;
            session.logout().await;
            synced
        });
        match result.await {
            Ok(e) => errors.extend(e),
            Err(e) => errors.push(format!("source '{source_name}': {e:#}")),
        }
    }
    info!(
        folders = folders.len(),
        errors = errors.len(),
        millis = started.elapsed().as_millis() as u64,
        "mail on screen synced"
    );
    Ok(errors)
}

/// The account's folders polled for to-dos that hold mail newer than their
/// last successful extraction.
async fn channels_pending_extraction(
    pool: &SqlitePool,
    source_id: i64,
) -> Result<Vec<(i64, String)>> {
    sqlx::query_as(
        "SELECT c.id, c.external_id FROM channels c \
         WHERE c.source_id = ? AND c.muted = 0 AND EXISTS ( \
             SELECT 1 FROM messages m WHERE m.channel_id = c.id AND m.reader_only = 0 \
               AND m.id > COALESCE(( \
                   SELECT up_to_message_id FROM extraction_runs r \
                   WHERE r.channel_id = c.id AND r.result IN ('ok', 'no_activity') \
                   ORDER BY r.ran_at DESC, r.id DESC LIMIT 1), 0)) \
         ORDER BY c.id",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await
    .context("listing folders to extract")
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

    let auth = if parsed["auth"].as_str() == Some("google") {
        ImapAuth::Google {
            refresh_ref: config_ref,
        }
    } else {
        ImapAuth::Password(
            secrets::fetch(&config_ref)
                .await
                .with_context(|| format!("fetching keychain entry {config_ref}"))?,
        )
    };

    Ok(ImapSource::new(
        source_id,
        ImapConfig {
            server,
            port,
            username,
            auth,
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
    store_channels(pool, &channels, source_id).await
}

/// Record the account's folders: one seen for the first time takes the
/// source's default; one already known keeps what the user chose.
async fn store_channels(
    pool: &SqlitePool,
    channels: &[crate::source::ChannelInfo],
    source_id: i64,
) -> Result<usize> {
    for ch in channels {
        // A folder seen for the first time takes the source's default; one we
        // already know keeps whatever the user chose.
        sqlx::query(
            "INSERT INTO channels (source_id, external_id, name, kind, muted) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(source_id)
        .bind(&ch.external_id)
        .bind(&ch.name)
        .bind(&ch.kind)
        .bind(!ch.subscribed_by_default)
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
        let embedder: Arc<dyn Embedder> = Arc::new(crate::test_util::FailingEmbedder);
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
                    links: None,
                    raw_json: None,
                    flags: 0,
                    mail: None,
                    raw: None,
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
        let embedder: Arc<dyn Embedder> = Arc::new(crate::test_util::FailingEmbedder);

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
        let embedder: Arc<dyn Embedder> = Arc::new(crate::test_util::FailingEmbedder);

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
                    subscribed_by_default: true,
                },
                ChannelInfo {
                    external_id: "INBOX/Lembrar".to_string(),
                    name: "INBOX/Lembrar".to_string(),
                    kind: "mailbox".to_string(),
                    subscribed_by_default: false,
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
        let muted_of = |ext: &'static str| {
            let pool = pool.clone();
            async move {
                let (m,): (i64,) =
                    sqlx::query_as("SELECT muted FROM channels WHERE external_id = ?")
                        .bind(ext)
                        .fetch_one(&pool)
                        .await?;
                anyhow::Ok(m)
            }
        };
        assert_eq!(muted_of("INBOX").await?, 0, "subscribed by default");
        assert_eq!(muted_of("INBOX/Lembrar").await?, 1, "muted by default");

        // Flip both, then re-discover: no duplicates, the user's choice stands.
        sqlx::query("UPDATE channels SET muted = 1 - muted")
            .execute(&pool)
            .await?;
        discover_channels(&pool, &source, source_id).await?;

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM channels WHERE source_id = ?")
            .bind(source_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 2, "re-discovery adds no duplicates");
        assert_eq!(
            muted_of("INBOX").await?,
            1,
            "re-discovery preserves mute state"
        );
        assert_eq!(
            muted_of("INBOX/Lembrar").await?,
            0,
            "re-discovery preserves mute state"
        );
        Ok(())
    }
}
