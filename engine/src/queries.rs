//! Read-side query helpers that translate SQLite rows into the wire DTOs
//! defined in `mnemis-types`. Used by Tauri commands and CLI commands.

use anyhow::{Context, Result};
use mnemis_types::{
    ActionDto, ActionStatus, Confidence, MessageDto, PendingResolutionDto, SourceHealth,
    SourceStatus, StatusSnapshot,
};
use sqlx::SqlitePool;

const SNIPPET_LEN: usize = 160;
const DEFAULT_MESSAGE_LIMIT: i64 = 50;
const MAX_MESSAGE_LIMIT: i64 = 200;

#[derive(Debug, Clone, Copy, Default)]
pub struct ActionFilter {
    /// If true, include actions with status `done`/`cancelled`/`dismissed`.
    pub include_resolved: bool,
}

/// Return actions ordered by recency (newest first). One row per action;
/// `evidence_count` is computed inline so the UI can show "N messages" without
/// a second roundtrip.
#[allow(clippy::type_complexity)]
pub async fn list_actions(pool: &SqlitePool, filter: ActionFilter) -> Result<Vec<ActionDto>> {
    let sql = if filter.include_resolved {
        "SELECT a.id, a.title, a.details, a.confidence, a.status, a.extracted_at, a.due_at, \
                COALESCE(c.name, ''), COALESCE(s.name, '') \
         FROM actions a \
         LEFT JOIN action_evidence ae ON ae.action_id = a.id AND ae.is_primary = 1 \
         LEFT JOIN messages m ON m.id = ae.message_id \
         LEFT JOIN channels c ON c.id = m.channel_id \
         LEFT JOIN sources s ON s.id = c.source_id \
         GROUP BY a.id \
         ORDER BY a.extracted_at DESC"
    } else {
        "SELECT a.id, a.title, a.details, a.confidence, a.status, a.extracted_at, a.due_at, \
                COALESCE(c.name, ''), COALESCE(s.name, '') \
         FROM actions a \
         LEFT JOIN action_evidence ae ON ae.action_id = a.id AND ae.is_primary = 1 \
         LEFT JOIN messages m ON m.id = ae.message_id \
         LEFT JOIN channels c ON c.id = m.channel_id \
         LEFT JOIN sources s ON s.id = c.source_id \
         WHERE a.status IN ('pending', 'auto_claimed', 'claimed') \
         GROUP BY a.id \
         ORDER BY a.extracted_at DESC"
    };

    let rows: Vec<(
        i64,
        String,
        Option<String>,
        String,
        String,
        i64,
        Option<i64>,
        String,
        String,
    )> = sqlx::query_as(sql)
        .fetch_all(pool)
        .await
        .context("listing actions")?;

    let mut out = Vec::with_capacity(rows.len());
    for (id, title, details, conf, status, created_at, due_at, channel_name, source_name) in rows {
        let evidence_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM action_evidence WHERE action_id = ?")
                .bind(id)
                .fetch_one(pool)
                .await?;

        out.push(ActionDto {
            id,
            title,
            details,
            confidence: Confidence::parse(&conf).unwrap_or(Confidence::Low),
            status: ActionStatus::parse(&status).unwrap_or(ActionStatus::Pending),
            created_at,
            due_at,
            evidence_count: evidence_count.0,
            channel_name: if channel_name.is_empty() {
                None
            } else {
                Some(channel_name)
            },
            source_name: if source_name.is_empty() {
                None
            } else {
                Some(source_name)
            },
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy)]
pub struct MessageFilter {
    /// Max rows to return. Clamped to [1, MAX_MESSAGE_LIMIT]. Default 50.
    pub limit: i64,
}

impl Default for MessageFilter {
    fn default() -> Self {
        Self {
            limit: DEFAULT_MESSAGE_LIMIT,
        }
    }
}

/// Return messages ordered by `posted_at` desc. `has_action` is true when at
/// least one action references the message as evidence.
#[allow(clippy::type_complexity)]
pub async fn list_messages(pool: &SqlitePool, filter: MessageFilter) -> Result<Vec<MessageDto>> {
    let limit = filter.limit.clamp(1, MAX_MESSAGE_LIMIT);

    let rows: Vec<(
        i64,
        String,
        Option<String>,
        String,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        i64,
    )> = sqlx::query_as(
        "SELECT m.id, m.external_id, m.subject, m.body, p.display_name, m.posted_at, \
                c.name, s.name, \
                EXISTS(SELECT 1 FROM action_evidence ae WHERE ae.message_id = m.id) \
         FROM messages m \
         LEFT JOIN people p ON p.id = m.author_id \
         LEFT JOIN channels c ON c.id = m.channel_id \
         LEFT JOIN sources s ON s.id = c.source_id \
         ORDER BY m.posted_at DESC \
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("listing messages")?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                external_id,
                subject,
                body,
                author_display,
                posted_at,
                channel_name,
                source_name,
                has_action,
            )| MessageDto {
                id,
                external_id,
                subject,
                snippet: snippet(&body),
                author_display,
                posted_at,
                channel_name,
                source_name,
                has_action: has_action != 0,
            },
        )
        .collect())
}

/// First non-empty line, capped at SNIPPET_LEN characters with an ellipsis.
fn snippet(body: &str) -> String {
    let first = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.chars().count() <= SNIPPET_LEN {
        first.to_string()
    } else {
        let cut: String = first.chars().take(SNIPPET_LEN).collect();
        format!("{cut}…")
    }
}

/// Snapshot for the status panel: per-source health + embed-queue depth +
/// most-recent extraction timestamp.
#[allow(clippy::type_complexity)]
pub async fn get_status(pool: &SqlitePool) -> Result<StatusSnapshot> {
    let source_rows: Vec<(
        i64,
        String,
        String,
        String,
        Option<i64>,
        Option<String>,
        i64,
    )> = sqlx::query_as(
        "SELECT id, name, kind, status, last_synced_at, last_error, consecutive_failures \
         FROM sources ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("loading sources for status")?;

    let sources = source_rows
        .into_iter()
        .map(
            |(id, name, kind, status, last_synced_at, last_error, consecutive_failures)| {
                SourceStatus {
                    id,
                    name,
                    kind,
                    health: SourceHealth::parse(&status).unwrap_or(SourceHealth::Warning),
                    last_synced_at,
                    last_error,
                    consecutive_failures,
                }
            },
        )
        .collect();

    let (embed_queue_depth,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM embed_queue")
        .fetch_one(pool)
        .await
        .context("counting embed queue")?;

    let last_extraction_at: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT MAX(ran_at) FROM extraction_runs WHERE result IN ('ok', 'error')")
            .fetch_optional(pool)
            .await?;
    let last_extraction_at = last_extraction_at.and_then(|(opt,)| opt);

    Ok(StatusSnapshot {
        sources,
        embed_queue_depth,
        last_extraction_at,
    })
}

/// Return resolution suggestions awaiting user review. A row appears here when
/// the extractor's most-recent `suggested_resolution` for an action has not
/// been superseded by a later user-driven event (`resolved`, `dismissed`,
/// `claimed`, `unclaimed`, or `suggestion_dismissed`) and the action itself
/// is still active.
pub async fn list_pending_resolutions(pool: &SqlitePool) -> Result<Vec<PendingResolutionDto>> {
    // The latest-per-action subquery picks one suggested_resolution row per
    // action; the NOT EXISTS clause suppresses ones the user already handled.
    let rows: Vec<(i64, String, String, i64)> = sqlx::query_as(
        "SELECT a.id, a.title, e.data_json, e.occurred_at \
         FROM action_events e \
         JOIN actions a ON a.id = e.action_id \
         WHERE e.event_kind = 'suggested_resolution' \
           AND e.id = ( \
               SELECT MAX(e2.id) FROM action_events e2 \
               WHERE e2.action_id = e.action_id \
                 AND e2.event_kind = 'suggested_resolution' \
           ) \
           AND a.status IN ('pending', 'auto_claimed', 'claimed') \
           AND NOT EXISTS ( \
               SELECT 1 FROM action_events e3 \
               WHERE e3.action_id = e.action_id \
                 AND e3.id > e.id \
                 AND e3.actor = 'user' \
                 AND e3.event_kind IN \
                     ('resolved','dismissed','claimed','unclaimed','suggestion_dismissed') \
           ) \
         ORDER BY e.occurred_at DESC",
    )
    .fetch_all(pool)
    .await
    .context("listing pending resolutions")?;

    let mut out = Vec::with_capacity(rows.len());
    for (action_id, action_title, data_json, suggested_at) in rows {
        let parsed: serde_json::Value = serde_json::from_str(&data_json).unwrap_or_default();
        let suggested_status = parsed
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("done")
            .to_string();
        let confidence = parsed
            .get("confidence")
            .and_then(|v| v.as_str())
            .and_then(Confidence::parse)
            .unwrap_or(Confidence::Medium);
        let rationale = parsed
            .get("rationale")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        out.push(PendingResolutionDto {
            action_id,
            action_title,
            suggested_status,
            confidence,
            rationale,
            suggested_at,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use chrono::Utc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn list_actions_returns_empty_for_fresh_db() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let rows = list_actions(&pool, ActionFilter::default()).await?;
        assert!(rows.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn list_actions_omits_resolved_unless_requested() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        for (status, conf) in [
            ("pending", "medium"),
            ("done", "high"),
            ("dismissed", "low"),
        ] {
            sqlx::query(
                "INSERT INTO actions (title, confidence, status, extracted_at) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(format!("t-{status}"))
            .bind(conf)
            .bind(status)
            .bind(now)
            .execute(&pool)
            .await?;
        }

        let active = list_actions(&pool, ActionFilter::default()).await?;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].status, ActionStatus::Pending);

        let all = list_actions(
            &pool,
            ActionFilter {
                include_resolved: true,
            },
        )
        .await?;
        assert_eq!(all.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn list_messages_returns_empty_for_fresh_db() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let rows = list_messages(&pool, MessageFilter::default()).await?;
        assert!(rows.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn list_messages_orders_newest_first_and_marks_actioned() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let now = Utc::now().timestamp();

        let (source_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'work', 'kc/work', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(&pool)
        .await?;
        let (channel_id,): (i64,) = sqlx::query_as(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, 'INBOX', 'INBOX', 'mailbox') RETURNING id",
        )
        .bind(source_id)
        .fetch_one(&pool)
        .await?;
        let (author_id,): (i64,) = sqlx::query_as(
            "INSERT INTO people (source_id, external_id, display_name) \
             VALUES (?, 'ana@example.com', 'Ana') RETURNING id",
        )
        .bind(source_id)
        .fetch_one(&pool)
        .await?;

        // Two messages, ten seconds apart.
        let (older_id,): (i64,) = sqlx::query_as(
            "INSERT INTO messages (channel_id, external_id, author_id, posted_at, \
                                   subject, body, body_format, ingested_at, flags) \
             VALUES (?, 'm-older', ?, ?, 'old', 'first line\nsecond', 'text', ?, 0) RETURNING id",
        )
        .bind(channel_id)
        .bind(author_id)
        .bind(now - 100)
        .bind(now - 100)
        .fetch_one(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO messages (channel_id, external_id, author_id, posted_at, \
                                   subject, body, body_format, ingested_at, flags) \
             VALUES (?, 'm-newer', ?, ?, 'new', 'newer body', 'text', ?, 0)",
        )
        .bind(channel_id)
        .bind(author_id)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await?;

        // Action referencing the older message only.
        let (action_id,): (i64,) = sqlx::query_as(
            "INSERT INTO actions (title, confidence, status, extracted_at) \
             VALUES ('do x', 'medium', 'pending', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO action_evidence (action_id, message_id, kind, is_primary) \
             VALUES (?, ?, 'source', 1)",
        )
        .bind(action_id)
        .bind(older_id)
        .execute(&pool)
        .await?;

        let rows = list_messages(&pool, MessageFilter::default()).await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].external_id, "m-newer");
        assert_eq!(rows[1].external_id, "m-older");
        assert_eq!(rows[0].snippet, "newer body");
        assert_eq!(rows[1].snippet, "first line");
        assert!(!rows[0].has_action);
        assert!(rows[1].has_action);
        Ok(())
    }

    #[tokio::test]
    async fn get_status_reports_fresh_db_as_empty() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let s = get_status(&pool).await?;
        assert!(s.sources.is_empty());
        assert_eq!(s.embed_queue_depth, 0);
        assert!(s.last_extraction_at.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn get_status_reflects_source_and_queue_state() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let now = Utc::now().timestamp();

        sqlx::query(
            "INSERT INTO sources (kind, name, config_ref, created_at, status, last_synced_at, \
                                  last_error, consecutive_failures) \
             VALUES ('imap', 'work', 'kc/work', ?, 'warning', ?, 'transient error', 1)",
        )
        .bind(now)
        .bind(now - 60)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO embed_queue (target_kind, target_id, text_hash, enqueued_at) \
             VALUES ('message', 1, 'h1', ?), ('message', 2, 'h2', ?)",
        )
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (1, 'INBOX', 'INBOX', 'mailbox')",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO extraction_runs (channel_id, ran_at, model, prompt_version, result, \
                                          messages_pending_embed) \
             VALUES (1, ?, 'm', 1, 'ok', 0)",
        )
        .bind(now - 30)
        .execute(&pool)
        .await?;

        let s = get_status(&pool).await?;
        assert_eq!(s.sources.len(), 1);
        let src = &s.sources[0];
        assert_eq!(src.name, "work");
        assert_eq!(src.health, SourceHealth::Warning);
        assert_eq!(src.consecutive_failures, 1);
        assert_eq!(src.last_synced_at, Some(now - 60));
        assert_eq!(s.embed_queue_depth, 2);
        assert_eq!(s.last_extraction_at, Some(now - 30));
        Ok(())
    }

    /// Seed an action + a `suggested_resolution` event on it. Returns the
    /// action id so tests can drive follow-up events.
    async fn seed_suggested(
        pool: &SqlitePool,
        title: &str,
        suggested_status: &str,
        suggested_at: i64,
    ) -> Result<i64> {
        let (action_id,): (i64,) = sqlx::query_as(
            "INSERT INTO actions (title, confidence, status, extracted_at) \
             VALUES (?, 'medium', 'pending', ?) RETURNING id",
        )
        .bind(title)
        .bind(suggested_at)
        .fetch_one(pool)
        .await?;
        let data = serde_json::json!({
            "status": suggested_status,
            "confidence": "medium",
            "rationale": "Looks resolved in the window",
        })
        .to_string();
        sqlx::query(
            "INSERT INTO action_events \
             (action_id, event_kind, actor, data_json, occurred_at) \
             VALUES (?, 'suggested_resolution', 'agent_queued', ?, ?)",
        )
        .bind(action_id)
        .bind(&data)
        .bind(suggested_at)
        .execute(pool)
        .await?;
        Ok(action_id)
    }

    #[tokio::test]
    async fn list_pending_resolutions_returns_active_suggestions() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        let id = seed_suggested(&pool, "Send invoice", "done", now).await?;

        let rows = list_pending_resolutions(&pool).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action_id, id);
        assert_eq!(rows[0].action_title, "Send invoice");
        assert_eq!(rows[0].suggested_status, "done");
        assert_eq!(rows[0].confidence, Confidence::Medium);
        assert!(rows[0].rationale.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn list_pending_resolutions_suppresses_user_resolved_actions() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        let id = seed_suggested(&pool, "Send invoice", "done", now).await?;

        // User confirmed the suggestion by marking done themselves.
        sqlx::query("UPDATE actions SET status = 'done', resolved_at = ? WHERE id = ?")
            .bind(now + 5)
            .bind(id)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO action_events (action_id, event_kind, actor, occurred_at) \
             VALUES (?, 'resolved', 'user', ?)",
        )
        .bind(id)
        .bind(now + 5)
        .execute(&pool)
        .await?;

        let rows = list_pending_resolutions(&pool).await?;
        assert!(
            rows.is_empty(),
            "user-resolved action should not appear as pending. got: {rows:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_pending_resolutions_suppresses_after_explicit_reject() -> Result<()> {
        // Even if the action stays active, an explicit 'suggestion_dismissed'
        // event closes that suggestion out — the user said no.
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        let id = seed_suggested(&pool, "Send invoice", "done", now).await?;
        sqlx::query(
            "INSERT INTO action_events (action_id, event_kind, actor, occurred_at) \
             VALUES (?, 'suggestion_dismissed', 'user', ?)",
        )
        .bind(id)
        .bind(now + 5)
        .execute(&pool)
        .await?;

        let rows = list_pending_resolutions(&pool).await?;
        assert!(rows.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn list_pending_resolutions_uses_latest_suggestion_per_action() -> Result<()> {
        // Older suggestion was rejected; newer suggestion stands.
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        let id = seed_suggested(&pool, "Send invoice", "done", now - 100).await?;
        sqlx::query(
            "INSERT INTO action_events (action_id, event_kind, actor, occurred_at) \
             VALUES (?, 'suggestion_dismissed', 'user', ?)",
        )
        .bind(id)
        .bind(now - 50)
        .execute(&pool)
        .await?;
        // Newer suggestion — pretending the extractor saw new evidence.
        let data = serde_json::json!({
            "status": "cancelled",
            "confidence": "low",
            "rationale": "Looks dropped from the plan",
        })
        .to_string();
        sqlx::query(
            "INSERT INTO action_events \
             (action_id, event_kind, actor, data_json, occurred_at) \
             VALUES (?, 'suggested_resolution', 'agent_queued', ?, ?)",
        )
        .bind(id)
        .bind(&data)
        .bind(now)
        .execute(&pool)
        .await?;

        let rows = list_pending_resolutions(&pool).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].suggested_status, "cancelled");
        assert_eq!(rows[0].confidence, Confidence::Low);
        Ok(())
    }
}
