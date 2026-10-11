//! Push: while the app runs, the server says when mail changes instead of
//! the app asking on a timer. Each account gets its own connection that
//! only waits, using the best the server announces: NOTIFY (every folder on
//! one connection), else IDLE (one connection per folder: the inbox and the
//! folders polled for to-dos, a few at most), else nothing, and the app
//! keeps its timed syncs. A change syncs just the folders it touched
//! ([`crate::orchestrator::sync_scope`]), a moment later so a burst is one
//! sync, then the app hears of it.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::source::SourceId;
use crate::source::imap::{ImapMailSession, Wake};

/// How long one IDLE lasts before it's renewed: servers may drop an IDLE
/// after 30 minutes of quiet (RFC 2177).
const RENEW: Duration = Duration::from_secs(25 * 60);
/// After a change, how long to wait for more before syncing.
const SETTLE: Duration = Duration::from_millis(1500);
/// Most folders watched with plain IDLE, a connection each.
const MAX_IDLE_FOLDERS: usize = 4;
/// Seconds before reconnecting after a failure, growing to the last.
const BACKOFF: [u64; 4] = [5, 30, 120, 300];
/// A watch that lasted this long resets the backoff.
const HEALTHY: Duration = Duration::from_secs(600);

/// A change the app hears about, once its folders have synced.
#[derive(Debug, Clone, Default)]
pub struct MailChange {
    /// The folders (channels) that changed.
    pub channels: Vec<i64>,
    /// Some of them are polled for to-dos: new mail there wants extraction.
    pub to_dos: bool,
    pub errors: Vec<String>,
}

pub type OnChange = Arc<dyn Fn(MailChange) + Send + Sync>;

/// Watches every account while it lives; dropping it stops.
pub struct MailWatch {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    watching: Arc<AtomicUsize>,
}

impl MailWatch {
    /// Whether some account is being watched now; without it the app's
    /// timed syncs are what bring mail.
    pub fn active(&self) -> bool {
        self.watching.load(Ordering::Relaxed) > 0
    }
}

impl Drop for MailWatch {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Start watching every IMAP account, on the current Tokio runtime.
pub async fn start(pool: SqlitePool, on_change: OnChange) -> Result<MailWatch> {
    let sources: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM sources WHERE status != 'disabled' AND kind = 'imap' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .context("listing accounts to watch")?;
    let watching = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for source_id in sources {
        let (tx, rx) = mpsc::unbounded_channel();
        tasks.push(tokio::spawn(sync_changes(
            pool.clone(),
            rx,
            on_change.clone(),
        )));
        tasks.push(tokio::spawn(supervise(
            pool.clone(),
            source_id,
            tx,
            watching.clone(),
        )));
    }
    Ok(MailWatch { tasks, watching })
}

/// Keep one account watched: reconnect after failures, with backoff; give
/// up only when the server can't push at all.
async fn supervise(
    pool: SqlitePool,
    source_id: i64,
    tx: mpsc::UnboundedSender<i64>,
    watching: Arc<AtomicUsize>,
) {
    let mut failures = 0;
    loop {
        let started = Instant::now();
        let result = watch_account(&pool, source_id, &tx, &watching).await;
        match result {
            Ok(false) => {
                info!(
                    source_id,
                    "mail watch: the server can't push; timed syncs only"
                );
                return;
            }
            Ok(true) => {}
            Err(e) => warn!(error = %format!("{e:#}"), source_id, "mail watch dropped"),
        }
        if started.elapsed() > HEALTHY {
            failures = 0;
        }
        let wait = BACKOFF[failures.min(BACKOFF.len() - 1)];
        failures += 1;
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

/// Counts an account as watched while alive.
struct Watching(Arc<AtomicUsize>);

impl Watching {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Watching(count.clone())
    }
}

impl Drop for Watching {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Watch one account until the connection fails (an error) or the server
/// turns out unable to push (`false`).
async fn watch_account(
    pool: &SqlitePool,
    source_id: i64,
    tx: &mpsc::UnboundedSender<i64>,
    watching: &Arc<AtomicUsize>,
) -> Result<bool> {
    let source = crate::orchestrator::build_imap_source(pool, SourceId(source_id)).await?;
    let mut session = source.mail_session().await?;
    if session.has("NOTIFY") && session.has("IDLE") {
        match session.notify_all().await {
            Ok(()) => {
                info!(source_id, "mail watch: NOTIFY");
                let _watching = Watching::new(watching);
                return notify_loop(pool, source_id, session, tx).await;
            }
            Err(e) => warn!(error = %format!("{e:#}"), source_id, "NOTIFY refused; trying IDLE"),
        }
    }
    let idle = session.has("IDLE");
    session.logout().await;
    if !idle {
        return Ok(false);
    }
    let folders = idle_folders(pool, source_id).await?;
    info!(
        source_id,
        folders = folders.len(),
        "mail watch: IDLE, a connection per folder"
    );
    let _watching = Watching::new(watching);
    let mut loops = tokio::task::JoinSet::new();
    for (channel_id, folder) in folders {
        let session = source.mail_session().await?;
        loops.spawn(idle_loop(session, channel_id, folder, tx.clone()));
    }
    // The first to fail ends the rest, and the watch reconnects whole.
    match loops.join_next().await {
        Some(Ok(result)) => result.map(|()| true),
        Some(Err(e)) => Err(e).context("watch task"),
        None => Ok(true),
    }
}

async fn notify_loop(
    pool: &SqlitePool,
    source_id: i64,
    mut session: ImapMailSession,
    tx: &mpsc::UnboundedSender<i64>,
) -> Result<bool> {
    loop {
        let (next, wake) = session.idle(RENEW).await?;
        session = next;
        if let Wake::Folders(folders) = wake {
            for folder in folders {
                let channel: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM channels WHERE source_id = ? AND external_id = ?",
                )
                .bind(source_id)
                .bind(&folder)
                .fetch_optional(pool)
                .await?;
                if let Some(channel) = channel {
                    let _ = tx.send(channel);
                }
            }
        }
    }
}

async fn idle_loop(
    mut session: ImapMailSession,
    channel_id: i64,
    folder: String,
    tx: mpsc::UnboundedSender<i64>,
) -> Result<()> {
    crate::mail::sync::MailServer::open(&mut session, &folder).await?;
    loop {
        let (next, wake) = session.idle(RENEW).await?;
        session = next;
        if wake == Wake::Selected {
            let _ = tx.send(channel_id);
        }
    }
}

/// The folders plain IDLE watches: the inbox first, then those polled for
/// to-dos, at most [`MAX_IDLE_FOLDERS`].
async fn idle_folders(pool: &SqlitePool, source_id: i64) -> Result<Vec<(i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, external_id FROM channels \
         WHERE source_id = ? AND kind = 'mailbox' AND (upper(external_id) = 'INBOX' OR muted = 0) \
         ORDER BY upper(external_id) != 'INBOX', id LIMIT ?",
    )
    .bind(source_id)
    .bind(MAX_IDLE_FOLDERS as i64)
    .fetch_all(pool)
    .await?)
}

/// Sync the folders changes name, a burst at a time, and tell the app.
async fn sync_changes(pool: SqlitePool, mut rx: mpsc::UnboundedReceiver<i64>, on_change: OnChange) {
    while let Some(first) = rx.recv().await {
        let mut channels = BTreeSet::from([first]);
        tokio::time::sleep(SETTLE).await;
        while let Ok(more) = rx.try_recv() {
            channels.insert(more);
        }
        let mut change = MailChange {
            channels: channels.iter().copied().collect(),
            ..Default::default()
        };
        for &channel_id in &channels {
            match crate::orchestrator::sync_scope(
                &pool,
                mnemis_types::MailScope::Mailbox { channel_id },
            )
            .await
            {
                Ok(errors) => change.errors.extend(errors),
                Err(e) => change.errors.push(format!("{e:#}")),
            }
        }
        change.to_dos = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM channels \
             WHERE id IN (SELECT value FROM json_each(?)) AND muted = 0)",
        )
        .bind(serde_json::to_string(&change.channels).unwrap_or_default())
        .fetch_one(&pool)
        .await
        .unwrap_or(false);
        info!(
            folders = change.channels.len(),
            to_dos = change.to_dos,
            errors = change.errors.len(),
            "mail watch: synced a change"
        );
        on_change(change);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::sync::tests::{folder, setup};

    #[tokio::test]
    async fn idle_watches_the_inbox_then_to_do_folders_a_few_at_most() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        for name in ["Archive", "Work", "INBOX", "Clients", "Family", "Bills"] {
            // Archive alone isn't polled for to-dos (muted).
            folder(&pool, source_id, name, name == "Archive").await?;
        }
        let names: Vec<String> = idle_folders(&pool, source_id)
            .await?
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        assert_eq!(names, ["INBOX", "Work", "Clients", "Family"]);
        Ok(())
    }
}
