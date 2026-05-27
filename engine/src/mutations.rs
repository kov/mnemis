//! Write-side helpers that change `actions` state in response to user input.
//! Read-only paths live in [`crate::queries`]; this module covers the
//! transitions the UI's claim / done / dismiss buttons trigger and any
//! future user-driven action edits.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use mnemis_types::ActionStatus;
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
