//! Keeping the reader's view of each folder current: the headers of mail
//! from the last [`WINDOW_DAYS`], flags changed on the server (read
//! elsewhere, flagged, answered), and mail that left the folder. New mail in
//! folders polled for to-dos arrives through the extraction poll, which
//! fetches it whole; everything else lands here as reading-only rows (see
//! [`crate::ingest::ingest_for_reading`]).
//!
//! The algorithm runs over [`MailServer`], an open session to the account
//! (a few when there are many folders to open), so tests drive it with a
//! fake server.

use std::collections::{HashMap, HashSet, VecDeque};
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
/// Sessions an account's sync may open folders on at once.
const SESSIONS: usize = 4;
/// Folders to open for each session beyond the first: fewer aren't worth
/// another login.
const PER_SESSION: usize = 6;

/// A folder as the server reports it when opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderStatus {
    /// How many messages the folder holds.
    pub exists: u32,
    pub uidvalidity: u32,
    pub uidnext: u32,
    /// Present when the server tracks flag changes (CONDSTORE).
    pub highest_modseq: Option<u64>,
}

/// With change tracking, the same UIDVALIDITY, MODSEQ, UIDNEXT and message
/// count mean nothing arrived, left or changed flags since the last sync.
fn unchanged(state: &MailState, status: &FolderStatus) -> bool {
    status.highest_modseq.is_some()
        && state.uidvalidity == Some(status.uidvalidity)
        && state.modseq == status.highest_modseq
        && state.uidnext == Some(status.uidnext)
        && state.exists == Some(status.exists)
}

/// Whether the folder holds mail stored before arrival times or sender
/// verdicts were kept ([`fill_missing`] needs the folder open).
async fn missing_anything(pool: &SqlitePool, channel_id: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE channel_id = ? AND uid IS NOT NULL \
           AND removed_at IS NULL AND (received_at IS NULL OR auth_json IS NULL))",
    )
    .bind(channel_id)
    .fetch_one(pool)
    .await?)
}

/// Fill in, for the folder's mail stored before they were kept, when the
/// server received it and its verdict on the sender: what tells forged mail
/// (see [`super::suspicion`]). A few bytes a message, once.
async fn fill_missing(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    channel_id: i64,
    uidvalidity: u32,
) -> Result<()> {
    let undated: Vec<u32> = sqlx::query_scalar::<_, i64>(
        "SELECT uid FROM messages WHERE channel_id = ? AND uidvalidity = ? \
           AND uid IS NOT NULL AND received_at IS NULL AND removed_at IS NULL",
    )
    .bind(channel_id)
    .bind(uidvalidity as i64)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|u| u as u32)
    .collect();
    for chunk in undated.chunks(500) {
        let received = server.received(chunk).await?;
        let mut tx = pool.begin().await?;
        for (uid, at) in received {
            sqlx::query(
                "UPDATE messages SET received_at = ? \
                 WHERE channel_id = ? AND uidvalidity = ? AND uid = ?",
            )
            .bind(at.timestamp())
            .bind(channel_id)
            .bind(uidvalidity as i64)
            .bind(uid as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }

    let unchecked: Vec<u32> = sqlx::query_scalar::<_, i64>(
        "SELECT uid FROM messages WHERE channel_id = ? AND uidvalidity = ? \
           AND uid IS NOT NULL AND auth_json IS NULL AND removed_at IS NULL",
    )
    .bind(channel_id)
    .bind(uidvalidity as i64)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|u| u as u32)
    .collect();
    for chunk in unchecked.chunks(500) {
        let headers = server.auth_headers(chunk).await?;
        let mut tx = pool.begin().await?;
        for (uid, header) in headers {
            let auth = super::suspicion::Auth::from_headers(&header);
            sqlx::query(
                "UPDATE messages SET auth_json = ? \
                 WHERE channel_id = ? AND uidvalidity = ? AND uid = ?",
            )
            .bind(serde_json::to_string(&auth)?)
            .bind(channel_id)
            .bind(uidvalidity as i64)
            .bind(uid as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

/// One message's headers, as fetched.
#[derive(Debug, Clone)]
pub struct FetchedHeaders {
    pub uid: u32,
    pub flags: u32,
    pub size: Option<u32>,
    pub header: Vec<u8>,
    pub received_at: Option<DateTime<Utc>>,
}

/// One whole message, as fetched.
#[derive(Debug, Clone)]
pub struct FetchedMessage {
    pub uid: u32,
    pub flags: u32,
    pub raw: Vec<u8>,
    pub received_at: Option<DateTime<Utc>>,
}

/// An account's mail server, one session.
#[async_trait]
pub trait MailServer: Send {
    /// Up to `count` more sessions to the same account, to open folders
    /// alongside this one; fewer where they can't be had.
    async fn others(&mut self, _count: usize) -> Vec<Box<dyn MailServer>> {
        Vec::new()
    }
    /// Log out of a session from [`MailServer::others`].
    async fn close(self: Box<Self>) {}
    /// Open a folder for the calls that follow.
    async fn open(&mut self, folder: &str) -> Result<FolderStatus>;
    /// Every folder's status in one go, where the server can (LIST-STATUS,
    /// with CONDSTORE for flag changes); empty where it can't, and the
    /// sync opens each folder instead.
    async fn statuses(&mut self) -> Result<HashMap<String, FolderStatus>>;
    /// UIDs of the mail in the open folder, all of it or what arrived since
    /// a date.
    async fn uids(&mut self, since: Option<DateTime<Utc>>) -> Result<Vec<u32>>;
    async fn headers(&mut self, uids: &[u32]) -> Result<Vec<FetchedHeaders>>;
    async fn messages(&mut self, uids: &[u32]) -> Result<Vec<FetchedMessage>>;
    /// `(uid, flags)` of the mail whose flags changed since `modseq`, or of
    /// all mail when `None`.
    async fn flags(&mut self, changed_since: Option<u64>) -> Result<Vec<(u32, u32)>>;
    /// The `Authentication-Results` headers of each of `uids` in the open
    /// folder, as a header section.
    async fn auth_headers(&mut self, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>>;
    /// When the server received each of `uids` in the open folder.
    async fn received(&mut self, uids: &[u32]) -> Result<Vec<(u32, DateTime<Utc>)>>;
    /// Mark mail in the open folder read or unread.
    async fn store_seen(&mut self, uids: &[u32], seen: bool) -> Result<()>;
    /// UIDs in the open folder matching `criteria`, searched by the server.
    async fn search(&mut self, criteria: &super::search::Criteria) -> Result<Vec<u32>>;
}

/// What the folder sync last saw, kept in `channels.mail_state`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MailState {
    uidvalidity: Option<u32>,
    modseq: Option<u64>,
    #[serde(default)]
    uidnext: Option<u32>,
    #[serde(default)]
    exists: Option<u32>,
}

#[derive(Debug, Default)]
pub struct SyncCounts {
    pub folders: usize,
    pub new: usize,
    pub flag_changes: usize,
    pub removed: usize,
    /// Folders the server showed unchanged since the last sync.
    pub unchanged: usize,
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
    // What changed, asked of the server once, where it can say: a folder it
    // shows unchanged isn't opened.
    let statuses = server.statuses().await.unwrap_or_else(|e| {
        warn!(error = %format!("{e:#}"), "mail sync: folder statuses failed");
        HashMap::new()
    });
    // Each folder left to open costs round trips: with many (no LIST-STATUS,
    // or the first sync), more sessions share them.
    let to_open = to_open(pool, &channels, &statuses).await?;
    let mut others = server
        .others((to_open / PER_SESSION).min(SESSIONS - 1))
        .await;
    let queue = Mutex::new(channels.into_iter().collect::<VecDeque<_>>());
    let mut sessions: Vec<&mut dyn MailServer> = vec![server];
    sessions.extend(others.iter_mut().map(|s| &mut **s));
    let shares = futures::future::join_all(sessions.into_iter().map(|session| {
        let (queue, statuses) = (&queue, &statuses);
        async move {
            let mut counts = SyncCounts::default();
            loop {
                let Some((channel_id, folder)) = queue.lock().expect("folder queue").pop_front()
                else {
                    break;
                };
                let known = statuses.get(&folder);
                match sync_folder_known(
                    pool,
                    session,
                    source_id,
                    channel_id,
                    &folder,
                    known,
                    Utc::now(),
                )
                .await
                {
                    Ok(c) => {
                        counts.folders += 1;
                        counts.new += c.new;
                        counts.flag_changes += c.flag_changes;
                        counts.removed += c.removed;
                        counts.unchanged += c.unchanged;
                    }
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), folder, "mail sync: folder failed");
                        counts.errors.push(format!("folder '{folder}': {e:#}"));
                    }
                }
            }
            counts
        }
    }))
    .await;
    for other in others {
        other.close().await;
    }
    for share in shares {
        counts.folders += share.folders;
        counts.new += share.new;
        counts.flag_changes += share.flag_changes;
        counts.removed += share.removed;
        counts.unchanged += share.unchanged;
        counts.errors.extend(share.errors);
    }
    Ok(counts)
}

/// How many of `channels` the sync will open: all but those the server
/// showed unchanged.
async fn to_open(
    pool: &SqlitePool,
    channels: &[(i64, String)],
    statuses: &HashMap<String, FolderStatus>,
) -> Result<usize> {
    if statuses.is_empty() {
        return Ok(channels.len());
    }
    let states: HashMap<i64, Option<String>> =
        sqlx::query_as::<_, (i64, Option<String>)>("SELECT id, mail_state FROM channels")
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
    Ok(channels
        .iter()
        .filter(|(id, folder)| {
            let state: MailState = states
                .get(id)
                .cloned()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            statuses
                .get(folder)
                .is_none_or(|status| !unchanged(&state, status))
        })
        .count())
}

pub async fn sync_folder(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
    channel_id: i64,
    folder: &str,
    now: DateTime<Utc>,
) -> Result<SyncCounts> {
    sync_folder_known(pool, server, source_id, channel_id, folder, None, now).await
}

/// [`sync_folder`], given the folder's status when the server already said
/// it ([`MailServer::statuses`]): unchanged since the last sync, it isn't
/// opened at all.
async fn sync_folder_known(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
    channel_id: i64,
    folder: &str,
    known: Option<&FolderStatus>,
    now: DateTime<Utc>,
) -> Result<SyncCounts> {
    let mut counts = SyncCounts::default();
    let (state, muted, cursor): (Option<String>, bool, Option<String>) =
        sqlx::query_as("SELECT mail_state, muted, cursor FROM channels WHERE id = ?")
            .bind(channel_id)
            .fetch_one(pool)
            .await?;
    let mut state: MailState = state
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if let Some(status) = known
        && unchanged(&state, status)
        && !missing_anything(pool, channel_id).await?
    {
        counts.unchanged = 1;
        return Ok(counts);
    }
    let status = server.open(folder).await?;
    let empty = status.exists == 0;

    if state.uidvalidity != Some(status.uidvalidity) {
        // The server renumbered the folder: forget the old UIDs; the rows are
        // matched again by Message-ID below.
        sqlx::query("UPDATE messages SET uid = NULL, uidvalidity = NULL WHERE channel_id = ?")
            .bind(channel_id)
            .execute(pool)
            .await?;
        state = MailState {
            uidvalidity: Some(status.uidvalidity),
            ..Default::default()
        };
    }

    if !empty {
        fill_missing(pool, server, channel_id, status.uidvalidity).await?;
    }

    if unchanged(&state, &status) {
        counts.unchanged = 1;
        return Ok(counts);
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

    // The same UIDNEXT and message count: nothing arrived or left since the
    // last sync, so only flags can have changed, and the folder's UIDs
    // needn't be listed.
    let same_mail = state.uidnext == Some(status.uidnext) && state.exists == Some(status.exists);

    // Mail that left the folder.
    let present: HashSet<u32> = if empty {
        HashSet::new()
    } else if same_mail {
        known.clone()
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
    let in_window = if empty || same_mail {
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
                    mail.received_at = h.received_at;
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
    state.uidnext = Some(status.uidnext);
    state.exists = Some(status.exists);
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
        /// When the server got it; the Date header's time when None.
        pub received: Option<DateTime<Utc>>,
        /// The server's authentication results ("dmarc=fail …").
        pub auth: Option<String>,
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
        /// Fail every store and fetch of whole messages, as when offline.
        pub offline: bool,
        /// UIDs of every whole message fetched.
        pub fetched: Vec<u32>,
        /// UID searches run.
        pub searches: usize,
        /// Folders opened.
        pub opens: usize,
        /// Answers every folder's status at once (LIST-STATUS).
        pub list_status: bool,
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
                    received: None,
                    auth: None,
                    flags: 0,
                    modseq: 1,
                },
            );
        }

        fn status_of(&self, folder: &str) -> Result<FolderStatus> {
            let f = self.folders.get(folder).context("no such folder")?;
            Ok(FolderStatus {
                exists: f.messages.len() as u32,
                uidvalidity: f.uidvalidity,
                uidnext: f.messages.keys().last().map_or(1, |u| u + 1),
                highest_modseq: self
                    .condstore
                    .then(|| f.messages.values().map(|m| m.modseq).max().unwrap_or(1)),
            })
        }

        fn current(&self) -> &FakeFolder {
            &self.folders[&self.open]
        }
    }

    pub(crate) fn header(m: &FakeMessage) -> Vec<u8> {
        format!(
            "{}Message-ID: <{}>\r\nSubject: {}\r\nFrom: Ana <ana@example.com>\r\nDate: {}\r\n\r\n",
            m.auth
                .as_deref()
                .map(|a| format!("Authentication-Results: mx.test; {a}\r\n"))
                .unwrap_or_default(),
            m.id,
            m.subject,
            m.date.to_rfc2822()
        )
        .into_bytes()
    }

    #[async_trait]
    impl MailServer for FakeServer {
        async fn open(&mut self, folder: &str) -> Result<FolderStatus> {
            let status = self.status_of(folder)?;
            self.opens += 1;
            self.open = folder.to_string();
            Ok(status)
        }

        async fn statuses(&mut self) -> Result<HashMap<String, FolderStatus>> {
            if !(self.list_status && self.condstore) {
                return Ok(HashMap::new());
            }
            let names: Vec<String> = self.folders.keys().cloned().collect();
            names
                .into_iter()
                .map(|n| Ok((n.clone(), self.status_of(&n)?)))
                .collect()
        }

        async fn uids(&mut self, since: Option<DateTime<Utc>>) -> Result<Vec<u32>> {
            self.searches += 1;
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
                        received_at: Some(m.received.unwrap_or(m.date)),
                    })
                })
                .collect())
        }

        async fn messages(&mut self, uids: &[u32]) -> Result<Vec<FetchedMessage>> {
            anyhow::ensure!(!self.offline, "offline");
            self.fetched.extend(uids);
            let f = self.current();
            Ok(uids
                .iter()
                .filter_map(|u| {
                    let m = f.messages.get(u)?;
                    let mut raw = header(m);
                    raw.extend(format!("The body of {}.\r\n", m.id).into_bytes());
                    Some(FetchedMessage {
                        uid: *u,
                        flags: m.flags,
                        raw,
                        received_at: Some(m.received.unwrap_or(m.date)),
                    })
                })
                .collect())
        }

        async fn search(&mut self, c: &crate::mail::search::Criteria) -> Result<Vec<u32>> {
            self.searches += 1;
            let has = |hay: &str, needle: &Option<String>| {
                needle
                    .as_ref()
                    .is_none_or(|n| hay.to_lowercase().contains(&n.to_lowercase()))
            };
            Ok(self
                .current()
                .messages
                .iter()
                .filter(|(_, m)| {
                    let from = "Ana <ana@example.com>";
                    let text = format!("{} The body of {}.", m.subject, m.id);
                    has(from, &c.from)
                        && has(&m.subject, &c.subject)
                        && has(&text, &c.text)
                        && c.since.is_none_or(|d| m.date.date_naive() >= d)
                        && c.before.is_none_or(|d| m.date.date_naive() < d)
                })
                .map(|(u, _)| *u)
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

        async fn auth_headers(&mut self, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>> {
            anyhow::ensure!(!self.offline, "offline");
            Ok(uids
                .iter()
                .filter_map(|u| Some((*u, header(self.current().messages.get(u)?))))
                .collect())
        }

        async fn received(&mut self, uids: &[u32]) -> Result<Vec<(u32, DateTime<Utc>)>> {
            anyhow::ensure!(!self.offline, "offline");
            Ok(uids
                .iter()
                .filter_map(|u| {
                    let m = self.current().messages.get(u)?;
                    Some((*u, m.received.unwrap_or(m.date)))
                })
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
    async fn stores_when_mail_arrived_and_fills_it_in_for_older_rows() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        folder(&pool, source_id, "Archive", true).await?;
        // With change tracking, so the second sync finds the folder unchanged.
        let mut server = FakeServer {
            condstore: true,
            ..Default::default()
        };
        server.add("Archive", 1, "forged@x", "You won", -120);
        server
            .folder("Archive")
            .messages
            .get_mut(&1)
            .unwrap()
            .received = Some(Utc::now() - Duration::days(1));
        server.add("Archive", 2, "fine@x", "Lunch", 3);
        let misdated = |pool: SqlitePool| async move {
            let rows: Vec<(String, i64, Option<i64>)> = sqlx::query_as(
                "SELECT external_id, posted_at, received_at FROM messages ORDER BY external_id",
            )
            .fetch_all(&pool)
            .await?;
            anyhow::Ok(
                rows.into_iter()
                    .map(|(id, posted, received)| (id, crate::mail::misdated(posted, received)))
                    .collect::<Vec<_>>(),
            )
        };
        let expected = vec![
            ("fine@x".to_string(), None),
            (
                "forged@x".to_string(),
                Some("dated 4 months after it arrived".to_string()),
            ),
        ];

        sync_folders(&pool, &mut server, source_id).await?;
        assert_eq!(misdated(pool.clone()).await?, expected);

        // Rows stored before arrival times were kept get them on the next sync.
        sqlx::query("UPDATE messages SET received_at = NULL")
            .execute(&pool)
            .await?;
        let second = sync_folders(&pool, &mut server, source_id).await?;
        assert_eq!(second.unchanged, 1);
        assert_eq!(misdated(pool.clone()).await?, expected);
        Ok(())
    }

    #[tokio::test]
    async fn stores_the_servers_verdict_on_the_sender() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        folder(&pool, source_id, "INBOX", true).await?;
        let mut server = FakeServer {
            condstore: true,
            ..Default::default()
        };
        server.add("INBOX", 1, "spoof@x", "Contract", 1);
        server.folder("INBOX").messages.get_mut(&1).unwrap().auth =
            Some("arc=none; dmarc=fail header.from=example.com".into());
        server.add("INBOX", 2, "fine@x", "Lunch", 1);
        let failures = |pool: SqlitePool| async move {
            let rows: Vec<(String, Option<String>)> =
                sqlx::query_as("SELECT external_id, auth_json FROM messages ORDER BY external_id")
                    .fetch_all(&pool)
                    .await?;
            anyhow::Ok(
                rows.into_iter()
                    .map(|(id, auth)| {
                        let auth: crate::mail::suspicion::Auth =
                            serde_json::from_str(&auth.unwrap_or_default()).unwrap_or_default();
                        (id, auth.failure())
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let expected = vec![
            ("fine@x".to_string(), None),
            (
                "spoof@x".to_string(),
                Some("example.com didn't authorize its sender (DMARC failed)".to_string()),
            ),
        ];

        sync_folders(&pool, &mut server, source_id).await?;
        assert_eq!(failures(pool.clone()).await?, expected);

        // Rows stored before verdicts were kept get them on the next sync.
        sqlx::query("UPDATE messages SET auth_json = NULL")
            .execute(&pool)
            .await?;
        sync_folders(&pool, &mut server, source_id).await?;
        assert_eq!(failures(pool.clone()).await?, expected);
        Ok(())
    }

    #[tokio::test]
    async fn skips_folders_the_server_shows_unchanged_and_opens_them_otherwise() -> Result<()> {
        for list_status in [true, false] {
            let (_tmp, pool, source_id) = setup().await?;
            folder(&pool, source_id, "INBOX", true).await?;
            folder(&pool, source_id, "Archive", true).await?;
            let mut server = FakeServer {
                condstore: true,
                list_status,
                ..Default::default()
            };
            server.add("INBOX", 1, "a@x", "Lunch", 1);
            server.add("Archive", 1, "b@x", "Dinner", 2);
            sync_folders(&pool, &mut server, source_id).await?;

            server.opens = 0;
            let idle = sync_folders(&pool, &mut server, source_id).await?;
            assert_eq!(idle.unchanged, 2);
            assert_eq!(server.opens, if list_status { 0 } else { 2 });

            // New mail in one folder: only that one is opened.
            server.add("INBOX", 2, "c@x", "Tea", 0);
            server.opens = 0;
            let next = sync_folders(&pool, &mut server, source_id).await?;
            assert_eq!(next.new, 1);
            assert_eq!(server.opens, if list_status { 1 } else { 2 });
        }
        Ok(())
    }

    /// One of several sessions onto the same [`FakeServer`].
    struct Session {
        server: Arc<tokio::sync::Mutex<FakeServer>>,
        open: String,
        /// Sessions opened besides the first.
        others: Arc<Mutex<usize>>,
    }

    impl Session {
        async fn server(&self) -> tokio::sync::MutexGuard<'_, FakeServer> {
            let mut server = self.server.lock().await;
            server.open = self.open.clone();
            server
        }
    }

    #[async_trait]
    impl MailServer for Session {
        async fn others(&mut self, count: usize) -> Vec<Box<dyn MailServer>> {
            *self.others.lock().unwrap() += count;
            (0..count)
                .map(|_| {
                    Box::new(Session {
                        server: self.server.clone(),
                        open: String::new(),
                        others: self.others.clone(),
                    }) as Box<dyn MailServer>
                })
                .collect()
        }
        async fn open(&mut self, folder: &str) -> Result<FolderStatus> {
            self.open = folder.to_string();
            self.server().await.open(folder).await
        }
        async fn statuses(&mut self) -> Result<HashMap<String, FolderStatus>> {
            self.server().await.statuses().await
        }
        async fn uids(&mut self, since: Option<DateTime<Utc>>) -> Result<Vec<u32>> {
            self.server().await.uids(since).await
        }
        async fn headers(&mut self, uids: &[u32]) -> Result<Vec<FetchedHeaders>> {
            self.server().await.headers(uids).await
        }
        async fn messages(&mut self, uids: &[u32]) -> Result<Vec<FetchedMessage>> {
            self.server().await.messages(uids).await
        }
        async fn flags(&mut self, changed_since: Option<u64>) -> Result<Vec<(u32, u32)>> {
            self.server().await.flags(changed_since).await
        }
        async fn auth_headers(&mut self, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>> {
            self.server().await.auth_headers(uids).await
        }
        async fn received(&mut self, uids: &[u32]) -> Result<Vec<(u32, DateTime<Utc>)>> {
            self.server().await.received(uids).await
        }
        async fn store_seen(&mut self, uids: &[u32], seen: bool) -> Result<()> {
            self.server().await.store_seen(uids, seen).await
        }
        async fn search(&mut self, c: &crate::mail::search::Criteria) -> Result<Vec<u32>> {
            self.server().await.search(c).await
        }
    }

    #[tokio::test]
    async fn shares_many_folders_among_more_sessions() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        let mut server = FakeServer {
            condstore: true,
            ..Default::default()
        };
        for n in 0..13 {
            let name = format!("Folder {n}");
            folder(&pool, source_id, &name, true).await?;
            server.add(&name, 1, &format!("{n}@x"), "Hello", 1);
        }
        let mut session = Session {
            server: Arc::new(tokio::sync::Mutex::new(server)),
            open: String::new(),
            others: Default::default(),
        };

        // Without LIST-STATUS every folder is opened: two more sessions.
        let first = sync_folders(&pool, &mut session, source_id).await?;
        assert_eq!((first.folders, first.new), (13, 13));
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert_eq!(count(&pool).await?, 13);
        assert_eq!(*session.others.lock().unwrap(), 2);

        // The server shows them all unchanged: no folder to open, no session.
        session.server.lock().await.list_status = true;
        *session.others.lock().unwrap() = 0;
        let idle = sync_folders(&pool, &mut session, source_id).await?;
        assert_eq!(idle.unchanged, 13);
        assert_eq!(*session.others.lock().unwrap(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn reads_only_the_flags_when_no_mail_came_or_went() -> Result<()> {
        for condstore in [true, false] {
            let (_tmp, pool, source_id) = setup().await?;
            folder(&pool, source_id, "INBOX", true).await?;
            let mut server = FakeServer {
                condstore,
                ..Default::default()
            };
            server.add("INBOX", 1, "a@x", "Lunch", 1);
            server.add("INBOX", 2, "b@x", "Dinner", 2);
            sync_folders(&pool, &mut server, source_id).await?;

            // Read elsewhere: the flag arrives without listing the folder.
            let m = server.folder("INBOX").messages.get_mut(&1).unwrap();
            m.flags = 1;
            m.modseq = 5;
            server.searches = 0;
            let next = sync_folders(&pool, &mut server, source_id).await?;
            assert_eq!((next.flag_changes, next.new, next.removed), (1, 0, 0));
            assert_eq!(server.searches, 0);

            // One left and one came: the same count, but listed again.
            server.folder("INBOX").messages.remove(&2);
            server.add("INBOX", 3, "c@x", "Tea", 0);
            let moved = sync_folders(&pool, &mut server, source_id).await?;
            assert_eq!((moved.new, moved.removed), (1, 1));
        }
        Ok(())
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

        // Nothing changed on the server: the folder isn't searched again.
        server.searches = 0;
        let idle = sync_folder(
            &pool,
            &mut server,
            source_id,
            archive,
            "Archive",
            Utc::now(),
        )
        .await?;
        assert_eq!((idle.unchanged, server.searches), (1, 0));

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
