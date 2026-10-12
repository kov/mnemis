//! New mail on a to-do's thread: a reply with a new date, the user's own
//! "done, sent it". Each thread with open to-dos and mail newer than what
//! was last considered gets a short agent run of its own, the to-dos and the
//! new mail, with tools to update, resolve or record. This covers mail in
//! any folder (the user's replies in Sent, a muted Archive), and channel
//! extraction leaves that mail to it ([`owned`]).
//!
//! Each to-do keeps two message ids (see migration 0014): `followup_from`,
//! the thread's newest message when the to-do was made, and
//! `followed_up_to`, the newest one a follow-up has handled.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use sqlx::SqlitePool;

use super::tools::{self, FetchBudget, ToolScope};
use crate::agent;
use crate::llm::{InputItem, LlmTransport, OutputItem, Role};

/// Turns a run may take; a model still calling tools after them is stopped.
const TURNS: usize = 4;
/// Fewest characters of each email shown, however many there are.
const MIN_EMAIL_CHARS: usize = 800;

const SYSTEM: &str = "You keep the user's to-dos current. New emails arrived \
in an email thread the user has open to-dos about. For each to-do they \
affect: call update_action with what changed (new or moved dates, amounts, \
places, people, next steps), rewriting details so they stay complete, and \
put the new emails' ids in evidence_external_ids; call resolve_action when \
they show it's done, answered or no longer needed (the user's own reply \
saying it's handled counts). Call record_action only for a clearly separate \
new request to the user. When the emails change nothing, call no tool and \
answer \"nothing\". Write in the to-do's language.";

/// A thread to follow up: its open to-dos and the mail after them.
#[derive(Debug)]
pub struct Pending {
    pub thread_id: i64,
    pub action_ids: Vec<i64>,
    /// The new mail, one row per message (a message filed in two folders
    /// counts once), oldest first.
    pub message_ids: Vec<i64>,
    /// The newest new row, copies included: what the to-dos have seen after
    /// the run.
    up_to: i64,
}

/// Set a new to-do's marks at its thread's newest message: later mail is
/// the follow-up's.
pub(crate) async fn start(conn: &mut sqlx::SqliteConnection, action_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE actions SET followup_from = ( \
             SELECT MAX(t.id) FROM action_evidence ae \
             JOIN messages m ON m.id = ae.message_id \
             JOIN messages t ON t.thread_id = m.thread_id \
             WHERE ae.action_id = ?1 AND ae.is_primary = 1) \
         WHERE id = ?1",
    )
    .bind(action_id)
    .execute(&mut *conn)
    .await
    .context("marking the to-do's thread")?;
    sqlx::query("UPDATE actions SET followed_up_to = followup_from WHERE id = ?")
        .bind(action_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The account's threads with open to-dos and mail they haven't seen.
pub async fn pending(pool: &SqlitePool, source_id: i64) -> Result<Vec<Pending>> {
    // Open to-dos only, as in channel extraction's list.
    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
        "SELECT m.thread_id, a.id, a.followed_up_to FROM actions a \
         JOIN action_evidence ae ON ae.action_id = a.id AND ae.is_primary = 1 \
         JOIN messages m ON m.id = ae.message_id \
         JOIN mail_threads th ON th.id = m.thread_id \
         WHERE th.source_id = ? AND a.status IN ('pending', 'auto_claimed', 'claimed') \
           AND a.followed_up_to IS NOT NULL \
           AND EXISTS (SELECT 1 FROM messages n \
                       WHERE n.thread_id = m.thread_id AND n.id > a.followed_up_to) \
         ORDER BY m.thread_id, a.id",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await
    .context("finding to-dos with new mail")?;
    let mut threads: BTreeMap<i64, (Vec<i64>, i64)> = BTreeMap::new();
    for (thread_id, action_id, seen) in rows {
        let entry = threads.entry(thread_id).or_insert((Vec::new(), seen));
        entry.0.push(action_id);
        entry.1 = entry.1.min(seen);
    }
    let mut out = Vec::new();
    for (thread_id, (action_ids, after)) in threads {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, external_id FROM messages WHERE thread_id = ? AND id > ? \
             ORDER BY posted_at, id",
        )
        .bind(thread_id)
        .bind(after)
        .fetch_all(pool)
        .await?;
        let up_to = rows.iter().map(|(id, _)| *id).max().unwrap_or(after);
        let mut seen = HashSet::new();
        let message_ids = rows
            .into_iter()
            .filter(|(_, external_id)| seen.insert(external_id.clone()))
            .map(|(id, _)| id)
            .collect();
        out.push(Pending {
            thread_id,
            action_ids,
            message_ids,
            up_to,
        });
    }
    Ok(out)
}

/// Run the follow-up of one thread; its mail should have its bodies (see
/// `orchestrator::complete_messages`). The to-dos are marked as having seen
/// the mail only when the run succeeds, so a failed one is tried again.
/// Returns how many to-dos it recorded.
pub async fn run(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    pending: &Pending,
    char_budget: usize,
) -> Result<usize> {
    let source_id: i64 = sqlx::query_scalar("SELECT source_id FROM mail_threads WHERE id = ?")
        .bind(pending.thread_id)
        .fetch_one(pool)
        .await
        .context("finding the thread")?;
    let prompt = prompt(pool, pending, char_budget).await?;
    let mut history = vec![InputItem::Message {
        role: Role::User,
        content: prompt,
    }];
    let defs: Vec<_> = tools::definitions()
        .into_iter()
        .filter(|d| ["update_action", "resolve_action", "record_action"].contains(&d.name.as_str()))
        .collect();
    let mut budget = FetchBudget::new(0);
    let mut recorded = 0;
    for step in 0..TURNS {
        let response =
            agent::send_with_stall_retry(llm, SYSTEM, &history, &defs, step, None, |_| {}).await?;
        let mut calls = Vec::new();
        for item in response.output {
            match item {
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    history.push(InputItem::FunctionCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    calls.push((call_id, name, arguments));
                }
                OutputItem::Thinking { block } => history.push(InputItem::Thinking { block }),
                _ => {}
            }
        }
        if calls.is_empty() {
            break;
        }
        for (call_id, name, arguments) in calls {
            let out = tools::dispatch(
                pool,
                ToolScope::Source(source_id),
                &mut budget,
                &name,
                &arguments,
            )
            .await;
            recorded += usize::from(out.recorded_action);
            history.push(InputItem::FunctionCallOutput {
                call_id,
                output: out.output,
            });
        }
    }
    let ids = serde_json::to_string(&pending.action_ids)?;
    sqlx::query(
        "UPDATE actions SET followed_up_to = ? \
         WHERE id IN (SELECT value FROM json_each(?)) AND followed_up_to < ?",
    )
    .bind(pending.up_to)
    .bind(&ids)
    .bind(pending.up_to)
    .execute(pool)
    .await
    .context("marking the thread followed up")?;
    Ok(recorded)
}

/// Today, the user's addresses, the thread's open to-dos, then its new mail
/// within `char_budget`.
async fn prompt(pool: &SqlitePool, pending: &Pending, char_budget: usize) -> Result<String> {
    let mut out = format!("Today is {}.\n", Local::now().format("%A %Y-%m-%d"));
    let profile = super::load_user_profile_for(pool, "imap").await?;
    if !profile.identifiers.is_empty() {
        out.push_str(&format!(
            "The user's addresses: {}.\n",
            profile.identifiers.join(", ")
        ));
    }
    out.push_str("\n# Open to-dos on this thread\n");
    for id in &pending.action_ids {
        let (title, details, due_at): (String, Option<String>, Option<i64>) =
            sqlx::query_as("SELECT title, details, due_at FROM actions WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await?;
        let due = due_at
            .and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0))
            .map(|d| format!(" — due {}", d.with_timezone(&Local).to_rfc3339()))
            .unwrap_or_default();
        out.push_str(&format!(
            "[A-{id}] {title}{due}\n{}\n",
            details.unwrap_or_default()
        ));
    }
    out.push_str("\n# New emails in the thread\n");
    let each = (char_budget / pending.message_ids.len().max(1)).max(MIN_EMAIL_CHARS);
    for id in &pending.message_ids {
        out.push_str("\n---\n");
        out.push_str(&super::manual::email(pool, *id, each).await?);
        out.push('\n');
    }
    Ok(out)
}

/// Which of `message_ids` channel extraction leaves to the follow-up: mail
/// that came after a to-do on its thread was made, while that to-do is open
/// (it's followed up, now or on a retry) or once a follow-up has handled it.
pub(super) async fn owned(pool: &SqlitePool, message_ids: &[i64]) -> Result<HashSet<i64>> {
    if message_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let owned: Vec<i64> = sqlx::query_scalar(
        "SELECT n.id FROM messages n WHERE n.id IN (SELECT value FROM json_each(?)) \
           AND EXISTS (SELECT 1 FROM actions a \
                       JOIN action_evidence ae ON ae.action_id = a.id AND ae.is_primary = 1 \
                       JOIN messages m ON m.id = ae.message_id \
                       WHERE m.thread_id = n.thread_id AND n.id > a.followup_from \
                         AND (a.status IN ('pending', 'auto_claimed', 'claimed') \
                              OR n.id <= a.followed_up_to))",
    )
    .bind(serde_json::to_string(message_ids)?)
    .fetch_all(pool)
    .await
    .context("finding mail left to follow-ups")?;
    Ok(owned.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::OutputItem;
    use crate::test_util::{MockLlm, MockStep, mock};
    use serde_json::json;

    /// An account with an inbox and a Sent folder (not polled for to-dos),
    /// a thread whose first email has a to-do, and an unrelated email.
    async fn setup() -> Result<(tempfile::TempDir, SqlitePool, i64)> {
        let tmp = tempfile::TempDir::new()?;
        let pool = crate::db::open(&tmp.path().join("t.db")).await?;
        crate::db::migrate(&pool).await?;
        sqlx::query(
            "INSERT INTO sources (id, kind, name, config_ref, created_at) VALUES (1, 'imap', 'a', 'r', 0); \
             INSERT INTO channels (id, source_id, external_id, name, kind) VALUES (1, 1, 'INBOX', 'INBOX', 'mailbox'); \
             INSERT INTO channels (id, source_id, external_id, name, kind, muted) VALUES (2, 1, 'Sent', 'Sent', 'mailbox', 1); \
             INSERT INTO mail_threads (id, source_id) VALUES (10, 1), (11, 1); \
             INSERT INTO messages (id, channel_id, external_id, posted_at, subject, body, body_format, ingested_at, thread_id) \
             VALUES (1, 1, '<m1@x>', 100, 'Dinner', 'Can you book a table for Thursday?', 'text', 0, 10), \
                    (2, 1, '<other@x>', 150, 'News', 'Weekly news.', 'text', 0, 11);",
        )
        .execute(&pool)
        .await?;
        let out = tools::dispatch(
            &pool,
            ToolScope::Source(1),
            &mut FetchBudget::unlimited(),
            "record_action",
            &json!({"title": "Book a table", "details": "For Thursday", "confidence": "medium",
                    "rationale": "asked", "evidence_external_ids": ["<m1@x>"]})
            .to_string(),
        )
        .await;
        assert!(out.recorded_action, "{}", out.output);
        let action_id: i64 = sqlx::query_scalar("SELECT id FROM actions")
            .fetch_one(&pool)
            .await?;
        Ok((tmp, pool, action_id))
    }

    /// A reply on the thread, filed in Sent for reading only.
    async fn reply(pool: &SqlitePool, id: i64, body: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO messages (id, channel_id, external_id, posted_at, subject, body, body_format, \
                                   ingested_at, thread_id, reader_only) \
             VALUES (?, 2, ?, ?, 'Re: Dinner', ?, 'text', 0, 10, 1)",
        )
        .bind(id)
        .bind(format!("<m{id}@x>"))
        .bind(100 + id)
        .bind(body)
        .execute(pool)
        .await?;
        Ok(())
    }

    fn update(action_id: i64, details: &str, evidence: &str) -> crate::llm::ResponsesResponse {
        mock::turn(vec![OutputItem::FunctionCall {
            call_id: "c1".to_string(),
            name: "update_action".to_string(),
            arguments: json!({"action_id": format!("A-{action_id}"), "details": details,
                              "evidence_external_ids": [evidence]})
            .to_string(),
        }])
    }

    #[tokio::test]
    async fn new_mail_on_the_thread_updates_its_to_do() -> Result<()> {
        let (_tmp, pool, action_id) = setup().await?;
        assert!(pending(&pool, 1).await?.is_empty(), "nothing new yet");

        reply(&pool, 3, "Thursday is full, make it Friday.").await?;
        let threads = pending(&pool, 1).await?;
        assert_eq!(threads.len(), 1);
        assert_eq!(
            (
                threads[0].action_ids.clone(),
                threads[0].message_ids.clone()
            ),
            (vec![action_id], vec![3])
        );

        let llm = MockLlm::new(vec![
            update(action_id, "For Friday", "<m3@x>"),
            mock::no_tools("updated"),
        ]);
        run(&pool, &llm, &threads[0], 10_000).await?;
        let (details, evidence): (String, i64) = sqlx::query_as(
            "SELECT details, (SELECT COUNT(*) FROM action_evidence WHERE action_id = a.id) \
             FROM actions a WHERE id = ?",
        )
        .bind(action_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!((details.as_str(), evidence), ("For Friday", 2));
        assert!(pending(&pool, 1).await?.is_empty(), "the reply is handled");

        // A reply that changes nothing: no tool call, still handled.
        reply(&pool, 4, "Thanks!").await?;
        let threads = pending(&pool, 1).await?;
        run(
            &pool,
            &MockLlm::new(vec![mock::no_tools("nothing")]),
            &threads[0],
            10_000,
        )
        .await?;
        assert!(pending(&pool, 1).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_follow_up_is_tried_again() -> Result<()> {
        let (_tmp, pool, _) = setup().await?;
        reply(&pool, 3, "Make it Friday.").await?;
        let threads = pending(&pool, 1).await?;
        let llm = MockLlm::scripted(vec![MockStep::Error("unreachable".to_string())]);
        assert!(run(&pool, &llm, &threads[0], 10_000).await.is_err());
        assert_eq!(pending(&pool, 1).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn channel_extraction_leaves_the_thread_s_new_mail_to_follow_ups() -> Result<()> {
        let (_tmp, pool, action_id) = setup().await?;
        reply(&pool, 3, "Make it Friday.").await?;
        // The to-do's own email and other threads stay extraction's.
        assert_eq!(owned(&pool, &[1, 2, 3]).await?, HashSet::from([3]));

        // Handled and then done: still not extraction's; later mail is.
        let threads = pending(&pool, 1).await?;
        run(
            &pool,
            &MockLlm::new(vec![mock::no_tools("nothing")]),
            &threads[0],
            10_000,
        )
        .await?;
        sqlx::query("UPDATE actions SET status = 'done' WHERE id = ?")
            .bind(action_id)
            .execute(&pool)
            .await?;
        reply(&pool, 4, "Anything else?").await?;
        assert_eq!(owned(&pool, &[3, 4]).await?, HashSet::from([3]));
        assert!(
            pending(&pool, 1).await?.is_empty(),
            "done to-dos aren't followed up"
        );
        Ok(())
    }

    #[tokio::test]
    async fn extraction_skips_the_follow_up_s_mail_and_moves_past_it() -> Result<()> {
        let (_tmp, pool, _) = setup().await?;
        // Mark the inbox extracted up to the to-do's email, then a reply
        // lands in it.
        sqlx::query(
            "INSERT INTO extraction_runs (channel_id, ran_at, up_to_message_id, model,                                           prompt_version, result)              VALUES (1, 0, 2, 'm', 1, 'ok')",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO messages (id, channel_id, external_id, posted_at, subject, body,                                    body_format, ingested_at, thread_id)              VALUES (3, 1, '<m3@x>', 103, 'Re: Dinner', 'Make it Friday.', 'text', 0, 10)",
        )
        .execute(&pool)
        .await?;
        // No model call: the window holds only the follow-up's mail.
        let outcome = super::super::extract_for_channel(
            &pool,
            &MockLlm::new(vec![]),
            1,
            "m",
            10_000,
            std::time::Duration::from_secs(5),
            None,
            super::super::Strategy::Agent,
        )
        .await?;
        assert_eq!(
            (outcome.result, outcome.up_to_message_id),
            ("no_activity", Some(3))
        );
        Ok(())
    }
}
