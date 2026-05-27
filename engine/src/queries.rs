//! Read-side query helpers that translate SQLite rows into the wire DTOs
//! defined in `mnemis-types`. Used by Tauri commands and CLI commands.

use anyhow::{Context, Result};
use mnemis_types::{ActionDto, ActionStatus, Confidence};
use sqlx::SqlitePool;

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
}
