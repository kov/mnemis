//! A to-do the user asked for: the user points at an email ("make this a
//! to-do") and one agent call writes it, with the email as its evidence.
//! Unlike extraction there's no deciding whether it's actionable; the user
//! already did.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use sqlx::SqlitePool;

use super::tools::{self, FetchBudget, ToolScope};
use crate::agent;
use crate::llm::{InputItem, LlmTransport, OutputItem, Role};

/// Longest body handed to the model, in characters.
const BODY_CHARS: usize = 12_000;
/// Turns to get a valid `record_action` call (a bad one gets its error back).
const TURNS: usize = 3;

const SYSTEM: &str = "You turn an email into a to-do for the user, who asked \
for one. Call record_action exactly once. The title is a short imperative \
phrase saying what the user has to do (not a summary of the email). Details \
hold what the user needs to act without rereading the email: who, what, \
deadlines, amounts, links. Set due_at (RFC 3339) only when the email gives or \
clearly implies a date. Use confidence \"high\", since the user asked, and \
put the email's id in evidence_external_ids. Write in the email's language.";

/// Ask the model for a to-do from `message_id`; returns the new action's id.
/// The to-do starts claimed: the user asked for it.
pub async fn to_do_from_message(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    message_id: i64,
) -> Result<i64> {
    let email = describe(pool, message_id).await?;
    let mut history = vec![InputItem::Message {
        role: Role::User,
        content: email,
    }];
    let defs: Vec<_> = tools::definitions()
        .into_iter()
        .filter(|d| d.name == "record_action")
        .collect();
    let mut budget = FetchBudget::new(0);
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
            bail!("the model didn't write a to-do");
        }
        for (call_id, name, arguments) in calls {
            let out =
                tools::dispatch(pool, ToolScope::Global, &mut budget, &name, &arguments).await;
            if out.recorded_action {
                let id = recorded_id(&out.output)?;
                claim(pool, id).await?;
                return Ok(id);
            }
            history.push(InputItem::FunctionCallOutput {
                call_id,
                output: out.output,
            });
        }
    }
    bail!("the model didn't write a valid to-do")
}

/// The email as the model sees it, after today's date.
async fn describe(pool: &SqlitePool, message_id: i64) -> Result<String> {
    Ok(format!(
        "Today is {}.\n\n{}",
        Local::now().format("%A %Y-%m-%d"),
        email(pool, message_id, BODY_CHARS).await?
    ))
}

/// An email as the model sees it: headers, then the text, cut at
/// `max_chars`.
pub(super) async fn email(pool: &SqlitePool, message_id: i64, max_chars: usize) -> Result<String> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        i64,
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        String,
        bool,
    )> = sqlx::query_as(
        "SELECT c.source_id, m.external_id, p.display_name, p.external_id, m.posted_at, \
                    m.subject, m.body, m.has_body \
             FROM messages m JOIN channels c ON c.id = m.channel_id \
             LEFT JOIN people p ON p.id = m.author_id WHERE m.id = ?",
    )
    .bind(message_id)
    .fetch_optional(pool)
    .await
    .context("loading the email")?;
    let Some((source_id, external_id, name, address, posted_at, subject, body, has_body)) = row
    else {
        bail!("no message {message_id}");
    };
    let body = if has_body {
        body
    } else {
        // Only the headers were synced; the full message may be cached.
        let mut conn = pool.acquire().await?;
        let raw = crate::mail::cache::get(&mut conn, source_id, &external_id)
            .await?
            .context("the email isn't downloaded yet; open it and try again")?;
        crate::source::imap::imported(&raw, 0, 0, 0, false)
            .map(|m| m.body)
            .unwrap_or_default()
    };
    let body: String = match body.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &body[..cut]),
        None => body,
    };
    let from = match (name, address) {
        (Some(name), Some(address)) => format!("{name} <{address}>"),
        (None, Some(address)) => address,
        (Some(name), None) => name,
        (None, None) => "?".to_string(),
    };
    let date = DateTime::<Utc>::from_timestamp(posted_at, 0)
        .map(|d| d.with_timezone(&Local).to_rfc3339())
        .unwrap_or_default();
    Ok(format!(
        "Email id: {external_id}\nFrom: {from}\nDate: {date}\nSubject: {subject}\n\n{body}",
        subject = subject.unwrap_or_default(),
    ))
}

fn recorded_id(output: &str) -> Result<i64> {
    let value: serde_json::Value = serde_json::from_str(output).context("reading the new to-do")?;
    value["action_id"]
        .as_str()
        .and_then(|id| id.strip_prefix("A-"))
        .and_then(|id| id.parse().ok())
        .context("the new to-do has no id")
}

async fn claim(pool: &SqlitePool, action_id: i64) -> Result<()> {
    sqlx::query("UPDATE actions SET status = 'claimed' WHERE id = ?")
        .bind(action_id)
        .execute(pool)
        .await
        .context("claiming the new to-do")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{MockLlm, mock};

    #[tokio::test]
    async fn writes_a_claimed_to_do_from_the_email() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let pool = crate::db::open(&tmp.path().join("t.db")).await?;
        crate::db::migrate(&pool).await?;
        sqlx::query(
            "INSERT INTO sources (id, kind, name, config_ref, created_at) VALUES (1, 'imap', 'a', 'r', 0); \
             INSERT INTO channels (id, source_id, external_id, name, kind) VALUES (1, 1, 'INBOX', 'INBOX', 'mailbox'); \
             INSERT INTO messages (id, channel_id, external_id, posted_at, subject, body, body_format, ingested_at) \
             VALUES (7, 1, '<m@x>', 0, 'Invoice', 'Please pay by Friday.', 'text', 0);",
        )
        .execute(&pool)
        .await?;
        // A bad call first (unknown evidence): its error goes back, then a
        // good one.
        let llm = MockLlm::new(vec![
            mock::record_action("Pay the invoice", "high", &["<nope@x>"]),
            mock::record_action("Pay the invoice", "high", &["<m@x>"]),
        ]);
        let id = to_do_from_message(&pool, &llm, 7).await?;
        let (title, status, evidence): (String, String, i64) = sqlx::query_as(
            "SELECT a.title, a.status, e.message_id FROM actions a \
             JOIN action_evidence e ON e.action_id = a.id WHERE a.id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            (title.as_str(), status.as_str(), evidence),
            ("Pay the invoice", "claimed", 7)
        );
        Ok(())
    }
}
