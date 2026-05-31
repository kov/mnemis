//! Two-way sync of mnemis actions to an external calendar backend.
//!
//! mnemis actions are to-dos with a due date. iCloud dropped CalDAV support for
//! reminders (VTODO) in the iOS 13 / Catalina upgrade — upgraded accounts silo
//! them where only EventKit can reach (see the `icloud-reminders-no-caldav`
//! memory) — so a due action is synced as an **all-day calendar event (VEVENT)**
//! with a morning-of alarm, which *does* round-trip over CalDAV to the iPhone
//! Calendar app. The CalDAV/VEVENT implementation lives in [`caldav`]; everything
//! above it (reconcile, the action↔event mapping) is written against the
//! provider-agnostic [`TaskBackend`] trait so another backend (a native EventKit
//! one on macOS, or Nextcloud Tasks) can slot in without touching reconcile.
//!
//! Design invariants carried from the v2 design notes:
//! - **Ownership rule:** mnemis owns only `rationale` + `evidence`, which never
//!   leave the database (they aren't part of a VEVENT); the calendar wins on
//!   what it *can* express — title and due date.
//! - **Completion is removal:** a calendar event has no "done" state, so when an
//!   action becomes terminal mnemis deletes the event rather than marking it.
//! - **Optimistic concurrency:** every conditional write carries the last-seen
//!   ETag; a mismatch surfaces as [`Conditional::Conflict`] rather than
//!   clobbering the server. Repeated conflicts park the action in
//!   `sync_status = 'needs_review'` instead of retrying forever.
//! - **Push on extract, pull on sync.**

use anyhow::Result;
use async_trait::async_trait;

pub mod caldav;
pub mod reconcile;
pub mod vevent;

/// An event as it currently exists on the remote backend: identity (`uid`),
/// location (`href`), concurrency token (`etag`), plus the fields we reconcile.
/// A calendar event carries no lifecycle status, so there is none here — an
/// action's completion is expressed by deleting the event.
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
}

/// The fields mnemis pushes when creating or updating an event. No `href`/`etag`:
/// the backend assigns the href on create and returns the new etag; the etag for
/// an update is passed separately so the conflict path is explicit.
#[derive(Debug, Clone)]
pub struct TaskWrite {
    pub uid: String,
    pub summary: String,
    pub description: Option<String>,
    pub due: Option<i64>,
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

/// A provider of remote calendar events. The CalDAV implementation is constructed
/// already bound to a discovered calendar, so the trait itself is collection-scoped
/// and free of discovery concerns.
#[async_trait]
pub trait TaskBackend: Send + Sync {
    /// Every event in the configured calendar, each with its current etag.
    async fn list_tasks(&self) -> Result<Vec<RemoteTask>>;

    /// Create a new event; the backend assigns the resource href.
    async fn create_task(&self, task: &TaskWrite) -> Result<Created>;

    /// Replace an existing event iff `etag` still matches (sent as `If-Match`).
    async fn update_task(
        &self,
        href: &str,
        etag: &str,
        task: &TaskWrite,
    ) -> Result<Conditional<String>>;

    /// Delete an event iff `etag` still matches (sent as `If-Match`).
    async fn delete_task(&self, href: &str, etag: &str) -> Result<Conditional<()>>;
}
