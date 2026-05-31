//! Two-way reconcile between mnemis actions and a [`TaskBackend`].
//!
//! Direction of authority follows the **ownership rule**: mnemis owns only
//! `rationale` + `evidence`, which never leave the database (they aren't part of
//! a VEVENT), so they're preserved automatically; the calendar wins on
//! everything it *can* express — title and due date. A calendar event has no
//! "done" state, so completion only flows mnemis → calendar (as a deletion).
//! Concretely, per tracked action:
//!
//! - **Resolved locally** (`done`/`cancelled`/`dismissed`): delete the event and
//!   drop the calendar linkage (keeping `due_at` + status) — a done item doesn't
//!   belong on the calendar, and clearing the link stops it being re-created.
//! - **Remote changed** (its ETag differs from what we stored): pull the remote
//!   title/due into the action, even if we also had a pending local edit — the
//!   calendar's edit was a deliberate user act, so it wins.
//! - **Only local changed** (`sync_status = 'dirty'`, ETag still matches): push
//!   our edit with `If-Match`. A `412` (someone raced us) parks the action in
//!   `needs_review` rather than looping.
//! - **Tracked but gone from the server**: the user deleted the event →
//!   "unpromote" the action (drop `due_at` + the calendar linkage) so it stays
//!   in mnemis but is no longer a reminder, and isn't immediately re-created.
//! - **New + has a due date + not terminal**: create an all-day VEVENT and record
//!   the `uid`/`href`/`etag`.
//!
//! Remote events with no matching action are left alone — mnemis doesn't import
//! arbitrary calendar events the user created directly (out of scope for v1).

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::Utc;
use mnemis_types::ActionStatus;
use sqlx::{Sqlite, SqlitePool, Transaction};

use super::{Conditional, RemoteTask, TaskBackend, TaskWrite};

/// Tally of what a sync run did, for the status panel / logs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncSummary {
    /// Reminders created on the server for newly-due actions.
    pub created: usize,
    /// Local edits pushed to the server.
    pub pushed: usize,
    /// Remote edits pulled into local actions.
    pub pulled: usize,
    /// Events removed from the calendar — either the action was resolved locally
    /// (linkage cleared, `due_at` kept) or the user deleted it remotely (action
    /// unpromoted).
    pub removed: usize,
    /// Push conflicts parked as `needs_review`.
    pub conflicts: usize,
    /// Per-action errors that didn't abort the whole run.
    pub errors: Vec<String>,
}

struct LocalRow {
    id: i64,
    title: String,
    details: Option<String>,
    due_at: Option<i64>,
    status: ActionStatus,
    uid: Option<String>,
    href: Option<String>,
    etag: Option<String>,
    sync_status: Option<String>,
}

enum Outcome {
    None,
    Created,
    Pushed,
    Pulled,
    Removed,
    Conflict,
}

/// Reconcile every reminder-relevant action against the backend's collection.
/// Per-action failures are collected into [`SyncSummary::errors`]; only a
/// failure to *list* the remote tasks aborts the whole run.
pub async fn sync_caldav(pool: &SqlitePool, backend: &dyn TaskBackend) -> Result<SyncSummary> {
    let remote = backend.list_tasks().await.context("listing remote tasks")?;
    let remote_by_uid: HashMap<&str, &RemoteTask> =
        remote.iter().map(|t| (t.uid.as_str(), t)).collect();
    let local = load_local(pool).await?;

    let mut summary = SyncSummary::default();
    for row in &local {
        match reconcile_one(pool, backend, row, &remote_by_uid).await {
            Ok(Outcome::Created) => summary.created += 1,
            Ok(Outcome::Pushed) => summary.pushed += 1,
            Ok(Outcome::Pulled) => summary.pulled += 1,
            Ok(Outcome::Removed) => summary.removed += 1,
            Ok(Outcome::Conflict) => summary.conflicts += 1,
            Ok(Outcome::None) => {}
            Err(e) => summary.errors.push(format!("action {}: {e:#}", row.id)),
        }
    }
    Ok(summary)
}

async fn reconcile_one(
    pool: &SqlitePool,
    backend: &dyn TaskBackend,
    row: &LocalRow,
    remote_by_uid: &HashMap<&str, &RemoteTask>,
) -> Result<Outcome> {
    match &row.uid {
        Some(uid) => match remote_by_uid.get(uid.as_str()) {
            Some(rt) => {
                let remote_changed = row.etag.as_deref() != Some(rt.etag.as_str());
                let dirty = row.sync_status.as_deref() == Some("dirty");
                if is_terminal(row.status) {
                    // Resolved locally → a calendar event can't be "done", so
                    // remove it. Use the freshly-listed etag for If-Match.
                    let href = row
                        .href
                        .as_deref()
                        .context("terminal action has no external_href")?;
                    match backend.delete_task(href, &rt.etag).await? {
                        Conditional::Ok(()) => {
                            clear_link(pool, row.id).await?;
                            Ok(Outcome::Removed)
                        }
                        Conditional::Conflict => {
                            mark_needs_review(pool, row.id).await?;
                            Ok(Outcome::Conflict)
                        }
                    }
                } else if remote_changed {
                    apply_remote(pool, row, rt).await?;
                    Ok(Outcome::Pulled)
                } else if dirty {
                    let href = row
                        .href
                        .as_deref()
                        .context("dirty action has no external_href")?;
                    let write = task_write(row, uid);
                    match backend.update_task(href, &rt.etag, &write).await? {
                        Conditional::Ok(new_etag) => {
                            mark_pushed(pool, row.id, &new_etag).await?;
                            Ok(Outcome::Pushed)
                        }
                        Conditional::Conflict => {
                            mark_needs_review(pool, row.id).await?;
                            Ok(Outcome::Conflict)
                        }
                    }
                } else {
                    Ok(Outcome::None)
                }
            }
            None => {
                unpromote(pool, row.id).await?;
                Ok(Outcome::Removed)
            }
        },
        None => {
            if row.due_at.is_some() && !is_terminal(row.status) {
                let uid = format!("mnemis-{}@mnemis", row.id);
                let write = task_write(row, &uid);
                let created = backend.create_task(&write).await?;
                link_created(pool, row.id, &uid, &created.href, &created.etag).await?;
                Ok(Outcome::Created)
            } else {
                Ok(Outcome::None)
            }
        }
    }
}

fn is_terminal(status: ActionStatus) -> bool {
    matches!(
        status,
        ActionStatus::Done | ActionStatus::Cancelled | ActionStatus::Dismissed
    )
}

fn task_write(row: &LocalRow, uid: &str) -> TaskWrite {
    TaskWrite {
        uid: uid.to_owned(),
        summary: row.title.clone(),
        description: row.details.clone(),
        due: row.due_at,
    }
}

/// Raw column tuple for a reminder-relevant action row (status still a string).
type RawLocalRow = (
    i64,
    String,
    Option<String>,
    Option<i64>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn load_local(pool: &SqlitePool) -> Result<Vec<LocalRow>> {
    let rows: Vec<RawLocalRow> = sqlx::query_as(
        "SELECT id, title, details, due_at, status, external_calendar_uid, external_href, \
         external_etag, sync_status FROM actions \
         WHERE due_at IS NOT NULL OR external_calendar_uid IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .context("loading reminder-relevant actions")?;

    rows.into_iter()
        .map(
            |(id, title, details, due_at, status, uid, href, etag, sync_status)| {
                let status = ActionStatus::parse(&status)
                    .with_context(|| format!("unknown status {status:?} on action {id}"))?;
                Ok(LocalRow {
                    id,
                    title,
                    details,
                    due_at,
                    status,
                    uid,
                    href,
                    etag,
                    sync_status,
                })
            },
        )
        .collect()
}

/// Calendar-wins: overwrite local title/details/due from the remote event.
/// `rationale`/`evidence` are never touched — they aren't in the VEVENT. There
/// is no status to pull: a calendar event has no "done" state.
async fn apply_remote(pool: &SqlitePool, row: &LocalRow, rt: &RemoteTask) -> Result<()> {
    let now = Utc::now().timestamp();
    let title = if rt.summary.is_empty() {
        row.title.clone()
    } else {
        rt.summary.clone()
    };

    let mut tx = pool.begin().await.context("begin caldav reconcile tx")?;

    sqlx::query(
        "UPDATE actions SET title = ?, details = ?, due_at = ?, external_etag = ?, \
         sync_status = 'synced', sync_error = NULL WHERE id = ?",
    )
    .bind(&title)
    .bind(&rt.description)
    .bind(rt.due)
    .bind(&rt.etag)
    .bind(row.id)
    .execute(&mut *tx)
    .await
    .context("applying remote fields")?;

    insert_event(
        &mut tx,
        row.id,
        "updated",
        serde_json::json!({ "caldav": "pulled" }),
        now,
    )
    .await?;

    tx.commit().await.context("commit caldav reconcile tx")
}

async fn mark_pushed(pool: &SqlitePool, action_id: i64, new_etag: &str) -> Result<()> {
    let now = Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE actions SET external_etag = ?, sync_status = 'synced', sync_error = NULL \
         WHERE id = ?",
    )
    .bind(new_etag)
    .bind(action_id)
    .execute(&mut *tx)
    .await?;
    insert_event(
        &mut tx,
        action_id,
        "updated",
        serde_json::json!({ "caldav": "pushed" }),
        now,
    )
    .await?;
    tx.commit().await.map_err(Into::into)
}

async fn mark_needs_review(pool: &SqlitePool, action_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE actions SET sync_status = 'needs_review', \
         sync_error = 'calendar changed during sync; needs review' WHERE id = ?",
    )
    .bind(action_id)
    .execute(pool)
    .await
    .context("marking action needs_review")?;
    Ok(())
}

/// The user removed the reminder on the calendar. Drop the due date + linkage
/// so it's no longer a reminder (and won't be recreated next run), but keep the
/// action and its status.
async fn unpromote(pool: &SqlitePool, action_id: i64) -> Result<()> {
    let now = Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE actions SET due_at = NULL, external_calendar_uid = NULL, external_href = NULL, \
         external_etag = NULL, sync_status = NULL, sync_error = NULL WHERE id = ?",
    )
    .bind(action_id)
    .execute(&mut *tx)
    .await?;
    insert_event(
        &mut tx,
        action_id,
        "updated",
        serde_json::json!({ "caldav": "removed_reminder" }),
        now,
    )
    .await?;
    tx.commit().await.map_err(Into::into)
}

async fn link_created(
    pool: &SqlitePool,
    action_id: i64,
    uid: &str,
    href: &str,
    etag: &str,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE actions SET external_calendar_uid = ?, external_href = ?, external_etag = ?, \
         sync_status = 'synced', sync_error = NULL WHERE id = ?",
    )
    .bind(uid)
    .bind(href)
    .bind(etag)
    .bind(action_id)
    .execute(&mut *tx)
    .await?;
    insert_event(
        &mut tx,
        action_id,
        "updated",
        serde_json::json!({ "caldav": "created_reminder" }),
        now,
    )
    .await?;
    tx.commit().await.map_err(Into::into)
}

/// The action was resolved locally and its event removed from the calendar.
/// Drop the calendar linkage but keep `due_at` + the resolved status — unlike
/// [`unpromote`], the action stays "done with a date", just no longer synced.
async fn clear_link(pool: &SqlitePool, action_id: i64) -> Result<()> {
    let now = Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE actions SET external_calendar_uid = NULL, external_href = NULL, \
         external_etag = NULL, sync_status = NULL, sync_error = NULL WHERE id = ?",
    )
    .bind(action_id)
    .execute(&mut *tx)
    .await?;
    insert_event(
        &mut tx,
        action_id,
        "updated",
        serde_json::json!({ "caldav": "completed_removed" }),
        now,
    )
    .await?;
    tx.commit().await.map_err(Into::into)
}

async fn insert_event(
    tx: &mut Transaction<'_, Sqlite>,
    action_id: i64,
    event_kind: &str,
    data: serde_json::Value,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO action_events (action_id, event_kind, actor, data_json, occurred_at) \
         VALUES (?, ?, 'caldav_sync', ?, ?)",
    )
    .bind(action_id)
    .bind(event_kind)
    .bind(data.to_string())
    .bind(now)
    .execute(&mut **tx)
    .await
    .context("inserting caldav_sync action_event")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use tempfile::TempDir;

    use crate::sync::Created;

    /// In-memory backend: preload `tasks` to stand in for server state; records
    /// creates/updates so assertions can inspect what we pushed.
    #[derive(Default)]
    struct MockBackend {
        tasks: Mutex<HashMap<String, RemoteTask>>,
        created: Mutex<Vec<TaskWrite>>,
        updated: Mutex<Vec<TaskWrite>>,
        deleted: Mutex<Vec<String>>,
        force_update_conflict: bool,
        next_etag: Mutex<u64>,
    }

    impl MockBackend {
        fn with_task(self, task: RemoteTask) -> Self {
            self.tasks.lock().unwrap().insert(task.uid.clone(), task);
            self
        }
        fn next_etag(&self) -> String {
            let mut n = self.next_etag.lock().unwrap();
            *n += 1;
            format!("\"srv-{n}\"")
        }
    }

    #[async_trait]
    impl TaskBackend for MockBackend {
        async fn list_tasks(&self) -> Result<Vec<RemoteTask>> {
            Ok(self.tasks.lock().unwrap().values().cloned().collect())
        }
        async fn create_task(&self, task: &TaskWrite) -> Result<Created> {
            self.created.lock().unwrap().push(task.clone());
            let etag = self.next_etag();
            let href = format!("https://srv/cal/{}.ics", task.uid);
            self.tasks.lock().unwrap().insert(
                task.uid.clone(),
                RemoteTask {
                    uid: task.uid.clone(),
                    href: href.clone(),
                    etag: etag.clone(),
                    summary: task.summary.clone(),
                    description: task.description.clone(),
                    due: task.due,
                },
            );
            Ok(Created { href, etag })
        }
        async fn update_task(
            &self,
            _href: &str,
            _etag: &str,
            task: &TaskWrite,
        ) -> Result<Conditional<String>> {
            self.updated.lock().unwrap().push(task.clone());
            if self.force_update_conflict {
                return Ok(Conditional::Conflict);
            }
            let etag = self.next_etag();
            if let Some(t) = self.tasks.lock().unwrap().get_mut(&task.uid) {
                t.summary = task.summary.clone();
                t.due = task.due;
                t.etag = etag.clone();
            }
            Ok(Conditional::Ok(etag))
        }
        async fn delete_task(&self, href: &str, _etag: &str) -> Result<Conditional<()>> {
            self.deleted.lock().unwrap().push(href.to_owned());
            self.tasks.lock().unwrap().retain(|_, t| t.href != href);
            Ok(Conditional::Ok(()))
        }
    }

    async fn pool() -> Result<(TempDir, SqlitePool)> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        Ok((tmp, pool))
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_action(
        pool: &SqlitePool,
        title: &str,
        rationale: &str,
        status: &str,
        due_at: Option<i64>,
        uid: Option<&str>,
        href: Option<&str>,
        etag: Option<&str>,
        sync_status: Option<&str>,
    ) -> Result<i64> {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO actions (title, confidence, rationale, status, due_at, \
             external_calendar_uid, external_href, external_etag, sync_status, extracted_at) \
             VALUES (?, 'high', ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(title)
        .bind(rationale)
        .bind(status)
        .bind(due_at)
        .bind(uid)
        .bind(href)
        .bind(etag)
        .bind(sync_status)
        .bind(1_700_000_000_i64)
        .fetch_one(pool)
        .await?;
        Ok(id)
    }

    fn remote(uid: &str, summary: &str, etag: &str, due: Option<i64>) -> RemoteTask {
        RemoteTask {
            uid: uid.into(),
            href: format!("https://srv/cal/{uid}.ics"),
            etag: etag.into(),
            summary: summary.into(),
            description: None,
            due,
        }
    }

    #[tokio::test]
    async fn pull_applies_calendar_edit_and_preserves_rationale() -> Result<()> {
        let (_tmp, pool) = pool().await?;
        let id = insert_action(
            &pool,
            "old title",
            "because Ana asked",
            "pending",
            Some(100),
            Some("mnemis-1@mnemis"),
            Some("https://srv/cal/mnemis-1@mnemis.ics"),
            Some("\"e1\""),
            Some("synced"),
        )
        .await?;
        let backend = MockBackend::default().with_task(remote(
            "mnemis-1@mnemis",
            "new title",
            "\"e2\"",
            Some(200),
        ));

        let summary = sync_caldav(&pool, &backend).await?;
        assert_eq!(summary.pulled, 1);

        let (title, due, etag, sync_status, rationale): (
            String,
            Option<i64>,
            String,
            String,
            String,
        ) = sqlx::query_as(
            "SELECT title, due_at, external_etag, sync_status, rationale FROM actions WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(title, "new title");
        assert_eq!(due, Some(200));
        assert_eq!(etag, "\"e2\"");
        assert_eq!(sync_status, "synced");
        assert_eq!(
            rationale, "because Ana asked",
            "rationale must be preserved"
        );
        Ok(())
    }

    #[tokio::test]
    async fn done_action_deletes_remote_event() -> Result<()> {
        // A calendar event has no "done" state, so resolving an action locally
        // removes its event and drops the linkage — but keeps due_at + status.
        let (_tmp, pool) = pool().await?;
        let id = insert_action(
            &pool,
            "task",
            "because Ana asked",
            "done",
            Some(100),
            Some("mnemis-1@mnemis"),
            Some("https://srv/cal/mnemis-1@mnemis.ics"),
            Some("\"e1\""),
            Some("dirty"),
        )
        .await?;
        let backend = MockBackend::default().with_task(remote(
            "mnemis-1@mnemis",
            "task",
            "\"e1\"",
            Some(100),
        ));

        let summary = sync_caldav(&pool, &backend).await?;
        assert_eq!(summary.removed, 1);
        assert_eq!(
            backend.deleted.lock().unwrap().as_slice(),
            ["https://srv/cal/mnemis-1@mnemis.ics"]
        );
        assert!(
            backend.tasks.lock().unwrap().is_empty(),
            "the event should be gone from the server"
        );

        let (status, due, uid, sync_status, rationale): (
            String,
            Option<i64>,
            Option<String>,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT status, due_at, external_calendar_uid, sync_status, rationale \
             FROM actions WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(status, "done", "status is unchanged");
        assert_eq!(due, Some(100), "due_at is kept");
        assert_eq!(uid, None, "calendar linkage is cleared");
        assert_eq!(sync_status, None);
        assert_eq!(rationale, "because Ana asked", "rationale preserved");

        let (kind,): (String,) = sqlx::query_as(
            "SELECT event_kind FROM action_events WHERE action_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(kind, "updated");
        Ok(())
    }

    #[tokio::test]
    async fn new_due_action_is_created_remotely() -> Result<()> {
        let (_tmp, pool) = pool().await?;
        let id = insert_action(
            &pool,
            "Call plumber",
            "r",
            "pending",
            Some(500),
            None,
            None,
            None,
            None,
        )
        .await?;
        let backend = MockBackend::default();

        let summary = sync_caldav(&pool, &backend).await?;
        assert_eq!(summary.created, 1);
        assert_eq!(backend.created.lock().unwrap().len(), 1);
        assert_eq!(backend.created.lock().unwrap()[0].summary, "Call plumber");

        let (uid, sync_status): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT external_calendar_uid, sync_status FROM actions WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(uid, Some(format!("mnemis-{id}@mnemis")));
        assert_eq!(sync_status.as_deref(), Some("synced"));
        Ok(())
    }

    #[tokio::test]
    async fn terminal_due_action_is_not_created() -> Result<()> {
        let (_tmp, pool) = pool().await?;
        let _id = insert_action(
            &pool,
            "done thing",
            "r",
            "done",
            Some(500),
            None,
            None,
            None,
            None,
        )
        .await?;
        let backend = MockBackend::default();

        let summary = sync_caldav(&pool, &backend).await?;
        assert_eq!(summary.created, 0);
        assert!(backend.created.lock().unwrap().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn dirty_local_edit_is_pushed() -> Result<()> {
        let (_tmp, pool) = pool().await?;
        let id = insert_action(
            &pool,
            "local new title",
            "r",
            "pending",
            Some(100),
            Some("mnemis-1@mnemis"),
            Some("https://srv/cal/mnemis-1@mnemis.ics"),
            Some("\"e1\""),
            Some("dirty"),
        )
        .await?;
        // Server etag matches stored → not remote-changed; we push.
        let backend = MockBackend::default().with_task(remote(
            "mnemis-1@mnemis",
            "old title",
            "\"e1\"",
            Some(100),
        ));

        let summary = sync_caldav(&pool, &backend).await?;
        assert_eq!(summary.pushed, 1);
        assert_eq!(
            backend.updated.lock().unwrap()[0].summary,
            "local new title"
        );

        let (sync_status, etag): (String, String) =
            sqlx::query_as("SELECT sync_status, external_etag FROM actions WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(sync_status, "synced");
        assert_ne!(etag, "\"e1\"", "etag should advance after push");
        Ok(())
    }

    #[tokio::test]
    async fn push_conflict_marks_needs_review() -> Result<()> {
        let (_tmp, pool) = pool().await?;
        let id = insert_action(
            &pool,
            "local new",
            "r",
            "pending",
            Some(100),
            Some("mnemis-1@mnemis"),
            Some("https://srv/cal/mnemis-1@mnemis.ics"),
            Some("\"e1\""),
            Some("dirty"),
        )
        .await?;
        let mut backend =
            MockBackend::default().with_task(remote("mnemis-1@mnemis", "old", "\"e1\"", Some(100)));
        backend.force_update_conflict = true;

        let summary = sync_caldav(&pool, &backend).await?;
        assert_eq!(summary.conflicts, 1);

        let (sync_status, sync_error): (String, Option<String>) =
            sqlx::query_as("SELECT sync_status, sync_error FROM actions WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(sync_status, "needs_review");
        assert!(sync_error.unwrap().contains("needs review"));
        Ok(())
    }

    #[tokio::test]
    async fn remote_deletion_unpromotes_the_action() -> Result<()> {
        let (_tmp, pool) = pool().await?;
        let id = insert_action(
            &pool,
            "was a reminder",
            "r",
            "claimed",
            Some(100),
            Some("mnemis-1@mnemis"),
            Some("h"),
            Some("\"e1\""),
            Some("synced"),
        )
        .await?;
        // Server has no such task.
        let backend = MockBackend::default();

        let summary = sync_caldav(&pool, &backend).await?;
        assert_eq!(summary.removed, 1);

        let (due, uid, sync_status, status): (Option<i64>, Option<String>, Option<String>, String) =
            sqlx::query_as(
                "SELECT due_at, external_calendar_uid, sync_status, status FROM actions WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(due, None);
        assert_eq!(uid, None);
        assert_eq!(sync_status, None);
        assert_eq!(status, "claimed", "status is left intact when un-promoting");
        Ok(())
    }
}
