//! Keeping the reader's view of each folder current: the headers of mail
//! from the last [`WINDOW_DAYS`], flags changed on the server (read
//! elsewhere, flagged, answered), and mail that left the folder. New mail in
//! folders polled for to-dos arrives through the extraction poll, which
//! fetches it whole; everything else lands here as reading-only rows (see
//! [`crate::ingest::ingest_for_reading`]).
//!
//! The algorithm runs over [`MailServer`], one open session per account, so
//! tests drive it with a fake server.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::warn;

use crate::source::SourceId;

/// How far back the thread lists reach.
pub const WINDOW_DAYS: i64 = 90;
/// UIDs per header fetch.
const CHUNK: usize = 50;

/// A folder as the server reports it when opened.
#[derive(Debug, Clone, Copy)]
pub struct FolderStatus {
    /// How many messages the folder holds.
    pub exists: u32,
    pub uidvalidity: u32,
    pub uidnext: u32,
    /// Present when the server tracks flag changes (CONDSTORE).
    pub highest_modseq: Option<u64>,
}

/// One message's headers, as fetched.
#[derive(Debug, Clone)]
pub struct FetchedHeaders {
    pub uid: u32,
    pub flags: u32,
    pub size: Option<u32>,
    pub header: Vec<u8>,
}

/// An account's mail server, one session.
#[async_trait]
pub trait MailServer: Send {
    /// Open a folder for the calls that follow.
    async fn open(&mut self, folder: &str) -> Result<FolderStatus>;
    /// UIDs of the mail in the open folder, all of it or what arrived since
    /// a date.
    async fn uids(&mut self, since: Option<DateTime<Utc>>) -> Result<Vec<u32>>;
    async fn headers(&mut self, uids: &[u32]) -> Result<Vec<FetchedHeaders>>;
    /// `(uid, flags)` of the mail whose flags changed since `modseq`, or of
    /// all mail when `None`.
    async fn flags(&mut self, changed_since: Option<u64>) -> Result<Vec<(u32, u32)>>;
    /// Mark mail in the open folder read or unread.
    async fn store_seen(&mut self, uids: &[u32], seen: bool) -> Result<()>;
}

/// What the folder sync last saw, kept in `channels.mail_state`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MailState {
    uidvalidity: Option<u32>,
    modseq: Option<u64>,
}

#[derive(Debug, Default)]
pub struct SyncCounts {
    pub folders: usize,
    pub new: usize,
    pub flag_changes: usize,
    pub removed: usize,
    pub errors: Vec<String>,
}

/// Only one sync per account at a time: the extraction sync and the
/// reader's quick sync both write placements and cursors.
pub async fn account_lock(pool: &SqlitePool, source_id: i64) -> tokio::sync::OwnedMutexGuard<()> {
    type Locks = Mutex<HashMap<(PathBuf, i64), Arc<tokio::sync::Mutex<()>>>>;
    static LOCKS: LazyLock<Locks> = LazyLock::new(Default::default);
    let key = (
        pool.connect_options().get_filename().to_path_buf(),
        source_id,
    );
    let lock = LOCKS
        .lock()
        .expect("account locks")
        .entry(key)
        .or_default()
        .clone();
    lock.lock_owned().await
}

/// Sync every folder of an account. A folder that fails is reported and
/// skipped; the rest still sync.
pub async fn sync_folders(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
) -> Result<SyncCounts> {
    let channels: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, external_id FROM channels WHERE source_id = ? AND kind = 'mailbox' ORDER BY id",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await
    .context("listing folders")?;
    let mut counts = SyncCounts::default();
    // The user's changes go first, so the flags read back include them.
    if let Err(e) = super::ops::flush(pool, server, source_id).await {
        counts.errors.push(format!("sending mail changes: {e:#}"));
    }
    for (channel_id, folder) in channels {
        match sync_folder(pool, server, source_id, channel_id, &folder, Utc::now()).await {
            Ok(c) => {
                counts.folders += 1;
                counts.new += c.new;
                counts.flag_changes += c.flag_changes;
                counts.removed += c.removed;
            }
            Err(e) => {
                warn!(error = %format!("{e:#}"), folder, "mail sync: folder failed");
                counts.errors.push(format!("folder '{folder}': {e:#}"));
            }
        }
    }
    Ok(counts)
}

pub async fn sync_folder(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
    channel_id: i64,
    folder: &str,
    now: DateTime<Utc>,
) -> Result<SyncCounts> {
    let mut counts = SyncCounts::default();
    let status = server.open(folder).await?;
    let empty = status.exists == 0;
    let (state, muted, cursor): (Option<String>, bool, Option<String>) =
        sqlx::query_as("SELECT mail_state, muted, cursor FROM channels WHERE id = ?")
            .bind(channel_id)
            .fetch_one(pool)
            .await?;
    let mut state: MailState = state
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if state.uidvalidity != Some(status.uidvalidity) {
        // The server renumbered the folder: forget the old UIDs; the rows are
        // matched again by Message-ID below.
        sqlx::query("UPDATE messages SET uid = NULL, uidvalidity = NULL WHERE channel_id = ?")
            .bind(channel_id)
            .execute(pool)
            .await?;
        state = MailState {
            uidvalidity: Some(status.uidvalidity),
            modseq: None,
        };
    }

    let known: HashSet<u32> = sqlx::query_scalar::<_, i64>(
        "SELECT uid FROM messages WHERE channel_id = ? AND uid IS NOT NULL AND removed_at IS NULL",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|u| u as u32)
    .collect();

    // Mail that left the folder.
    let present: HashSet<u32> = if empty {
        HashSet::new()
    } else {
        server.uids(None).await?.into_iter().collect()
    };
    let gone: Vec<u32> = known.difference(&present).copied().collect();
    if !gone.is_empty() {
        sqlx::query(
            "UPDATE messages SET removed_at = ? \
             WHERE channel_id = ? AND uid IN (SELECT value FROM json_each(?))",
        )
        .bind(now.timestamp())
        .bind(channel_id)
        .bind(serde_json::to_string(&gone)?)
        .execute(pool)
        .await?;
        counts.removed = gone.len();
    }

    // New mail in the window. In a folder polled for to-dos, mail from the
    // poll's position on belongs to the poll (it fetches it whole, for
    // extraction); until the poll has a position there, leave it all to it.
    let poll_from = if muted {
        None
    } else {
        match cursor.as_deref().and_then(poll_position) {
            Some((validity, next)) if validity == status.uidvalidity => Some(next),
            _ => Some(0),
        }
    };
    let in_window = if empty {
        Vec::new()
    } else {
        server.uids(Some(now - Duration::days(WINDOW_DAYS))).await?
    };
    let mut new: Vec<u32> = in_window
        .into_iter()
        .filter(|uid| !known.contains(uid) && poll_from.is_none_or(|limit| *uid < limit))
        .collect();
    // Newest first, so an interrupted first sync has filled the top of the
    // list; each chunk is saved as it arrives, so the next run resumes.
    new.sort_unstable_by(|a, b| b.cmp(a));
    for chunk in new.chunks(CHUNK) {
        let messages: Vec<_> = server
            .headers(chunk)
            .await?
            .into_iter()
            .filter_map(|h| {
                let mut m = crate::source::imap::imported(
                    &h.header,
                    h.uid,
                    status.uidvalidity,
                    h.flags,
                    true,
                )?;
                if let Some(mail) = m.mail.as_mut() {
                    mail.size = h.size;
                }
                Some(m)
            })
            .collect();
        counts.new +=
            crate::ingest::ingest_for_reading(pool, SourceId(source_id), channel_id, &messages)
                .await?;
    }

    // Flags changed on the server. Without change tracking, every message's
    // flags come back and are compared. A change made here and not yet sent
    // (`mail_ops`) wins over the server's older value.
    let since = state.modseq.filter(|_| status.highest_modseq.is_some());
    let flags = if empty {
        Vec::new()
    } else {
        server.flags(since).await?
    };
    let mut tx = pool.begin().await?;
    for (uid, flags) in flags {
        let changed = sqlx::query(
            "UPDATE messages SET flags = ? \
             WHERE channel_id = ? AND uid = ? AND flags != ? \
               AND NOT EXISTS (SELECT 1 FROM mail_ops o \
                               WHERE o.channel_id = messages.channel_id AND o.uid = messages.uid)",
        )
        .bind(flags as i64)
        .bind(channel_id)
        .bind(uid as i64)
        .bind(flags as i64)
        .execute(&mut *tx)
        .await?;
        counts.flag_changes += changed.rows_affected() as usize;
    }
    state.modseq = status.highest_modseq;
    sqlx::query("UPDATE channels SET mail_state = ? WHERE id = ?")
        .bind(serde_json::to_string(&state)?)
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(counts)
}

/// `(UIDVALIDITY, UIDNEXT)` from an extraction poll cursor.
fn poll_position(cursor: &str) -> Option<(u32, u32)> {
    let (validity, next) = cursor.split_once(':')?;
    Some((validity.parse().ok()?, next.parse().ok()?))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    #[derive(Clone)]
    pub(crate) struct FakeMessage {
        pub id: String,
        pub subject: String,
        pub date: DateTime<Utc>,
        pub flags: u32,
        pub modseq: u64,
    }

    #[derive(Default)]
    pub(crate) struct FakeFolder {
        pub uidvalidity: u32,
        pub messages: BTreeMap<u32, FakeMessage>,
    }

    /// A mail server in memory: folders of messages with a date, flags and
    /// the MODSEQ of their last flag change.
    #[derive(Default)]
    pub(crate) struct FakeServer {
        pub folders: HashMap<String, FakeFolder>,
        pub condstore: bool,
        /// Every store: folder, UIDs, read or unread.
        pub stored: Vec<(String, Vec<u32>, bool)>,
        /// Fail every store, as when offline.
        pub offline: bool,
        pub open: String,
    }

    impl FakeServer {
        pub fn folder(&mut self, name: &str) -> &mut FakeFolder {
            self.folders
                .entry(name.to_string())
                .or_insert_with(|| FakeFolder {
                    uidvalidity: 1,
                    ..Default::default()
                })
        }

        pub fn add(&mut self, folder: &str, uid: u32, id: &str, subject: &str, age_days: i64) {
            self.folder(folder).messages.insert(
                uid,
                FakeMessage {
                    id: id.to_string(),
                    subject: subject.to_string(),
                    date: Utc::now() - Duration::days(age_days),
                    flags: 0,
                    modseq: 1,
                },
            );
        }

        fn current(&self) -> &FakeFolder {
            &self.folders[&self.open]
        }
    }

    pub(crate) fn header(m: &FakeMessage) -> Vec<u8> {
        format!(
            "Message-ID: <{}>\r\nSubject: {}\r\nFrom: Ana <ana@example.com>\r\nDate: {}\r\n\r\n",
            m.id,
            m.subject,
            m.date.to_rfc2822()
        )
        .into_bytes()
    }

    #[async_trait]
    impl MailServer for FakeServer {
        async fn open(&mut self, folder: &str) -> Result<FolderStatus> {
            let f = self.folders.get(folder).context("no such folder")?;
            let status = FolderStatus {
                exists: f.messages.len() as u32,
                uidvalidity: f.uidvalidity,
                uidnext: f.messages.keys().last().map_or(1, |u| u + 1),
                highest_modseq: self
                    .condstore
                    .then(|| f.messages.values().map(|m| m.modseq).max().unwrap_or(1)),
            };
            self.open = folder.to_string();
            Ok(status)
        }

        async fn uids(&mut self, since: Option<DateTime<Utc>>) -> Result<Vec<u32>> {
            Ok(self
                .current()
                .messages
                .iter()
                .filter(|(_, m)| since.is_none_or(|s| m.date >= s))
                .map(|(u, _)| *u)
                .collect())
        }

        async fn headers(&mut self, uids: &[u32]) -> Result<Vec<FetchedHeaders>> {
            let f = self.current();
            Ok(uids
                .iter()
                .filter_map(|u| {
                    let m = f.messages.get(u)?;
                    Some(FetchedHeaders {
                        uid: *u,
                        flags: m.flags,
                        size: Some(1000),
                        header: header(m),
                    })
                })
                .collect())
        }

        async fn flags(&mut self, changed_since: Option<u64>) -> Result<Vec<(u32, u32)>> {
            Ok(self
                .current()
                .messages
                .iter()
                .filter(|(_, m)| changed_since.is_none_or(|s| m.modseq > s))
                .map(|(u, m)| (*u, m.flags))
                .collect())
        }

        async fn store_seen(&mut self, uids: &[u32], seen: bool) -> Result<()> {
            anyhow::ensure!(!self.offline, "offline");
            let open = self.open.clone();
            let folder = self.folders.get_mut(&open).context("no folder open")?;
            let modseq = folder
                .messages
                .values()
                .map(|m| m.modseq)
                .max()
                .unwrap_or(1)
                + 1;
            for uid in uids {
                if let Some(m) = folder.messages.get_mut(uid) {
                    m.flags = if seen { m.flags | 1 } else { m.flags & !1 };
                    m.modseq = modseq;
                }
            }
            self.stored.push((open, uids.to_vec(), seen));
            Ok(())
        }
    }

    pub(crate) async fn setup() -> Result<(TempDir, SqlitePool, i64)> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("mnemis.db")).await?;
        db::migrate(&pool).await?;
        let source_id = sqlx::query_scalar(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'Fastmail', 'kc', 0) RETURNING id",
        )
        .fetch_one(&pool)
        .await?;
        Ok((tmp, pool, source_id))
    }

    pub(crate) async fn folder(
        pool: &SqlitePool,
        source_id: i64,
        name: &str,
        muted: bool,
    ) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "INSERT INTO channels (source_id, external_id, name, kind, muted) \
             VALUES (?, ?, ?, 'mailbox', ?) RETURNING id",
        )
        .bind(source_id)
        .bind(name)
        .bind(name)
        .bind(muted)
        .fetch_one(pool)
        .await?)
    }

    async fn row(
        pool: &SqlitePool,
        id: &str,
    ) -> Result<(Option<i64>, i64, Option<i64>, bool, bool)> {
        Ok(sqlx::query_as(
            "SELECT uid, flags, removed_at, reader_only, has_body FROM messages WHERE external_id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await?)
    }

    async fn count(pool: &SqlitePool) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(pool)
            .await?)
    }

    #[tokio::test]
    async fn keeps_a_muted_folder_current_for_reading() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        let archive = folder(&pool, source_id, "Archive", true).await?;
        let mut server = FakeServer {
            condstore: true,
            ..Default::default()
        };
        server.add("Archive", 1, "old@x", "Last year", 400);
        server.add("Archive", 2, "a@x", "Lunch", 10);
        server.add("Archive", 3, "b@x", "Dinner", 1);

        let first = sync_folders(&pool, &mut server, source_id).await?;
        assert_eq!(
            first.new, 2,
            "mail older than the window stays on the server"
        );
        let (uid, _, _, reader_only, has_body) = row(&pool, "a@x").await?;
        assert_eq!(uid, Some(2));
        assert!(reader_only && !has_body, "headers only, never for to-dos");

        // Read elsewhere, and another one deleted.
        let m = server.folder("Archive").messages.get_mut(&2).unwrap();
        m.flags = 0x01;
        m.modseq = 5;
        server.folder("Archive").messages.remove(&3);
        let second = sync_folder(
            &pool,
            &mut server,
            source_id,
            archive,
            "Archive",
            Utc::now(),
        )
        .await?;
        assert_eq!((second.new, second.flag_changes, second.removed), (0, 1, 1));
        assert_eq!(row(&pool, "a@x").await?.1, 0x01);
        assert!(
            row(&pool, "b@x").await?.2.is_some(),
            "marked gone, not deleted"
        );
        assert_eq!(count(&pool).await?, 2);
        Ok(())
    }

    #[tokio::test]
    async fn leaves_new_mail_to_the_poll_and_places_what_it_already_stored() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        let inbox = folder(&pool, source_id, "INBOX", false).await?;
        sqlx::query("UPDATE channels SET cursor = '1:10' WHERE id = ?")
            .bind(inbox)
            .execute(&pool)
            .await?;
        // Polled before the reader existed: whole, but with no UID.
        sqlx::query(
            "INSERT INTO messages (channel_id, external_id, posted_at, subject, body, body_format, ingested_at) \
             VALUES (?, 'a@x', 0, 'Lunch', 'the body', 'text', 0)",
        )
        .bind(inbox)
        .execute(&pool)
        .await?;
        let mut server = FakeServer::default();
        server.add("INBOX", 3, "a@x", "Lunch", 5);
        server.add("INBOX", 5, "b@x", "Older mail", 20);
        server.add("INBOX", 12, "new@x", "Just arrived", 0);

        sync_folders(&pool, &mut server, source_id).await?;
        let (uid, _, _, reader_only, has_body) = row(&pool, "a@x").await?;
        assert_eq!(uid, Some(3), "the stored row gains its placement");
        assert!(!reader_only && has_body, "and stays an extraction message");
        assert!(
            row(&pool, "b@x").await?.3,
            "history before the poll is reading-only"
        );
        let new: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE external_id = 'new@x'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(new, 0, "mail from the poll's position on is the poll's");
        Ok(())
    }

    #[tokio::test]
    async fn a_renumbered_folder_is_matched_again_by_message_id() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        let archive = folder(&pool, source_id, "Archive", true).await?;
        let mut server = FakeServer::default();
        server.add("Archive", 7, "a@x", "Lunch", 3);
        sync_folders(&pool, &mut server, source_id).await?;

        let f = server.folder("Archive");
        f.uidvalidity = 2;
        let m = f.messages.remove(&7).unwrap();
        f.messages.insert(1, m);
        let again = sync_folder(
            &pool,
            &mut server,
            source_id,
            archive,
            "Archive",
            Utc::now(),
        )
        .await?;
        assert_eq!((again.new, again.removed), (0, 0));
        assert_eq!(row(&pool, "a@x").await?.0, Some(1));
        assert_eq!(count(&pool).await?, 1);
        Ok(())
    }
}
