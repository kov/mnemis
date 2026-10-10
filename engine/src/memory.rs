//! What the user has taught the model: one note of rules, kept in
//! `memory_notes`, that both the extraction and chat prompts carry.
//!
//! The model maintains it. Each piece of feedback that comes with a reason (a
//! dismissal, a wrong auto-claim) is merged in by a tool-less LLM call that
//! rewrites the whole note: the reason generalised to the pattern it points at,
//! overlapping rules folded together, contradicted ones dropped. The user can
//! read and edit the note directly; their edit is simply the next version.
//! `dismissal_feedback` stays as the raw log the rules were learned from.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use mnemis_types::FeedbackKind;
use sqlx::SqlitePool;

use crate::agent;
use crate::llm::{ContentItem, InputItem, LlmTransport, OutputItem, Role};

/// `memory_notes.key` of the rules note.
pub const RULES_KEY: &str = "rules";

/// Merges rewrite the whole note, so two at once would lose one's lesson.
static MERGE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The current rules, `None` until there are any.
pub async fn rules(pool: &SqlitePool) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT content FROM memory_notes WHERE key = ?")
        .bind(RULES_KEY)
        .fetch_optional(pool)
        .await
        .context("loading memory rules")?;
    Ok(row
        .map(|(c,)| c.trim().to_string())
        .filter(|c| !c.is_empty()))
}

/// Replace the rules (a user edit, or a merge's result). Empty clears them.
pub async fn save_rules(pool: &SqlitePool, content: &str) -> Result<()> {
    let content = content.trim();
    if content.is_empty() {
        sqlx::query("DELETE FROM memory_notes WHERE key = ?")
            .bind(RULES_KEY)
            .execute(pool)
            .await
            .context("clearing memory rules")?;
        return Ok(());
    }
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO memory_notes (key, content, created_at, updated_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
    )
    .bind(RULES_KEY)
    .bind(content)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .context("saving memory rules")?;
    Ok(())
}

const MERGE_SYSTEM: &str = "You maintain a short list of rules that tell an assistant which \
    of the user's emails and messages are to-dos for them and which are not. The rules are \
    learned from the user's feedback on items the assistant got wrong.\n\n\
    You are given the current rules and new feedback. Rewrite the rules so they include what the \
    feedback teaches:\n\
    - Generalise to the pattern the user's reason points at (a sender, a domain, a kind of \
      message, a topic), not the one item. Name senders, domains and subjects concretely so the \
      rule can be applied.\n\
    - Fold the lesson into an existing rule when they overlap; drop or amend a rule the feedback \
      contradicts — newer feedback wins.\n\
    - Keep every existing rule the feedback doesn't touch, word for word.\n\
    - One rule per line, each starting with \"- \". At most 30 rules.\n\n\
    Reply with the complete updated list and nothing else.";

/// Merge feedback about `action_id` into the rules and return the new rules.
/// Call it after [`crate::mutations::record_dismissal_feedback`], only when the
/// user gave a reason. The first merge also folds in earlier reasons from
/// `dismissal_feedback`, so feedback given before the rules existed counts.
pub async fn learn_from_feedback(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    action_id: i64,
    kind: FeedbackKind,
    reason: &str,
) -> Result<String> {
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("no reason to learn from");
    }
    let _serialized = MERGE_LOCK.lock().await;

    let item = describe_action(pool, action_id).await?;
    let current = rules(pool).await?;
    let earlier = match current {
        Some(_) => Vec::new(),
        None => earlier_reasons(pool, action_id).await?,
    };

    let request = merge_request(current.as_deref(), &earlier, kind, &item, reason);
    let history = vec![InputItem::Message {
        role: Role::User,
        content: request,
    }];
    let response =
        agent::send_with_stall_retry(llm, MERGE_SYSTEM, &history, &[], 0, None, |_| {}).await?;
    let mut text = String::new();
    for item in response.output {
        if let OutputItem::Message { content } = item {
            for c in content {
                if let ContentItem::OutputText { text: t } = c {
                    text.push_str(&t);
                }
            }
        }
    }
    let merged = clean_rules(&text);
    if merged.is_empty() {
        // Never let a bad reply wipe what was learned so far.
        bail!("the model returned no rules");
    }
    save_rules(pool, &merged).await?;
    Ok(merged)
}

fn merge_request(
    current: Option<&str>,
    earlier: &[(String, String)],
    kind: FeedbackKind,
    item: &str,
    reason: &str,
) -> String {
    let mut out = String::from("# Current rules\n");
    out.push_str(current.unwrap_or("(none yet)"));
    out.push_str("\n\n");
    if !earlier.is_empty() {
        out.push_str("# Earlier feedback, not yet in the rules\n");
        for (example, why) in earlier {
            out.push_str(&format!(
                "- Item: {}\n  Reason: {}\n",
                one_line(example),
                one_line(why)
            ));
        }
        out.push('\n');
    }
    out.push_str("# New feedback\n");
    out.push_str(match kind {
        FeedbackKind::Dismissed => {
            "The assistant surfaced this as a to-do; the user dismissed it as not one.\n"
        }
        FeedbackKind::WrongAutoClaim => {
            "The assistant auto-claimed this for the user; the user says it shouldn't have.\n"
        }
    });
    out.push_str(&format!("Item: {item}\nUser's reason: {reason}\n"));
    out
}

/// The action as the merge sees it: title, details, and where it came from.
async fn describe_action(pool: &SqlitePool, action_id: i64) -> Result<String> {
    let (title, details): (String, Option<String>) =
        sqlx::query_as("SELECT title, details FROM actions WHERE id = ?")
            .bind(action_id)
            .fetch_optional(pool)
            .await?
            .with_context(|| format!("action {action_id} not found"))?;
    let source: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT p.display_name, p.external_id, m.subject \
         FROM action_evidence ae JOIN messages m ON m.id = ae.message_id \
         LEFT JOIN people p ON p.id = m.author_id \
         WHERE ae.action_id = ? ORDER BY ae.is_primary DESC, ae.message_id LIMIT 1",
    )
    .bind(action_id)
    .fetch_optional(pool)
    .await?;
    let mut out = one_line(&title);
    if let Some(d) = details.filter(|d| !d.trim().is_empty()) {
        out.push_str(&format!(" — {}", one_line(&d)));
    }
    if let Some((name, addr, subject)) = source {
        let from = match (name, addr) {
            (Some(n), Some(a)) => format!("{n} <{a}>"),
            (Some(n), None) => n,
            (None, Some(a)) => a,
            (None, None) => String::new(),
        };
        if !from.is_empty() {
            out.push_str(&format!(" (from {from}"));
            if let Some(s) = subject {
                out.push_str(&format!(", subject \"{}\"", one_line(&s)));
            }
            out.push(')');
        }
    }
    Ok(out)
}

/// Reasons given before the rules existed (newest first, capped), excluding
/// the feedback being merged now.
async fn earlier_reasons(pool: &SqlitePool, action_id: i64) -> Result<Vec<(String, String)>> {
    let current_example: Option<(String,)> =
        sqlx::query_as("SELECT title FROM actions WHERE id = ?")
            .bind(action_id)
            .fetch_optional(pool)
            .await?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT example_text, reason FROM dismissal_feedback \
         WHERE TRIM(reason) != '' ORDER BY created_at DESC LIMIT 20",
    )
    .fetch_all(pool)
    .await
    .context("loading earlier feedback")?;
    Ok(rows
        .into_iter()
        .filter(|(example, _)| {
            current_example
                .as_ref()
                .is_none_or(|(title,)| !example.starts_with(title.as_str()))
        })
        .collect())
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The model's reply as a rules list: code fences and blank lines dropped.
fn clean_rules(raw: &str) -> String {
    raw.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::llm::{ResponsesResponse, ToolDef};
    use async_trait::async_trait;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Replies with a fixed text and records the request it was sent.
    struct Scripted {
        reply: String,
        seen: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(reply: &str) -> Self {
            Scripted {
                reply: reply.to_string(),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmTransport for Scripted {
        async fn send(
            &self,
            _instructions: &str,
            input: Vec<InputItem>,
            _tools: &[ToolDef],
            _previous_response_id: Option<&str>,
        ) -> Result<ResponsesResponse> {
            for i in &input {
                if let InputItem::Message { content, .. } = i {
                    self.seen.lock().unwrap().push(content.clone());
                }
            }
            Ok(ResponsesResponse {
                id: "r".into(),
                status: "completed".into(),
                output: vec![OutputItem::Message {
                    content: vec![ContentItem::OutputText {
                        text: self.reply.clone(),
                    }],
                }],
            })
        }
    }

    async fn pool_with_action() -> Result<(TempDir, SqlitePool, i64)> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO actions (title, details, confidence, status, rationale, extracted_at) \
             VALUES ('Renew the newsletter', 'Weekly digest asks to renew', 'medium', \
             'dismissed', 'r', 0) RETURNING id",
        )
        .fetch_one(&pool)
        .await?;
        Ok((tmp, pool, id))
    }

    #[tokio::test]
    async fn merging_feedback_saves_the_models_rules() -> Result<()> {
        let (_tmp, pool, id) = pool_with_action().await?;
        sqlx::query(
            "INSERT INTO dismissal_feedback (scope_kind, example_text, reason, created_at, kind) \
             VALUES ('global', 'Pay the gym', 'I cancelled that membership', 0, 'dismissed')",
        )
        .execute(&pool)
        .await?;
        let llm = Scripted::new("```\n- Newsletters are never action items\n\n```");

        let merged = learn_from_feedback(
            &pool,
            &llm,
            id,
            FeedbackKind::Dismissed,
            "newsletters never are",
        )
        .await?;

        assert_eq!(merged, "- Newsletters are never action items");
        assert_eq!(rules(&pool).await?.as_deref(), Some(merged.as_str()));
        let request = llm.seen.lock().unwrap().join("\n");
        assert!(request.contains("Renew the newsletter"), "{request}");
        assert!(request.contains("newsletters never are"), "{request}");
        assert!(
            request.contains("I cancelled that membership"),
            "the first merge folds in earlier reasons: {request}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn later_merges_send_the_current_rules_and_skip_old_feedback() -> Result<()> {
        let (_tmp, pool, id) = pool_with_action().await?;
        save_rules(&pool, "- Existing rule").await?;
        sqlx::query(
            "INSERT INTO dismissal_feedback (scope_kind, example_text, reason, created_at, kind) \
             VALUES ('global', 'Pay the gym', 'I cancelled that membership', 0, 'dismissed')",
        )
        .execute(&pool)
        .await?;
        let llm = Scripted::new("- Existing rule\n- New rule");

        learn_from_feedback(&pool, &llm, id, FeedbackKind::Dismissed, "because").await?;

        let request = llm.seen.lock().unwrap().join("\n");
        assert!(request.contains("- Existing rule"), "{request}");
        assert!(!request.contains("cancelled that membership"), "{request}");
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_reply_keeps_the_rules() -> Result<()> {
        let (_tmp, pool, id) = pool_with_action().await?;
        save_rules(&pool, "- Keep me").await?;
        let llm = Scripted::new("  \n```\n```");

        assert!(
            learn_from_feedback(&pool, &llm, id, FeedbackKind::Dismissed, "x")
                .await
                .is_err()
        );
        assert_eq!(rules(&pool).await?.as_deref(), Some("- Keep me"));
        Ok(())
    }

    #[tokio::test]
    async fn saving_empty_rules_clears_them() -> Result<()> {
        let (_tmp, pool, _) = pool_with_action().await?;
        save_rules(&pool, "- A rule").await?;
        save_rules(&pool, "  ").await?;
        assert_eq!(rules(&pool).await?, None);
        Ok(())
    }
}
