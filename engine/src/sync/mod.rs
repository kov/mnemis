//! Two-way sync of mnemis actions to an external task backend.
//!
//! mnemis actions are to-dos with a lifecycle (`pending → claimed → done`) and
//! an optional due date — which maps cleanly onto an iCalendar **VTODO** task,
//! *not* a calendar VEVENT. The CalDAV/VTODO implementation lives in
//! [`caldav`]; everything above it (reconcile, the action↔task mapping) is
//! written against the provider-agnostic [`TaskBackend`] trait so a future
//! Google-Tasks adapter (Google's CalDAV can't do VTODO) can slot in without
//! touching the reconcile logic.
//!
//! Design invariants carried from the v2 design notes:
//! - **Ownership rule:** mnemis owns only `rationale` + `evidence`; the calendar
//!   side wins on everything else (title, due, status) if it changed there.
//! - **Optimistic concurrency:** every conditional write carries the last-seen
//!   ETag; a mismatch surfaces as [`Conditional::Conflict`] rather than
//!   clobbering the server. Repeated conflicts park the action in
//!   `sync_status = 'needs_review'` instead of retrying forever.
//! - **Push on extract, pull on sync.**

use anyhow::Result;
use async_trait::async_trait;
use mnemis_types::ActionStatus;

pub mod caldav;
pub mod reconcile;
pub mod vtodo;

/// Lifecycle state of a remote task, abstracted over the backend. For CalDAV
/// this is the VTODO `STATUS` property (mapped in [`vtodo`]); the mnemis-side
/// `ActionStatus` mapping lives here so it stays backend-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Not started — VTODO `NEEDS-ACTION`.
    NeedsAction,
    /// Started but not finished — VTODO `IN-PROCESS`.
    InProcess,
    /// Finished — VTODO `COMPLETED`.
    Completed,
    /// Abandoned — VTODO `CANCELLED`.
    Cancelled,
}

impl TaskStatus {
    /// Map a mnemis action's status to the remote lifecycle when **pushing**.
    /// `auto_claimed` reads as not-yet-started to the user (the agent claimed
    /// it, not the user) so it goes to `NeedsAction`; `dismissed` is a soft
    /// "won't do" so it cancels rather than completes.
    pub fn from_action_status(status: ActionStatus) -> Self {
        match status {
            ActionStatus::Pending | ActionStatus::AutoClaimed => Self::NeedsAction,
            ActionStatus::Claimed => Self::InProcess,
            ActionStatus::Done => Self::Completed,
            ActionStatus::Cancelled | ActionStatus::Dismissed => Self::Cancelled,
        }
    }

    /// The canonical mnemis status for a remote lifecycle state when **pulling**
    /// a *user-made* change. This is intentionally many-to-one's inverse: the
    /// remote has only four states, so `auto_claimed`/`dismissed` collapse to
    /// `claimed`/`cancelled` here. Reconcile must therefore only apply this when
    /// the remote status differs from what *we last pushed*
    /// ([`from_action_status`] of the current row) — otherwise a clean
    /// round-trip would wrongly demote `auto_claimed` to `pending`.
    ///
    /// [`from_action_status`]: TaskStatus::from_action_status
    pub fn to_action_status(self) -> ActionStatus {
        match self {
            Self::NeedsAction => ActionStatus::Pending,
            Self::InProcess => ActionStatus::Claimed,
            Self::Completed => ActionStatus::Done,
            Self::Cancelled => ActionStatus::Cancelled,
        }
    }
}

/// A task as it currently exists on the remote backend: identity (`uid`),
/// location (`href`), concurrency token (`etag`), plus the fields we reconcile.
#[derive(Debug, Clone)]
pub struct RemoteTask {
    /// iCalendar `UID` — stable across edits, matches `actions.external_calendar_uid`.
    pub uid: String,
    /// Resource path within the collection, used for conditional PUT/DELETE.
    pub href: String,
    /// Server ETag at the time it was listed.
    pub etag: String,
    pub summary: String,
    pub description: Option<String>,
    /// Due date as unix seconds, mirroring `actions.due_at`.
    pub due: Option<i64>,
    pub status: TaskStatus,
}

/// The fields mnemis pushes when creating or updating a task. No `href`/`etag`:
/// the backend assigns the href on create and returns the new etag; the etag
/// for an update is passed separately so the conflict path is explicit.
#[derive(Debug, Clone)]
pub struct TaskWrite {
    pub uid: String,
    pub summary: String,
    pub description: Option<String>,
    pub due: Option<i64>,
    pub status: TaskStatus,
}

/// Where a freshly-created resource landed, plus its initial etag.
#[derive(Debug, Clone)]
pub struct Created {
    pub href: String,
    pub etag: String,
}

/// Outcome of a conditional (If-Match) write. `Ok` carries the post-write state
/// (a new etag for updates, `()` for deletes); `Conflict` means the server's
/// etag no longer matched — the caller should pull, reconcile, and possibly
/// retry rather than overwrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conditional<T> {
    Ok(T),
    Conflict,
}

/// A provider of remote tasks. The CalDAV implementation is constructed already
/// bound to a discovered task collection, so the trait itself is collection-scoped
/// and free of discovery concerns.
#[async_trait]
pub trait TaskBackend: Send + Sync {
    /// Every task in the configured collection, each with its current etag.
    async fn list_tasks(&self) -> Result<Vec<RemoteTask>>;

    /// Create a new task; the backend assigns the resource href.
    async fn create_task(&self, task: &TaskWrite) -> Result<Created>;

    /// Replace an existing task iff `etag` still matches (sent as `If-Match`).
    async fn update_task(
        &self,
        href: &str,
        etag: &str,
        task: &TaskWrite,
    ) -> Result<Conditional<String>>;

    /// Delete a task iff `etag` still matches (sent as `If-Match`).
    async fn delete_task(&self, href: &str, etag: &str) -> Result<Conditional<()>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_status_mapping_is_stable_under_round_trip() {
        // The reconcile invariant: re-deriving the remote status from the
        // canonical pulled status must equal what we'd push for the original.
        // This is what lets reconcile use a status-equality check to tell a
        // genuine remote edit apart from a no-op round-trip without demoting
        // (e.g.) auto_claimed → pending.
        for status in [
            ActionStatus::Pending,
            ActionStatus::AutoClaimed,
            ActionStatus::Claimed,
            ActionStatus::Done,
            ActionStatus::Cancelled,
            ActionStatus::Dismissed,
        ] {
            let pushed = TaskStatus::from_action_status(status);
            let canonical = pushed.to_action_status();
            assert_eq!(
                pushed,
                TaskStatus::from_action_status(canonical),
                "status {status:?} is not stable across a no-op round-trip"
            );
        }
    }

    #[test]
    fn user_completion_on_the_calendar_is_detectable() {
        // A pending action pushed as NeedsAction; the user ticks it done on the
        // calendar → Completed. That must map to a different mnemis status than
        // what we pushed, so reconcile recognises the change.
        let pushed = TaskStatus::from_action_status(ActionStatus::Pending);
        assert_ne!(TaskStatus::Completed, pushed);
        assert_eq!(TaskStatus::Completed.to_action_status(), ActionStatus::Done);
    }
}
