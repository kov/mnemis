//! Score the extractor against what is already known about recent mail.
//!
//! Re-runs extraction over messages the database already holds, with the
//! tools that change actions captured instead of run (nothing is written),
//! and compares what the model would have recorded with the actions already
//! there: those it found, those it missed, those it made up, and those the
//! user had dismissed. The reference is the earlier model's judgement plus the
//! user's dismissals — noisy, but the same for every model and prompt being
//! compared.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::SqlitePool;

use super::prompt::ExistingAction;
use super::{
    DryCall, ExtractionScope, batch_system_prompt, load_channel, load_rows, load_user_profile_for,
    run_agent_loop, split_into_batches, staged,
};
use crate::llm::LlmTransport;

pub use super::Strategy;

#[derive(Debug, Default, Serialize)]
pub struct EvalReport {
    pub messages: usize,
    pub batches: usize,
    pub secs: u64,
    /// Batches that failed outright.
    pub errors: Vec<String>,
    /// Actions already recorded (and not dismissed) from these messages.
    pub reference: usize,
    pub found: Vec<String>,
    pub missed: Vec<String>,
    /// What the model recorded that matches no known action.
    pub invented: Vec<String>,
    /// What the model recorded from messages whose action the user dismissed.
    pub dismissed: Vec<String>,
    pub updates: usize,
    pub resolves: usize,
    /// Resolves that close a to-do with a message the user's history
    /// resolved it with.
    pub resolved_right: Vec<String>,
    /// Resolves of a to-do with a message that didn't resolve it.
    pub resolved_wrong: Vec<String>,
    /// Messages shown as a suspicious-sender warning instead of a to-do,
    /// marked when an action was known (or dismissed) from them.
    pub warnings: Vec<String>,
    /// Each batch's closing message: the model's account of what it did.
    pub summaries: Vec<String>,
}

/// Evaluate over the messages of unmuted channels posted in `[from, to)`
/// (unix seconds): every one an action was recorded or resolved from, plus `others` more,
/// picked by a fixed scramble of their ids so repeated runs see the same set.
#[allow(clippy::too_many_arguments)]
pub async fn evaluate(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    window_char_budget: usize,
    idle_timeout: Duration,
    from: i64,
    to: i64,
    others: i64,
    strategy: Strategy,
) -> Result<EvalReport> {
    let started = Instant::now();
    let picked: Vec<(i64, i64)> = sqlx::query_as(
        "WITH candidates AS ( \
             SELECT m.id, m.channel_id, \
                 EXISTS (SELECT 1 FROM action_evidence ae \
                         WHERE ae.message_id = m.id) AS cited \
             FROM messages m JOIN channels c ON c.id = m.channel_id \
             WHERE m.posted_at >= ? AND m.posted_at < ? AND c.muted = 0) \
         SELECT id, channel_id FROM candidates WHERE cited \
         UNION \
         SELECT id, channel_id FROM ( \
             SELECT id, channel_id FROM candidates WHERE NOT cited \
             ORDER BY (id * 2654435761) % 4294967296 LIMIT ?)",
    )
    .bind(from)
    .bind(to)
    .bind(others)
    .fetch_all(pool)
    .await
    .context("picking messages to evaluate")?;
    let mut by_channel: BTreeMap<i64, HashSet<i64>> = BTreeMap::new();
    for (id, channel) in picked {
        by_channel.entry(channel).or_default().insert(id);
    }

    let mut report = EvalReport::default();
    for (channel_id, ids) in by_channel {
        evaluate_channel(
            pool,
            llm,
            window_char_budget,
            idle_timeout,
            channel_id,
            &ids,
            strategy,
            &mut report,
        )
        .await?;
    }
    report.secs = started.elapsed().as_secs();
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_channel(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    window_char_budget: usize,
    idle_timeout: Duration,
    channel_id: i64,
    ids: &HashSet<i64>,
    strategy: Strategy,
    report: &mut EvalReport,
) -> Result<()> {
    let channel = load_channel(pool, channel_id).await?;
    let profile = load_user_profile_for(pool, &channel.source_kind).await?;
    let memory_rules = crate::memory::rules(pool).await?;
    let first = ids.iter().min().copied().unwrap_or(0);
    let mut rows = load_rows(pool, channel_id, first - 1, i64::MAX).await?;
    rows.retain(|m| ids.contains(&m.id));
    report.messages += rows.len();

    // What is known about these messages: the actions recorded from them.
    let known: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT a.id, a.title, a.status, m.external_id FROM actions a \
         JOIN action_evidence ae ON ae.action_id = a.id AND ae.kind = 'source' \
         JOIN messages m ON m.id = ae.message_id \
         WHERE m.channel_id = ? AND m.id >= ?",
    )
    .bind(channel_id)
    .bind(first)
    .fetch_all(pool)
    .await
    .context("loading the reference actions")?;
    let in_window: HashSet<&str> = rows.iter().map(|m| m.external_id.as_str()).collect();
    let mut reference: HashMap<i64, (String, bool, HashSet<String>)> = HashMap::new();
    for (id, title, status, external_id) in known {
        if !in_window.contains(external_id.as_str()) {
            continue;
        }
        let entry = reference
            .entry(id)
            .or_insert_with(|| (title, status == "dismissed", HashSet::new()));
        entry.2.insert(external_id);
    }

    // What each to-do was resolved with, to score resolves.
    let resolutions: HashSet<(i64, String)> = sqlx::query_as(
        "SELECT ae.action_id, m.external_id FROM action_evidence ae \
         JOIN messages m ON m.id = ae.message_id \
         WHERE ae.kind = 'resolution' AND m.channel_id = ?",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await
    .context("loading resolutions")?
    .into_iter()
    .collect();
    let titles: HashMap<i64, String> = sqlx::query_as("SELECT id, title FROM actions")
        .fetch_all(pool)
        .await
        .context("loading action titles")?
        .into_iter()
        .collect();

    let scope = ExtractionScope {
        source_id: channel.source_id,
        channel_id,
    };
    let mut calls: Vec<DryCall> = Vec::new();
    // A day's mail at a time, as frequent syncs would see it, so each batch
    // is judged as of its own day.
    let mut days: BTreeMap<i64, Vec<_>> = BTreeMap::new();
    for row in rows {
        days.entry(row.posted_at.div_euclid(86_400))
            .or_default()
            .push(row);
    }
    let mut queue: std::collections::VecDeque<_> = days
        .into_values()
        .flat_map(|day| split_into_batches(day, window_char_budget))
        .collect();
    while let Some(batch) = queue.pop_front() {
        report.batches += 1;
        // As if extraction ran just after the newest of these arrived, so
        // old deadlines aren't judged as long past.
        let now = batch
            .iter()
            .map(|m| m.posted_at)
            .max()
            .and_then(|t| chrono::DateTime::from_timestamp(t + 600, 0))
            .unwrap_or_else(chrono::Utc::now);
        // The to-dos open on the batch's day, as a sync then would have seen
        // them; none recorded from this day's mail, or the answers would be
        // in the prompt.
        let day = batch[0].posted_at.div_euclid(86_400) * 86_400;
        let existing = open_on(pool, channel_id, day).await?;
        if strategy == Strategy::Staged {
            let ctx = staged::Context {
                profile: &profile,
                memory_rules: memory_rules.as_deref(),
                existing: &existing,
                now,
                body_chars: window_char_budget,
                idle_timeout,
            };
            for message in &batch {
                match staged::run(pool, llm, scope, &ctx, message, Some(&mut calls)).await {
                    Ok(outcome) => {
                        if let Some(warning) = outcome.warning {
                            let known = reference
                                .values()
                                .find(|(_, _, evidence)| evidence.contains(&message.external_id))
                                .map_or("", |(_, dismissed, _)| {
                                    if *dismissed {
                                        " [dismissed action]"
                                    } else {
                                        " [known action]"
                                    }
                                });
                            report.warnings.push(format!(
                                "{}{known}: {} — {warning}",
                                channel.name,
                                message.subject.as_deref().unwrap_or("(no subject)")
                            ));
                        }
                        report
                            .summaries
                            .push(format!("{}: {}", channel.name, outcome.summary));
                    }
                    Err(e) => report.errors.push(format!("{}: {e:#}", channel.name)),
                }
            }
            continue;
        }
        let prompt = batch_system_prompt(
            &channel,
            &profile,
            memory_rules.as_deref(),
            &batch,
            &existing,
            now,
        );
        let outcome = run_agent_loop(
            pool,
            llm,
            scope,
            prompt,
            window_char_budget,
            idle_timeout,
            None,
            Some(&mut calls),
        )
        .await;
        if let Ok((_, Some(summary))) = &outcome {
            report
                .summaries
                .push(format!("{}: {summary}", channel.name));
        }
        if let Err(e) = outcome {
            let chain = format!("{e:#}");
            if crate::agent::is_context_overflow(&chain) && batch.len() > 1 {
                let mut left = batch;
                let right = left.split_off(left.len() / 2);
                queue.push_front(right);
                queue.push_front(left);
            } else {
                report.errors.push(format!("{}: {chain}", channel.name));
            }
        }
    }

    // Score: a recorded action matches a known one when they cite a message
    // in common.
    let mut matched: HashSet<i64> = HashSet::new();
    for call in &calls {
        let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
        match call.name.as_str() {
            "update_action" => report.updates += 1,
            "resolve_action" => {
                report.resolves += 1;
                let id = args["action_id"]
                    .as_str()
                    .and_then(|s| s.trim_start_matches("A-").parse::<i64>().ok())
                    .unwrap_or(0);
                let title = titles
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| format!("A-{id}"));
                let right = args["evidence_external_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .any(|e| resolutions.contains(&(id, e.to_string())));
                if right {
                    report.resolved_right.push(title);
                } else {
                    report.resolved_wrong.push(title);
                }
            }
            _ => {
                let title = args["title"].as_str().unwrap_or("(untitled)").to_string();
                let cited: HashSet<&str> = args["evidence_external_ids"]
                    .as_array()
                    .map(|ids| ids.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let hits: Vec<(&i64, bool)> = reference
                    .iter()
                    .filter(|(_, (_, _, evidence))| {
                        evidence.iter().any(|e| cited.contains(e.as_str()))
                    })
                    .map(|(id, (_, dismissed, _))| (id, *dismissed))
                    .collect();
                if hits.iter().any(|(_, dismissed)| *dismissed) {
                    report.dismissed.push(title);
                } else if hits.is_empty() {
                    report.invented.push(title);
                } else {
                    matched.extend(hits.iter().map(|(id, _)| **id));
                }
            }
        }
    }
    for (id, (title, dismissed, _)) in &reference {
        if *dismissed {
            continue;
        }
        report.reference += 1;
        if matched.contains(id) {
            report.found.push(title.clone());
        } else {
            report.missed.push(title.clone());
        }
    }
    Ok(())
}

/// The channel's to-dos that were open at `at` (unix seconds): recorded
/// from earlier mail, not closed before then, and not dismissed.
async fn open_on(pool: &SqlitePool, channel_id: i64, at: i64) -> Result<Vec<ExistingAction>> {
    let rows: Vec<(i64, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT a.id, a.title, a.details, a.due_at FROM actions a \
         JOIN action_evidence ae ON ae.action_id = a.id AND ae.kind = 'source' \
         JOIN messages m ON m.id = ae.message_id \
         WHERE m.channel_id = ?1 AND a.status != 'dismissed' \
         AND (a.resolved_at IS NULL OR a.resolved_at >= ?2) \
         GROUP BY a.id HAVING MIN(m.posted_at) < ?2 \
         ORDER BY MIN(m.posted_at) DESC LIMIT 50",
    )
    .bind(channel_id)
    .bind(at)
    .fetch_all(pool)
    .await
    .context("loading the to-dos open then")?;
    Ok(rows
        .into_iter()
        .map(|(id, title, details, due_at)| ExistingAction {
            id,
            title,
            details,
            due_at: due_at.and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::tools;
    use crate::test_util::{
        MockLlm, SeedMessage, fetch_actions, mock, seed_history, seed_messages, seed_minimal,
    };

    fn message<'a>(external_id: &'a str, subject: &'a str, body: &'a str) -> SeedMessage<'a> {
        SeedMessage {
            external_id,
            author_email: "ana@example.com",
            author_name: "Ana",
            subject,
            body,
            recipients: &[],
        }
    }

    #[tokio::test]
    async fn scores_against_known_actions_without_writing() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let pool = crate::db::open(&tmp.path().join("t.db")).await?;
        crate::db::migrate(&pool).await?;
        let ctx = seed_minimal(&pool).await?;
        seed_messages(
            &pool,
            ctx.source_id,
            ctx.channel_id,
            &[
                message("ask", "Review", "Can you review the budget by Friday?"),
                message("news", "Digest", "Ten crates you should know about."),
                message("promo", "Sale", "Everything 50% off this weekend."),
                message("trip", "Trip", "Please book the hotel for March."),
            ],
        )
        .await?;
        // What an earlier run recorded: the review and the hotel are real,
        // and the user dismissed one made from the promotion.
        for (title, evidence) in [
            ("Review the budget", "ask"),
            ("Book the hotel", "trip"),
            ("Shop the sale", "promo"),
        ] {
            let args = serde_json::json!({
                "title": title, "details": "", "confidence": "high", "rationale": "",
                "evidence_external_ids": [evidence],
            });
            tools::dispatch(
                &pool,
                tools::ToolScope::Source(ctx.source_id),
                &mut tools::FetchBudget::unlimited(),
                "record_action",
                &args.to_string(),
            )
            .await;
        }
        sqlx::query("UPDATE actions SET status = 'dismissed' WHERE title = 'Shop the sale'")
            .execute(&pool)
            .await?;

        let llm = MockLlm::new(vec![
            mock::record_action("Review the budget", "high", &["ask"]),
            mock::record_action("Read the digest", "low", &["news"]),
            mock::record_action("Buy things on sale", "medium", &["promo"]),
            mock::no_tools("Recorded three."),
        ]);
        let now = chrono::Utc::now().timestamp();
        let report = evaluate(
            &pool,
            &llm,
            100_000,
            Duration::from_secs(5),
            now - 3600,
            now + 3600,
            1,
            Strategy::Agent,
        )
        .await?;

        // The three cited messages, and one of the rest.
        assert_eq!(report.messages, 4);
        assert_eq!(report.reference, 2);
        assert_eq!(report.found, vec!["Review the budget"]);
        assert_eq!(report.missed, vec!["Book the hotel"]);
        assert_eq!(report.invented, vec!["Read the digest"]);
        assert_eq!(report.dismissed, vec!["Buy things on sale"]);
        assert!(report.errors.is_empty());
        // Nothing the model "recorded" reached the database.
        assert_eq!(fetch_actions(&pool).await?.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn staged_strategy_records_only_what_it_judges_actionable() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let pool = crate::db::open(&tmp.path().join("t.db")).await?;
        crate::db::migrate(&pool).await?;
        let ctx = seed_minimal(&pool).await?;
        seed_messages(
            &pool,
            ctx.source_id,
            ctx.channel_id,
            &[
                message("ask", "Review", "Can you review the budget by Friday?"),
                message("news", "Digest", "Ten crates you should know about."),
                SeedMessage {
                    external_id: "phish",
                    author_email: "no-reply@suporte5.ux6qrc.com",
                    author_name: "Bradesco Net Empresas",
                    subject: "Atualização necessária",
                    body: "Confirme seus dados para evitar o bloqueio.",
                    recipients: &[],
                },
            ],
        )
        .await?;
        seed_history(&pool, ctx.channel_id).await?;
        let args = serde_json::json!({
            "title": "Review the budget", "details": "", "confidence": "high", "rationale": "",
            "evidence_external_ids": ["ask"],
        });
        tools::dispatch(
            &pool,
            tools::ToolScope::Source(ctx.source_id),
            &mut tools::FetchBudget::unlimited(),
            "record_action",
            &args.to_string(),
        )
        .await;

        // Per message: a kind, a summary, a verdict, and (on 1) the recording
        // turns.
        let llm = MockLlm::new(vec![
            mock::no_tools("personal"),
            mock::no_tools("Ana asks the user to review the budget by Friday."),
            mock::no_tools("1"),
            mock::record_action("Review the budget", "high", &["ask"]),
            // Marketing skips the verdict.
            mock::no_tools("marketing"),
            mock::no_tools("Ten crates newsletter."),
            // A new domain under an unrelated name: a warning, no verdict.
            mock::no_tools("account"),
            mock::no_tools("Bradesco asks the user to confirm their data."),
        ]);
        let now = chrono::Utc::now().timestamp();
        let report = evaluate(
            &pool,
            &llm,
            100_000,
            Duration::from_secs(5),
            now - 3600,
            now + 3600,
            5,
            Strategy::Staged,
        )
        .await?;

        assert_eq!(report.found, vec!["Review the budget"]);
        assert!(report.invented.is_empty());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.summaries.len(), 3);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("ux6qrc.com"),
            "{}",
            report.warnings[0]
        );
        assert_eq!(fetch_actions(&pool).await?.len(), 1);
        Ok(())
    }
}
