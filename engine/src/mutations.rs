//! Write-side helpers that change `actions` state in response to user input.
//! Read-only paths live in [`crate::queries`]; this module covers the
//! transitions the UI's claim / done / dismiss buttons trigger and any
//! future user-driven action edits.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use mnemis_types::{ActionStatus, FeedbackKind};
use sqlx::SqlitePool;

const VALID_TARGETS: &[ActionStatus] = &[
    ActionStatus::Claimed,
    ActionStatus::Done,
    ActionStatus::Cancelled,
    ActionStatus::Dismissed,
    ActionStatus::Pending,
];

/// Apply a user-driven status change to an action. Rejects nonsensical
/// transitions, updates `actions.{status,claimed_at,resolved_at,
/// dismissed_reason}` as appropriate, and inserts an `action_events` row
/// describing the change.
///
/// `dismissed_reason` is recorded only when transitioning to
/// `Dismissed`. The 3.2 dismissal-feedback dialog will populate it; for
/// now an empty string is fine.
pub async fn update_action_status(
    pool: &SqlitePool,
    action_id: i64,
    new_status: ActionStatus,
    dismissed_reason: Option<String>,
) -> Result<()> {
    if !VALID_TARGETS.contains(&new_status) {
        bail!("status {:?} is not user-settable", new_status);
    }

    let current_status: (String,) = sqlx::query_as("SELECT status FROM actions WHERE id = ?")
        .bind(action_id)
        .fetch_optional(pool)
        .await
        .context("loading current action status")?
        .ok_or_else(|| anyhow::anyhow!("action {action_id} not found"))?;
    let current = ActionStatus::parse(&current_status.0)
        .ok_or_else(|| anyhow::anyhow!("unknown stored status {:?}", current_status.0))?;

    if current == new_status {
        return Ok(());
    }

    let event_kind = event_kind_for(current, new_status)?;
    let now = Utc::now().timestamp();

    let mut tx = pool.begin().await.context("begin tx")?;

    // Status + timestamps.
    match new_status {
        ActionStatus::Claimed => {
            sqlx::query(
                "UPDATE actions SET status = 'claimed', claimed_at = ?, resolved_at = NULL, \
                 dismissed_reason = NULL WHERE id = ?",
            )
            .bind(now)
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
        }
        ActionStatus::Done | ActionStatus::Cancelled => {
            sqlx::query(
                "UPDATE actions SET status = ?, resolved_at = ?, dismissed_reason = NULL \
                 WHERE id = ?",
            )
            .bind(status_str(new_status))
            .bind(now)
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
        }
        ActionStatus::Dismissed => {
            sqlx::query(
                "UPDATE actions SET status = 'dismissed', resolved_at = ?, \
                 dismissed_reason = ? WHERE id = ?",
            )
            .bind(now)
            .bind(dismissed_reason.unwrap_or_default())
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
        }
        ActionStatus::Pending => {
            // Reopen: drop timestamps and any prior dismiss reason.
            sqlx::query(
                "UPDATE actions SET status = 'pending', claimed_at = NULL, resolved_at = NULL, \
                 dismissed_reason = NULL WHERE id = ?",
            )
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
        }
        _ => unreachable!("guarded by VALID_TARGETS"),
    }

    sqlx::query(
        "INSERT INTO action_events (action_id, event_kind, actor, data_json, occurred_at) \
         VALUES (?, ?, 'user', ?, ?)",
    )
    .bind(action_id)
    .bind(event_kind)
    .bind(serde_json::json!({ "to": status_str(new_status) }).to_string())
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await.context("commit tx")?;
    Ok(())
}

/// Record user feedback about why an action should not have been
/// surfaced (or should not have been auto-claimed). The extractor reads
/// these rows back as labelled negative examples on subsequent runs.
///
/// Scope is derived from the action's evidence: prefer the primary
/// evidence's channel; fall back to the channel's source; fall back to
/// global. Comment is optional — passing `None` (or an empty string)
/// records no learning signal but still inserts a row so we can audit
/// what the user objected to. Callers that want "skip" semantics should
/// simply not call this function.
pub async fn record_dismissal_feedback(
    pool: &SqlitePool,
    action_id: i64,
    kind: FeedbackKind,
    comment: Option<String>,
) -> Result<()> {
    let example_text: (String, Option<String>) =
        sqlx::query_as("SELECT title, details FROM actions WHERE id = ?")
            .bind(action_id)
            .fetch_optional(pool)
            .await
            .context("loading action for feedback scope")?
            .ok_or_else(|| anyhow::anyhow!("action {action_id} not found"))?;
    let example_text = match example_text {
        (t, Some(d)) if !d.trim().is_empty() => format!("{t}\n\n{d}"),
        (t, _) => t,
    };

    // Resolve scope by walking action_evidence → messages → channels.
    let scope: Option<(i64, i64)> = sqlx::query_as(
        "SELECT m.channel_id, c.source_id
         FROM action_evidence ae
         JOIN messages m ON m.id = ae.message_id
         JOIN channels c ON c.id = m.channel_id
         WHERE ae.action_id = ?
         ORDER BY ae.is_primary DESC, ae.message_id ASC
         LIMIT 1",
    )
    .bind(action_id)
    .fetch_optional(pool)
    .await
    .context("resolving feedback scope")?;

    let (scope_kind, scope_id) = match scope {
        Some((channel_id, _)) => ("channel", Some(channel_id)),
        None => ("global", None),
    };

    let reason = comment.unwrap_or_default();
    let now = Utc::now().timestamp();

    sqlx::query(
        "INSERT INTO dismissal_feedback \
         (scope_kind, scope_id, example_text, reason, created_at, kind) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(scope_kind)
    .bind(scope_id)
    .bind(example_text)
    .bind(reason)
    .bind(now)
    .bind(kind.as_str())
    .execute(pool)
    .await
    .context("inserting dismissal_feedback")?;

    Ok(())
}

/// User explicitly rejected the agent's resolution suggestion for this
/// action. Inserts a `suggestion_dismissed` event so the suggestion stops
/// appearing in [`crate::queries::list_pending_resolutions`]. The action
/// itself is untouched — rejecting a suggestion is not the same as
/// dismissing the action.
pub async fn reject_resolution_suggestion(pool: &SqlitePool, action_id: i64) -> Result<()> {
    let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM actions WHERE id = ?")
        .bind(action_id)
        .fetch_optional(pool)
        .await
        .context("looking up action for reject")?;
    if exists.is_none() {
        anyhow::bail!("action {action_id} not found");
    }
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO action_events (action_id, event_kind, actor, occurred_at) \
         VALUES (?, 'suggestion_dismissed', 'user', ?)",
    )
    .bind(action_id)
    .bind(now)
    .execute(pool)
    .await
    .context("inserting suggestion_dismissed event")?;
    Ok(())
}

fn status_str(s: ActionStatus) -> &'static str {
    match s {
        ActionStatus::Pending => "pending",
        ActionStatus::AutoClaimed => "auto_claimed",
        ActionStatus::Claimed => "claimed",
        ActionStatus::Done => "done",
        ActionStatus::Cancelled => "cancelled",
        ActionStatus::Dismissed => "dismissed",
    }
}

fn event_kind_for(from: ActionStatus, to: ActionStatus) -> Result<&'static str> {
    use ActionStatus::*;
    let kind = match (from, to) {
        // Reopening a resolved action.
        (Done | Cancelled | Dismissed, Pending | Claimed) => "unresolved",
        // Undoing a claim (manual or auto). Distinct from `unresolved` so
        // the audit log + downstream views can tell "rolled back a finished
        // action" apart from "took back a claim that was never resolved".
        (Claimed | AutoClaimed, Pending) => "unclaimed",
        (_, Claimed) => "claimed",
        (_, Done | Cancelled) => "resolved",
        (_, Dismissed) => "dismissed",
        (_, Pending) => "unresolved",
        _ => bail!("no event kind for {:?} → {:?}", from, to),
    };
    Ok(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use tempfile::TempDir;

    async fn seed_action(pool: &SqlitePool, status: &str) -> Result<i64> {
        let now = Utc::now().timestamp();
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO actions (title, confidence, status, extracted_at) \
             VALUES ('t', 'medium', ?, ?) RETURNING id",
        )
        .bind(status)
        .bind(now)
        .fetch_one(pool)
        .await?;
        Ok(id)
    }

    /// Seed a source + channel + message and link it to `action_id` as
    /// primary evidence. Returns the channel_id so tests can assert scope.
    async fn seed_action_with_evidence(
        pool: &SqlitePool,
        title: &str,
        details: Option<&str>,
    ) -> Result<(i64, i64)> {
        let now = Utc::now().timestamp();
        let (source_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 's', 'r', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(pool)
        .await?;
        let (channel_id,): (i64,) = sqlx::query_as(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, 'INBOX', 'INBOX', 'mailbox') RETURNING id",
        )
        .bind(source_id)
        .fetch_one(pool)
        .await?;
        let (message_id,): (i64,) = sqlx::query_as(
            "INSERT INTO messages (channel_id, external_id, posted_at, body, body_format, ingested_at) \
             VALUES (?, 'ext-1', ?, 'b', 'text', ?) RETURNING id",
        )
        .bind(channel_id)
        .bind(now)
        .bind(now)
        .fetch_one(pool)
        .await?;
        let (action_id,): (i64,) = sqlx::query_as(
            "INSERT INTO actions (title, details, confidence, status, extracted_at) \
             VALUES (?, ?, 'medium', 'pending', ?) RETURNING id",
        )
        .bind(title)
        .bind(details)
        .bind(now)
        .fetch_one(pool)
        .await?;
        sqlx::query(
            "INSERT INTO action_evidence (action_id, message_id, kind, is_primary) \
             VALUES (?, ?, 'source', 1)",
        )
        .bind(action_id)
        .bind(message_id)
        .execute(pool)
        .await?;
        Ok((action_id, channel_id))
    }

    #[tokio::test]
    async fn record_dismissal_feedback_scopes_to_evidence_channel() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let (action_id, channel_id) =
            seed_action_with_evidence(&pool, "Pay invoice 42", Some("from billing@x")).await?;

        record_dismissal_feedback(
            &pool,
            action_id,
            FeedbackKind::Dismissed,
            Some("billing emails aren't actionable for me".into()),
        )
        .await?;

        let (scope_kind, scope_id, example_text, reason, kind): (
            String,
            Option<i64>,
            String,
            String,
            String,
        ) = sqlx::query_as(
            "SELECT scope_kind, scope_id, example_text, reason, kind \
             FROM dismissal_feedback",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(scope_kind, "channel");
        assert_eq!(scope_id, Some(channel_id));
        assert!(example_text.contains("Pay invoice 42"));
        assert!(example_text.contains("from billing@x"));
        assert_eq!(reason, "billing emails aren't actionable for me");
        assert_eq!(kind, "dismissed");
        Ok(())
    }

    #[tokio::test]
    async fn record_dismissal_feedback_records_wrong_auto_claim_without_comment() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let (action_id, _channel) =
            seed_action_with_evidence(&pool, "Confirm with Sam", None).await?;

        // No comment — extractor learns nothing from this row but we still
        // record what was undone for auditability.
        record_dismissal_feedback(&pool, action_id, FeedbackKind::WrongAutoClaim, None).await?;

        let (kind, reason): (String, String) =
            sqlx::query_as("SELECT kind, reason FROM dismissal_feedback")
                .fetch_one(&pool)
                .await?;
        assert_eq!(kind, "wrong_auto_claim");
        assert_eq!(reason, "");
        Ok(())
    }

    #[tokio::test]
    async fn record_dismissal_feedback_falls_back_to_global_without_evidence() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let action_id = seed_action(&pool, "pending").await?;

        record_dismissal_feedback(
            &pool,
            action_id,
            FeedbackKind::Dismissed,
            Some("noise".into()),
        )
        .await?;

        let (scope_kind, scope_id): (String, Option<i64>) =
            sqlx::query_as("SELECT scope_kind, scope_id FROM dismissal_feedback")
                .fetch_one(&pool)
                .await?;
        assert_eq!(scope_kind, "global");
        assert!(scope_id.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn pending_to_claimed_records_event_and_sets_timestamp() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let id = seed_action(&pool, "pending").await?;

        update_action_status(&pool, id, ActionStatus::Claimed, None).await?;

        let (status, claimed_at): (String, Option<i64>) =
            sqlx::query_as("SELECT status, claimed_at FROM actions WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(status, "claimed");
        assert!(claimed_at.is_some());

        let (event_kind, actor): (String, String) =
            sqlx::query_as("SELECT event_kind, actor FROM action_events WHERE action_id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(event_kind, "claimed");
        assert_eq!(actor, "user");
        Ok(())
    }

    #[tokio::test]
    async fn auto_claimed_to_done_records_resolved() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let id = seed_action(&pool, "auto_claimed").await?;
        update_action_status(&pool, id, ActionStatus::Done, None).await?;

        let (status, resolved_at): (String, Option<i64>) =
            sqlx::query_as("SELECT status, resolved_at FROM actions WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(status, "done");
        assert!(resolved_at.is_some());

        let (event_kind,): (String,) =
            sqlx::query_as("SELECT event_kind FROM action_events WHERE action_id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(event_kind, "resolved");
        Ok(())
    }

    #[tokio::test]
    async fn dismiss_stores_reason_when_provided() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let id = seed_action(&pool, "pending").await?;
        update_action_status(
            &pool,
            id,
            ActionStatus::Dismissed,
            Some("not actually for me".into()),
        )
        .await?;

        let (status, reason): (String, Option<String>) =
            sqlx::query_as("SELECT status, dismissed_reason FROM actions WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(status, "dismissed");
        assert_eq!(reason.as_deref(), Some("not actually for me"));
        Ok(())
    }

    #[tokio::test]
    async fn reject_resolution_suggestion_inserts_event_and_leaves_status() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let id = seed_action(&pool, "pending").await?;

        reject_resolution_suggestion(&pool, id).await?;

        let (status,): (String,) = sqlx::query_as("SELECT status FROM actions WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "pending", "reject must not touch action status");

        let (kind, actor): (String, String) =
            sqlx::query_as("SELECT event_kind, actor FROM action_events WHERE action_id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(kind, "suggestion_dismissed");
        assert_eq!(actor, "user");
        Ok(())
    }

    #[tokio::test]
    async fn unclaim_emits_unclaimed_event_not_unresolved() -> Result<()> {
        // Undoing a claim is structurally different from rolling back a
        // finished action: the action was never resolved, just claimed. Use
        // a distinct event_kind so the audit log + downstream views can tell
        // them apart (`unclaimed` vs `unresolved`).
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;

        let manual = seed_action(&pool, "claimed").await?;
        update_action_status(&pool, manual, ActionStatus::Pending, None).await?;
        let (kind,): (String,) =
            sqlx::query_as("SELECT event_kind FROM action_events WHERE action_id = ?")
                .bind(manual)
                .fetch_one(&pool)
                .await?;
        assert_eq!(kind, "unclaimed");

        let auto = seed_action(&pool, "auto_claimed").await?;
        update_action_status(&pool, auto, ActionStatus::Pending, None).await?;
        let (kind,): (String,) =
            sqlx::query_as("SELECT event_kind FROM action_events WHERE action_id = ?")
                .bind(auto)
                .fetch_one(&pool)
                .await?;
        assert_eq!(kind, "unclaimed");
        Ok(())
    }

    #[tokio::test]
    async fn reopen_done_action_inserts_unresolved_event() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let id = seed_action(&pool, "done").await?;
        update_action_status(&pool, id, ActionStatus::Claimed, None).await?;

        let (event_kind,): (String,) =
            sqlx::query_as("SELECT event_kind FROM action_events WHERE action_id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(event_kind, "unresolved");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_unknown_action() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let err = update_action_status(&pool, 999, ActionStatus::Claimed, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
        Ok(())
    }
}
