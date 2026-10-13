//! Mail the user is waiting for: "when the Mailman confirmation arrives,
//! make me a to-do to answer it". The chat agent saves a watch (what to look
//! for, what to do); before channel extraction, every sync checks the new
//! mail against the open watches, and an email that matches gets a short
//! agent run that carries out the instruction. The chat the watch came from
//! gets a note saying what happened, as it does when a watch expires
//! unmatched.
//!
//! Matching reads only the sender, subject and opening of each email, in
//! batches, so the cost stays small whatever the mail volume; the full email
//! is read only for a match. Each watch keeps `checked_up_to`, the newest
//! message checked against it, advanced only after a successful check, so a
//! failed one is tried again. A matched email is the watch's: channel
//! extraction leaves it alone ([`owned`]).

use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::{Local, Utc};
use sqlx::SqlitePool;

use super::tools::{self, FetchBudget, ToolScope};
use crate::agent;
use crate::llm::{ContentItem, InputItem, LlmTransport, OutputItem, ResponsesResponse, Role};
use crate::mail::view::role;
use mnemis_types::MailboxRole;

/// How long a watch waits when the chat doesn't say.
pub const DEFAULT_DAYS: i64 = 7;
/// Turns the run on a match may take.
const TURNS: usize = 4;
/// Characters of each email's opening shown when matching.
const SNIPPET_CHARS: usize = 200;
/// Fewest characters of the matched email shown to the run.
const MIN_EMAIL_CHARS: usize = 2_000;

const MATCH_SYSTEM: &str = "You check new emails against emails the user is \
waiting for. Each watch (W-n) describes an email the user expects. Reply with \
one line per email that is the one a watch waits for, as \"W-n E-n\", and \
nothing else; reply \"none\" when none is. Match only an email that is clearly \
the one described, not merely the same sender or topic.";

const ACT_SYSTEM: &str = "The user asked you to watch for an email and to do \
something when it arrives. An email that looks like it has arrived. If it \
isn't the one described after all, call no tool and answer \"no match\". \
Otherwise carry out the instruction with the tools: the instruction is the \
user's own, so record and change to-dos with high confidence, and cite the \
email's id as evidence. Then answer with one short sentence telling the user \
what you did (\"Created a to-do to …\"), in the language of their \
instruction.";

/// An open watch.
#[derive(Debug, Clone)]
pub struct Watch {
    pub id: i64,
    pub chat_id: Option<i64>,
    pub looking_for: String,
    pub instruction: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    checked_up_to: i64,
}

/// Save a watch from chat `chat_id`; it considers mail arriving from now
/// on. `days` is how long it waits (`None`: [`DEFAULT_DAYS`]; 0: forever).
pub async fn create(
    pool: &SqlitePool,
    chat_id: Option<i64>,
    looking_for: &str,
    instruction: &str,
    days: Option<i64>,
) -> Result<i64> {
    let looking_for = looking_for.trim();
    let instruction = instruction.trim();
    anyhow::ensure!(!looking_for.is_empty(), "say which email to watch for");
    anyhow::ensure!(!instruction.is_empty(), "say what to do when it arrives");
    let now = Utc::now().timestamp();
    let days = days.unwrap_or(DEFAULT_DAYS);
    anyhow::ensure!((0..=365).contains(&days), "days must be 0 (forever) to 365");
    let expires_at = (days > 0).then(|| now + days * 86_400);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO mail_watches (chat_id, looking_for, instruction, created_at, expires_at, \
                                   checked_up_to) \
         VALUES (?, ?, ?, ?, ?, (SELECT COALESCE(MAX(id), 0) FROM messages)) RETURNING id",
    )
    .bind(chat_id)
    .bind(looking_for)
    .bind(instruction)
    .bind(now)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .context("saving the watch")?;
    Ok(id)
}

/// Stop an open watch. False when there was no open watch `id`.
pub async fn cancel(pool: &SqlitePool, id: i64) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE mail_watches SET status = 'cancelled', closed_at = ? \
         WHERE id = ? AND status = 'open'",
    )
    .bind(Utc::now().timestamp())
    .bind(id)
    .execute(pool)
    .await
    .context("cancelling the watch")?;
    Ok(done.rows_affected() > 0)
}

/// The open watches, oldest first.
pub async fn open(pool: &SqlitePool) -> Result<Vec<Watch>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, Option<i64>, String, String, i64, Option<i64>, i64)> = sqlx::query_as(
        "SELECT id, chat_id, looking_for, instruction, created_at, expires_at, checked_up_to \
         FROM mail_watches WHERE status = 'open' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("listing watches")?;
    Ok(rows
        .into_iter()
        .map(
            |(id, chat_id, looking_for, instruction, created_at, expires_at, checked_up_to)| {
                Watch {
                    id,
                    chat_id,
                    looking_for,
                    instruction,
                    created_at,
                    expires_at,
                    checked_up_to,
                }
            },
        )
        .collect())
}

/// Close the watches past their time, telling their chats. Returns how many.
pub async fn expire(pool: &SqlitePool) -> Result<usize> {
    let now = Utc::now().timestamp();
    let rows: Vec<(i64, Option<i64>, String)> = sqlx::query_as(
        "UPDATE mail_watches SET status = 'expired', closed_at = ?1 \
         WHERE status = 'open' AND expires_at IS NOT NULL AND expires_at < ?1 \
         RETURNING id, chat_id, looking_for",
    )
    .bind(now)
    .fetch_all(pool)
    .await
    .context("expiring watches")?;
    for (_, chat_id, looking_for) in &rows {
        if let Some(chat_id) = chat_id {
            note(
                pool,
                *chat_id,
                &format!("I stopped watching for {looking_for}: it hadn't arrived in time."),
            )
            .await?;
        }
    }
    Ok(rows.len())
}

/// A new email as matching sees it.
struct Candidate {
    id: i64,
    external_id: String,
    line: String,
}

/// Check the mail that arrived since the open watches last looked, and act
/// on the matches. Bodies needn't be downloaded: a match is (see
/// `orchestrator::complete_messages`). Returns how many to-dos were recorded.
pub async fn check(pool: &SqlitePool, llm: &dyn LlmTransport, char_budget: usize) -> Result<usize> {
    let watches = open(pool).await?;
    let Some(after) = watches.iter().map(|w| w.checked_up_to).min() else {
        return Ok(0);
    };
    let (candidates, up_to) = new_mail(pool, after).await?;
    let mut recorded = 0;
    // Each batch's emails, then the watches' list, within the budget.
    let per_batch = (char_budget / 2 / (SNIPPET_CHARS + 120)).clamp(5, 50);
    for batch in candidates.chunks(per_batch) {
        let watches = open(pool).await?;
        for (watch_id, index) in matches(llm, &watches, batch).await? {
            let (Some(watch), Some(email)) =
                (watches.iter().find(|w| w.id == watch_id), batch.get(index))
            else {
                continue;
            };
            // Mail from before the watch was saved isn't its business.
            if email.id <= watch.checked_up_to || !still_open(pool, watch.id).await? {
                continue;
            }
            recorded += act(pool, llm, watch, email, char_budget).await?;
        }
        let last = batch.last().map(|c| c.id).unwrap_or(after);
        advance(pool, last).await?;
    }
    advance(pool, up_to).await?;
    Ok(recorded)
}

/// The mail after message `after`, one row per email (a copy filed in a
/// second folder counts once), in arrival order, leaving out what the user
/// wrote or threw away (Sent, Drafts, Trash); and the newest row of all.
async fn new_mail(pool: &SqlitePool, after: i64) -> Result<(Vec<Candidate>, i64)> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        bool,
    )> = sqlx::query_as(
        "SELECT m.id, m.external_id, c.external_id, p.display_name, p.external_id, m.subject, \
                    m.body, m.has_body \
             FROM messages m JOIN channels c ON c.id = m.channel_id \
             LEFT JOIN people p ON p.id = m.author_id \
             WHERE m.id > ? ORDER BY m.id",
    )
    .bind(after)
    .fetch_all(pool)
    .await
    .context("loading new mail for watches")?;
    let up_to = rows.iter().map(|r| r.0).max().unwrap_or(after);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (id, external_id, folder, name, address, subject, body, has_body) in rows {
        let folder = crate::source::imap_utf7::decode_mailbox_name(&folder);
        if matches!(
            role(&folder),
            MailboxRole::Sent | MailboxRole::Drafts | MailboxRole::Trash
        ) || !seen.insert(external_id.clone())
        {
            continue;
        }
        let from = match (name, address) {
            (Some(name), Some(address)) => format!("{name} <{address}>"),
            (None, Some(address)) => address,
            (Some(name), None) => name,
            (None, None) => "?".to_string(),
        };
        let opening = if has_body {
            tools::snippet(&body, SNIPPET_CHARS)
        } else {
            String::new()
        };
        out.push(Candidate {
            id,
            external_id,
            line: format!(
                "from {from} — {} — {opening}",
                subject.as_deref().unwrap_or("(no subject)")
            ),
        });
    }
    Ok((out, up_to))
}

/// Which emails of `batch` (by index) the watches wait for.
async fn matches(
    llm: &dyn LlmTransport,
    watches: &[Watch],
    batch: &[Candidate],
) -> Result<Vec<(i64, usize)>> {
    if watches.is_empty() || batch.is_empty() {
        return Ok(Vec::new());
    }
    let mut prompt = String::from("# Waiting for\n");
    for w in watches {
        prompt.push_str(&format!("W-{}: {}\n", w.id, w.looking_for));
    }
    prompt.push_str("\n# New emails\n");
    for (i, c) in batch.iter().enumerate() {
        prompt.push_str(&format!("E-{}: {}\n", i + 1, c.line));
    }
    let history = vec![InputItem::Message {
        role: Role::User,
        content: prompt,
    }];
    let response =
        agent::send_with_stall_retry(llm, MATCH_SYSTEM, &history, &[], 0, None, |_| {}).await?;
    Ok(parse_matches(&text(&response)))
}

/// "W-3 E-2" lines as (watch id, email index from 0).
fn parse_matches(reply: &str) -> Vec<(i64, usize)> {
    let number = |token: &str, prefix: &str| -> Option<i64> {
        token
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .strip_prefix(prefix)?
            .parse()
            .ok()
    };
    let mut out = Vec::new();
    for line in reply.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let watch = tokens.iter().find_map(|t| number(t, "W-"));
        let email = tokens.iter().find_map(|t| number(t, "E-"));
        if let (Some(watch), Some(email)) = (watch, email)
            && email >= 1
        {
            out.push((watch, email as usize - 1));
        }
    }
    out
}

/// Carry out `watch` on `email`. When the run finds it's no match after all,
/// the watch stays open; otherwise it closes, and its chat hears what was
/// done. Returns how many to-dos it recorded.
async fn act(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    watch: &Watch,
    email: &Candidate,
    char_budget: usize,
) -> Result<usize> {
    crate::orchestrator::complete_messages(pool, &[email.id]).await?;
    let source_id: i64 = sqlx::query_scalar(
        "SELECT c.source_id FROM messages m JOIN channels c ON c.id = m.channel_id WHERE m.id = ?",
    )
    .bind(email.id)
    .fetch_one(pool)
    .await?;
    let mut prompt = format!("Today is {}.\n", Local::now().format("%A %Y-%m-%d"));
    prompt.push_str(&format!(
        "\n# Watching for\n{}\n\n# The user's instruction\n{}\n\n# The email\n",
        watch.looking_for, watch.instruction
    ));
    prompt.push_str(
        &super::manual::email(pool, email.id, (char_budget / 2).max(MIN_EMAIL_CHARS)).await?,
    );
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
    let mut acted = false;
    let mut reply = String::new();
    for step in 0..TURNS {
        let response =
            agent::send_with_stall_retry(llm, ACT_SYSTEM, &history, &defs, step, None, |_| {})
                .await?;
        reply = text(&response);
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
        acted = true;
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
    if !acted && reply.trim().to_lowercase().starts_with("no match") {
        return Ok(0);
    }
    sqlx::query(
        "UPDATE mail_watches SET status = 'matched', matched_message_id = ?, closed_at = ? \
         WHERE id = ? AND status = 'open'",
    )
    .bind(email.id)
    .bind(Utc::now().timestamp())
    .bind(watch.id)
    .execute(pool)
    .await
    .context("closing the watch")?;
    if let Some(chat_id) = watch.chat_id {
        let subject: Option<String> =
            sqlx::query_scalar("SELECT subject FROM messages WHERE id = ?")
                .bind(email.id)
                .fetch_one(pool)
                .await?;
        let mut text = format!(
            "The email I was watching for arrived: \u{201c}{}\u{201d} ({}).",
            subject.as_deref().unwrap_or("no subject"),
            email.external_id
        );
        if !reply.trim().is_empty() {
            text.push(' ');
            text.push_str(reply.trim());
        }
        note(pool, chat_id, &text).await?;
    }
    Ok(recorded)
}

async fn still_open(pool: &SqlitePool, id: i64) -> Result<bool> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM mail_watches WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(status.as_deref() == Some("open"))
}

/// The open watches have now seen the mail up to message `up_to`.
async fn advance(pool: &SqlitePool, up_to: i64) -> Result<()> {
    sqlx::query(
        "UPDATE mail_watches SET checked_up_to = ?1 WHERE status = 'open' AND checked_up_to < ?1",
    )
    .bind(up_to)
    .execute(pool)
    .await
    .context("marking the mail checked")?;
    Ok(())
}

/// Tell chat `chat_id` something, as the assistant, and bring it to the top
/// of the chat list.
async fn note(pool: &SqlitePool, chat_id: i64, text: &str) -> Result<()> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM chats WHERE id = ?")
        .bind(chat_id)
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Ok(());
    }
    crate::chat::store::append_turn(pool, chat_id, "assistant", Some(text), None, None, None)
        .await?;
    crate::chat::store::touch_chat(pool, chat_id).await?;
    Ok(())
}

/// The text of a reply, tool calls left out.
pub(crate) fn text(response: &ResponsesResponse) -> String {
    let mut out = String::new();
    for item in &response.output {
        if let OutputItem::Message { content } = item {
            for c in content {
                if let ContentItem::OutputText { text } = c {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

/// Which of `message_ids` are emails a watch matched (any copy of them), so
/// channel extraction leaves them to it.
pub(super) async fn owned(pool: &SqlitePool, message_ids: &[i64]) -> Result<HashSet<i64>> {
    if message_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let owned: Vec<i64> = sqlx::query_scalar(
        "SELECT n.id FROM messages n WHERE n.id IN (SELECT value FROM json_each(?)) \
           AND EXISTS (SELECT 1 FROM mail_watches w JOIN messages m ON m.id = w.matched_message_id \
                       WHERE m.external_id = n.external_id)",
    )
    .bind(serde_json::to_string(message_ids)?)
    .fetch_all(pool)
    .await
    .context("finding mail matched by watches")?;
    Ok(owned.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{MockLlm, MockStep, mock};
    use serde_json::json;

    /// An account with an inbox, Sent and Trash, an email from before any
    /// watch, and a chat.
    async fn setup() -> Result<(tempfile::TempDir, SqlitePool, i64)> {
        let tmp = tempfile::TempDir::new()?;
        let pool = crate::db::open(&tmp.path().join("t.db")).await?;
        crate::db::migrate(&pool).await?;
        sqlx::query(
            "INSERT INTO sources (id, kind, name, config_ref, created_at) VALUES (1, 'imap', 'a', 'r', 0); \
             INSERT INTO channels (id, source_id, external_id, name, kind) VALUES (1, 1, 'INBOX', 'INBOX', 'mailbox'); \
             INSERT INTO channels (id, source_id, external_id, name, kind) VALUES (2, 1, 'Sent', 'Sent', 'mailbox'); \
             INSERT INTO channels (id, source_id, external_id, name, kind) VALUES (3, 1, 'Trash', 'Trash', 'mailbox'); \
             INSERT INTO messages (id, channel_id, external_id, posted_at, subject, body, body_format, ingested_at) \
             VALUES (1, 1, '<old@x>', 100, 'Old', 'An old email.', 'text', 0);",
        )
        .execute(&pool)
        .await?;
        let chat = crate::chat::store::create_chat(&pool, None, None).await?;
        Ok((tmp, pool, chat))
    }

    async fn arrive(pool: &SqlitePool, id: i64, channel: i64, subject: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO messages (id, channel_id, external_id, posted_at, subject, body, body_format, ingested_at) \
             VALUES (?, ?, ?, ?, ?, 'Reply to confirm.', 'text', 0)",
        )
        .bind(id)
        .bind(channel)
        .bind(format!("<m{id}@x>"))
        .bind(100 + id)
        .bind(subject)
        .execute(pool)
        .await?;
        Ok(())
    }

    fn record(evidence: &str) -> ResponsesResponse {
        mock::turn(vec![OutputItem::FunctionCall {
            call_id: "c1".to_string(),
            name: "record_action".to_string(),
            arguments: json!({"title": "Confirm the unsubscription", "details": "Answer Mailman", "confidence": "high",
                              "rationale": "the user asked", "evidence_external_ids": [evidence]})
            .to_string(),
        }])
    }

    async fn status(pool: &SqlitePool, id: i64) -> Result<(String, i64)> {
        Ok(
            sqlx::query_as("SELECT status, checked_up_to FROM mail_watches WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await?,
        )
    }

    async fn notes(pool: &SqlitePool, chat: i64) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT content FROM chat_turns WHERE chat_id = ? AND role = 'assistant' ORDER BY id",
        )
        .bind(chat)
        .fetch_all(pool)
        .await?)
    }

    #[tokio::test]
    async fn an_awaited_email_gets_its_to_do_and_the_chat_hears() -> Result<()> {
        let (_tmp, pool, chat) = setup().await?;
        let id = create(
            &pool,
            Some(chat),
            "the Mailman confirmation",
            "record a to-do to confirm",
            None,
        )
        .await?;
        let expires_at: i64 =
            sqlx::query_scalar("SELECT expires_at FROM mail_watches WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert!(
            (expires_at - Utc::now().timestamp() - 7 * 86_400).abs() < 60,
            "a week by default"
        );

        arrive(&pool, 2, 1, "Weekly news").await?;
        arrive(&pool, 3, 1, "Your confirmation is required").await?;
        let llm = MockLlm::new(vec![
            mock::no_tools(&format!("W-{id} E-2")),
            record("<m3@x>"),
            mock::no_tools("Created a to-do to confirm the unsubscription."),
        ]);
        assert_eq!(check(&pool, &llm, 10_000).await?, 1);
        assert_eq!(status(&pool, id).await?.0, "matched");
        let (title, evidence): (String, String) = sqlx::query_as(
            "SELECT a.title, m.external_id FROM actions a \
             JOIN action_evidence ae ON ae.action_id = a.id JOIN messages m ON m.id = ae.message_id",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            (title.as_str(), evidence.as_str()),
            ("Confirm the unsubscription", "<m3@x>")
        );
        let notes = notes(&pool, chat).await?;
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("Your confirmation is required"),
            "{}",
            notes[0]
        );
        assert!(notes[0].contains("Created a to-do"), "{}", notes[0]);
        // Channel extraction leaves the matched email to the watch.
        assert_eq!(owned(&pool, &[2, 3]).await?, HashSet::from([3]));

        // A closed watch costs no more model calls.
        arrive(&pool, 4, 1, "Another").await?;
        assert_eq!(check(&pool, &MockLlm::new(vec![]), 10_000).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn mail_that_matches_nothing_is_checked_once() -> Result<()> {
        let (_tmp, pool, chat) = setup().await?;
        let id = create(
            &pool,
            Some(chat),
            "the Mailman confirmation",
            "record a to-do",
            None,
        )
        .await?;
        assert_eq!(
            status(&pool, id).await?.1,
            1,
            "mail from before the watch isn't checked"
        );
        arrive(&pool, 2, 1, "Weekly news").await?;
        // Sent and Trash aren't looked at; the inbox email is.
        arrive(&pool, 3, 2, "Re: something").await?;
        arrive(&pool, 4, 3, "Deleted").await?;
        let llm = MockLlm::new(vec![mock::no_tools("none")]);
        assert_eq!(check(&pool, &llm, 10_000).await?, 0);
        assert_eq!(status(&pool, id).await?, ("open".to_string(), 4));
        // Nothing new: no model call.
        check(&pool, &MockLlm::new(vec![]), 10_000).await?;
        assert!(notes(&pool, chat).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_check_is_tried_again() -> Result<()> {
        let (_tmp, pool, _) = setup().await?;
        let id = create(&pool, None, "the confirmation", "record a to-do", None).await?;
        arrive(&pool, 2, 1, "Your confirmation is required").await?;
        let llm = MockLlm::scripted(vec![MockStep::Error("unreachable".to_string())]);
        assert!(check(&pool, &llm, 10_000).await.is_err());
        assert_eq!(status(&pool, id).await?, ("open".to_string(), 1));
        Ok(())
    }

    #[tokio::test]
    async fn a_closer_look_can_find_it_is_not_the_one() -> Result<()> {
        let (_tmp, pool, chat) = setup().await?;
        let id = create(
            &pool,
            Some(chat),
            "the confirmation",
            "record a to-do",
            None,
        )
        .await?;
        arrive(&pool, 2, 1, "Confirm your other list").await?;
        let llm = MockLlm::new(vec![
            mock::no_tools(&format!("W-{id} E-1")),
            mock::no_tools("no match"),
        ]);
        assert_eq!(check(&pool, &llm, 10_000).await?, 0);
        assert_eq!(status(&pool, id).await?, ("open".to_string(), 2));
        assert!(notes(&pool, chat).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn watches_past_their_time_expire_and_say_so() -> Result<()> {
        let (_tmp, pool, chat) = setup().await?;
        let stale = create(
            &pool,
            Some(chat),
            "the confirmation",
            "record a to-do",
            Some(3),
        )
        .await?;
        let forever = create(&pool, Some(chat), "the invoice", "record a to-do", Some(0)).await?;
        sqlx::query("UPDATE mail_watches SET expires_at = 1 WHERE id = ?")
            .bind(stale)
            .execute(&pool)
            .await?;
        assert_eq!(expire(&pool).await?, 1);
        assert_eq!(status(&pool, stale).await?.0, "expired");
        assert_eq!(status(&pool, forever).await?.0, "open");
        let notes = notes(&pool, chat).await?;
        assert!(
            notes[0].contains("stopped watching for the confirmation"),
            "{}",
            notes[0]
        );
        assert!(cancel(&pool, forever).await?);
        assert!(!cancel(&pool, forever).await?, "already closed");
        assert!(open(&pool).await?.is_empty());
        Ok(())
    }

    #[test]
    fn reads_matches_however_they_are_written() {
        assert_eq!(parse_matches("W-3 E-2\nW-4: E-10."), vec![(3, 1), (4, 9)]);
        assert!(parse_matches("none").is_empty());
        assert!(parse_matches("W-3 E-0").is_empty());
    }
}
