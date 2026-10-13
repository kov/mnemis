//! Describing new mail as it arrives: each email gets a category, an interest
//! level and a short reason, once, from a tool-less batch call that reads the
//! sender, subject and opening. The model judges interest by what the user
//! told it they care about (the interests note, their profile), the mail
//! they are waiting for (open watches) and the rules learned from feedback.
//!
//! An email the reply leaves out is stored as `other`, interest 0, so a
//! model that skips lines can't make the same mail be sent again on every
//! sync; a failed call stores nothing, and the next sync tries again.

use anyhow::{Context, Result};
use chrono::Utc;
use mnemis_types::MailCategory;
use sqlx::SqlitePool;

use super::Mail;
use crate::agent;
use crate::extract::expected::text;
use crate::extract::tools::snippet;
use crate::llm::{InputItem, LlmTransport, Role};

/// How far back untagged mail is picked up: the first sync tags the last
/// week, so the briefing is useful at once.
const DAYS: i64 = 7;
/// Characters of each email's opening the model sees.
const SNIPPET_CHARS: usize = 300;

const SYSTEM: &str = "You sort the user's new emails so their briefing can tell \
them what came in and what is worth reading. For every email E-n reply with one \
line, \"E-n | category | interest | reason\", and nothing else.\n\
category: person (a person writing to the user), newsletter, notification \
(automated alerts, service and social updates), transactional (receipts, bills, \
orders, bookings, codes), list (mailing-list discussion), other.\n\
interest: 0 noise, 1 worth a glance, 2 worth reading, 3 important. Judge it by \
what the user cares about (below), whether a real person wrote to them, whether \
they are waiting for it, and whether it changes something for them (money, \
dates, accounts, people they know).\n\
reason: a few words saying what it is and why it matters, written for the user \
(\"Ana asks to move Friday's dinner\", \"weekly deals, nothing you follow\").\n\
An email marked SUSPICIOUS failed checks that point to phishing: don't rate it \
by what it claims, and start its reason with \"possible phishing\".";

/// Describe the mail that arrived lately and has no description yet. Returns
/// how many emails were described.
pub async fn tag_new_mail(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    char_budget: usize,
) -> Result<usize> {
    let mut untagged: Vec<Mail> = super::period_mail(pool, super::since(DAYS))
        .await?
        .into_iter()
        .filter(|m| m.category.is_none())
        .collect();
    if untagged.is_empty() {
        return Ok(0);
    }
    untagged.reverse();
    let context = context(pool).await?;
    let per_batch = (char_budget / 2 / (SNIPPET_CHARS + 160)).clamp(5, 40);
    let mut tagged = 0;
    for batch in untagged.chunks(per_batch) {
        let mut prompt = context.clone();
        prompt.push_str("\n# New emails\n");
        let ids: Vec<i64> = batch.iter().map(|m| m.id).collect();
        let warnings = super::warnings(pool, &ids).await?;
        for (i, m) in batch.iter().enumerate() {
            prompt.push_str(&format!(
                "E-{}: from {} — {} — {}",
                i + 1,
                m.from,
                m.subject,
                opening(pool, m.id).await?
            ));
            if let Some(w) = warnings.get(&m.id) {
                prompt.push_str(&format!(" — SUSPICIOUS: {}", w.join("; ")));
            }
            prompt.push('\n');
        }
        let history = vec![InputItem::Message {
            role: Role::User,
            content: prompt,
        }];
        let response =
            agent::send_with_stall_retry(llm, SYSTEM, &history, &[], 0, None, |_| {}).await?;
        let mut tags: Vec<Option<(MailCategory, i64, String)>> = vec![None; batch.len()];
        for (index, tag) in parse(&text(&response)) {
            if let Some(slot) = tags.get_mut(index) {
                *slot = Some(tag);
            }
        }
        let now = Utc::now().timestamp();
        for (m, tag) in batch.iter().zip(tags) {
            let (category, interest, reason) =
                tag.unwrap_or((MailCategory::Other, 0, String::new()));
            sqlx::query(
                "INSERT OR REPLACE INTO mail_tags (message_id, category, interest, reason, tagged_at) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(m.id)
            .bind(category.as_str())
            .bind(interest)
            .bind(reason)
            .bind(now)
            .execute(pool)
            .await
            .context("saving mail tags")?;
        }
        tagged += batch.len();
    }
    Ok(tagged)
}

/// What the model judges interest by.
async fn context(pool: &SqlitePool) -> Result<String> {
    let profile = crate::settings::get_user_profile(pool).await?;
    let mut out = format!("# The user\n{}\n", profile.display_name);
    if let Some(cp) = profile.custom_prompt.filter(|c| !c.trim().is_empty()) {
        out.push_str(&format!("\nAbout them:\n{}\n", cp.trim()));
    }
    if let Some(interests) = crate::memory::interests(pool).await? {
        out.push_str(&format!(
            "\nWhat they want to hear about in their mail:\n{interests}\n"
        ));
    }
    let watches = crate::extract::expected::open(pool).await?;
    if !watches.is_empty() {
        out.push_str("\nEmails they are waiting for:\n");
        for w in watches {
            out.push_str(&format!("- {}\n", w.looking_for));
        }
    }
    if let Some(rules) = crate::memory::rules(pool).await? {
        out.push_str(&format!(
            "\nWhat they taught you about which mail needs doing something:\n{rules}\n"
        ));
    }
    Ok(out)
}

async fn opening(pool: &SqlitePool, id: i64) -> Result<String> {
    let (body, has_body): (String, bool) =
        sqlx::query_as("SELECT body, has_body FROM messages WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await?;
    Ok(if has_body {
        snippet(&body, SNIPPET_CHARS)
    } else {
        String::new()
    })
}

/// "E-n | category | interest | reason" lines as (email index from 0, tag).
fn parse(reply: &str) -> Vec<(usize, (MailCategory, i64, String))> {
    let mut out = Vec::new();
    for line in reply.lines() {
        let mut fields = line.split('|').map(str::trim);
        let Some(index) = fields.next().and_then(|f| {
            f.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                .strip_prefix("E-")?
                .parse::<usize>()
                .ok()
                .filter(|n| *n >= 1)
        }) else {
            continue;
        };
        let category = fields
            .next()
            .map(|f| f.to_lowercase())
            .and_then(|f| MailCategory::parse(&f))
            .unwrap_or(MailCategory::Other);
        let interest = fields
            .next()
            .and_then(|f| f.chars().find(char::is_ascii_digit))
            .and_then(|c| c.to_digit(10))
            .map_or(0, |d| i64::from(d.min(3)));
        let reason = fields.collect::<Vec<_>>().join(" | ");
        out.push((index - 1, (category, interest, reason)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::{arrive, setup};
    use super::*;
    use crate::test_util::{MockLlm, MockStep, mock};

    async fn tags(pool: &SqlitePool) -> Result<Vec<(i64, String, i64, String)>> {
        Ok(sqlx::query_as(
            "SELECT message_id, category, interest, reason FROM mail_tags ORDER BY message_id",
        )
        .fetch_all(pool)
        .await?)
    }

    #[tokio::test]
    async fn new_mail_is_described_once() -> Result<()> {
        let (_tmp, pool) = setup().await?;
        arrive(&pool, 1, 1, 1, "Dinner on Friday?", 3).await?;
        arrive(&pool, 2, 1, 2, "50% off", 2).await?;
        arrive(&pool, 3, 1, 2, "Weekly digest", 1).await?;
        // Not the briefing's: a muted folder, the user's own, too old.
        arrive(&pool, 4, 2, 2, "Promo", 1).await?;
        arrive(&pool, 5, 3, 3, "My reply", 1).await?;
        arrive(&pool, 6, 1, 1, "Old", 24 * 10).await?;
        // Oldest first; the reply skips the third email.
        let llm = MockLlm::new(vec![mock::no_tools(
            "E-1 | person | 3 | Ana invites you to dinner\nE-2 | Newsletter | interest 0 | a sale",
        )]);
        assert_eq!(tag_new_mail(&pool, &llm, 10_000).await?, 3);
        assert_eq!(
            tags(&pool).await?,
            vec![
                (
                    1,
                    "person".to_string(),
                    3,
                    "Ana invites you to dinner".to_string()
                ),
                (2, "newsletter".to_string(), 0, "a sale".to_string()),
                (3, "other".to_string(), 0, String::new()),
            ]
        );
        // Nothing new: no model call.
        assert_eq!(tag_new_mail(&pool, &MockLlm::new(vec![]), 10_000).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_call_is_tried_again() -> Result<()> {
        let (_tmp, pool) = setup().await?;
        arrive(&pool, 1, 1, 1, "Dinner on Friday?", 3).await?;
        let llm = MockLlm::scripted(vec![MockStep::Error("unreachable".to_string())]);
        assert!(tag_new_mail(&pool, &llm, 10_000).await.is_err());
        assert!(tags(&pool).await?.is_empty());
        Ok(())
    }

    #[test]
    fn reads_tags_however_they_are_written() {
        assert_eq!(
            parse("E-2 | list | 2 | a thread | with a pipe\nnoise\nE-0 | person | 3 | x"),
            vec![(
                1,
                (MailCategory::List, 2, "a thread | with a pipe".to_string())
            )]
        );
        assert_eq!(
            parse("**E-1** | weird | 9 |")[0].1,
            (MailCategory::Other, 3, String::new())
        );
    }
}
