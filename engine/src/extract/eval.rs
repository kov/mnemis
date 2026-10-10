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

use super::{
    DryCall, ExtractionScope, batch_system_prompt, load_channel, load_existing_actions,
    load_user_profile_for, load_window, run_agent_loop, split_into_batches,
};
use crate::llm::LlmTransport;

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
}

/// Evaluate over up to `max_messages` of the newest messages posted in the
/// last `days` days, from unmuted channels.
pub async fn evaluate(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    window_char_budget: usize,
    idle_timeout: Duration,
    days: i64,
    max_messages: i64,
) -> Result<EvalReport> {
    let started = Instant::now();
    let since = chrono::Utc::now().timestamp() - days * 86_400;
    let picked: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT m.id, m.channel_id FROM messages m JOIN channels c ON c.id = m.channel_id \
         WHERE m.posted_at >= ? AND c.muted = 0 ORDER BY m.posted_at DESC LIMIT ?",
    )
    .bind(since)
    .bind(max_messages)
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
            &mut report,
        )
        .await?;
    }
    report.secs = started.elapsed().as_secs();
    Ok(report)
}

async fn evaluate_channel(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    window_char_budget: usize,
    idle_timeout: Duration,
    channel_id: i64,
    ids: &HashSet<i64>,
    report: &mut EvalReport,
) -> Result<()> {
    let channel = load_channel(pool, channel_id).await?;
    let profile = load_user_profile_for(pool, &channel.source_kind).await?;
    let memory_rules = crate::memory::rules(pool).await?;
    let first = ids.iter().min().copied().unwrap_or(0);
    let mut rows = load_window(pool, channel_id, Some(first - 1)).await?;
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

    // The prompt may only show actions that predate these messages, or the
    // answers would be in it.
    let existing: Vec<_> = load_existing_actions(pool, channel_id)
        .await?
        .into_iter()
        .filter(|a| !reference.contains_key(&a.id))
        .collect();

    let scope = ExtractionScope {
        source_id: channel.source_id,
        channel_id,
    };
    let mut calls: Vec<DryCall> = Vec::new();
    let mut queue: std::collections::VecDeque<_> =
        split_into_batches(rows, window_char_budget).into();
    while let Some(batch) = queue.pop_front() {
        report.batches += 1;
        let prompt = batch_system_prompt(
            &channel,
            &profile,
            memory_rules.as_deref(),
            &batch,
            &existing,
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
            "resolve_action" => report.resolves += 1,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::tools;
    use crate::test_util::{
        MockLlm, SeedMessage, fetch_actions, mock, seed_messages, seed_minimal,
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
        let report = evaluate(&pool, &llm, 100_000, Duration::from_secs(5), 1, 10).await?;

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
}
