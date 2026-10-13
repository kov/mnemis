//! The briefing: what mail came in over the last few days and what of it the
//! user may want to read.
//!
//! Two layers. As mail syncs, [`tag`] describes each new email once (its kind,
//! how much it is worth reading, why), so the briefing's counts and lists for
//! any period are a query over stored tags, and switching periods costs no
//! model call. Less often, [`agent`] runs over a whole period: it reads what
//! deserves it, looks up earlier mail for context, and writes a short summary
//! with the emails to read.
//!
//! Only mail that to-do extraction reads counts: unmuted folders, not Sent,
//! Drafts, Trash or Junk, and not the user's own mail. A copy filed in a
//! second folder counts once.

pub mod agent;
pub mod tag;

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::Utc;
use mnemis_types::{
    AccountCountDto, BriefingDto, BriefingMailDto, BriefingTodoDto, BriefingWatchDto,
    CategoryCountDto, MailCategory, MailboxRole,
};
use sqlx::SqlitePool;

use crate::mail::view::role;

/// The longest period a briefing covers: the reader keeps about 90 days.
pub const MAX_DAYS: i64 = 90;
/// Interest from which an email is worth reading.
const WORTH_READING: i64 = 2;
/// Most emails each list shows.
const LIST_LIMIT: usize = 15;

/// Start of the period of the last `days` days.
pub(crate) fn since(days: i64) -> i64 {
    Utc::now().timestamp() - days.clamp(1, MAX_DAYS) * 86_400
}

/// An email of a period, with its tag when it has one.
#[derive(Debug, Clone)]
pub(crate) struct Mail {
    pub id: i64,
    pub external_id: String,
    pub thread_id: Option<i64>,
    pub account: String,
    pub from: String,
    pub subject: String,
    pub received_at: i64,
    pub unread: bool,
    pub category: Option<MailCategory>,
    pub interest: Option<i64>,
    pub reason: String,
}

impl Mail {
    fn dto(&self) -> BriefingMailDto {
        BriefingMailDto {
            message_id: self.id,
            thread_id: self.thread_id,
            from: self.from.clone(),
            subject: self.subject.clone(),
            received_at: self.received_at,
            unread: self.unread,
            category: self.category,
            interest: self.interest,
            reason: self.reason.clone(),
            warnings: Vec::new(),
        }
    }
}

const SELECT_MAIL: &str = "SELECT m.id, m.external_id, m.thread_id, s.name, p.display_name, \
            p.external_id, m.subject, COALESCE(m.received_at, m.posted_at), m.flags, \
            m.removed_at IS NOT NULL, c.external_id, t.category, t.interest, t.reason \
     FROM messages m JOIN channels c ON c.id = m.channel_id JOIN sources s ON s.id = c.source_id \
     LEFT JOIN people p ON p.id = m.author_id LEFT JOIN mail_tags t ON t.message_id = m.id";

#[allow(clippy::type_complexity)]
type MailRow = (
    i64,
    String,
    Option<i64>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
    bool,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
);

/// One row as a [`Mail`], with its folder and sender address.
fn mail(row: MailRow) -> (Mail, String, Option<String>) {
    let (
        id,
        external_id,
        thread_id,
        account,
        name,
        address,
        subject,
        received_at,
        flags,
        removed,
        folder,
        category,
        interest,
        reason,
    ) = row;
    let from = match (name, address.as_deref()) {
        (Some(name), _) if !name.trim().is_empty() => name,
        (_, Some(address)) => address.to_string(),
        _ => "?".to_string(),
    };
    (
        Mail {
            id,
            external_id,
            thread_id,
            account,
            from,
            subject: subject
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "(no subject)".to_string()),
            received_at,
            unread: !removed && flags & 1 == 0,
            category: category.as_deref().and_then(MailCategory::parse),
            interest,
            reason: reason.unwrap_or_default(),
        },
        crate::source::imap_utf7::decode_mailbox_name(&folder),
        address,
    )
}

/// The mail received since `since`, newest first.
pub(crate) async fn period_mail(pool: &SqlitePool, since: i64) -> Result<Vec<Mail>> {
    let rows: Vec<MailRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT_MAIL} WHERE c.kind = 'mailbox' AND c.muted = 0 AND m.reader_only = 0 \
           AND COALESCE(m.received_at, m.posted_at) >= ? \
         ORDER BY COALESCE(m.received_at, m.posted_at) DESC, m.id DESC"
    )))
    .bind(since)
    .fetch_all(pool)
    .await
    .context("loading the period's mail")?;
    let own: HashSet<String> = crate::mail::own_addresses(pool)
        .await?
        .into_iter()
        .collect();
    let mut out: Vec<Mail> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let (m, folder, address) = mail(row);
        if matches!(
            role(&folder),
            MailboxRole::Sent | MailboxRole::Drafts | MailboxRole::Trash | MailboxRole::Junk
        ) || address.is_some_and(|a| own.contains(&a.to_lowercase()))
        {
            continue;
        }
        match index.get(&m.external_id) {
            Some(&i) => {
                // Copies: read if any copy is, tagged if any copy is.
                let kept = &mut out[i];
                kept.unread &= m.unread;
                if kept.category.is_none() && m.category.is_some() {
                    kept.category = m.category;
                    kept.interest = m.interest;
                    kept.reason = m.reason;
                }
            }
            None => {
                index.insert(m.external_id.clone(), out.len());
                out.push(m);
            }
        }
    }
    Ok(out)
}

/// The email with Message-ID `external_id`, wherever it is filed.
pub(crate) async fn mail_by_external_id(
    pool: &SqlitePool,
    external_id: &str,
) -> Result<Option<Mail>> {
    let rows: Vec<MailRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT_MAIL} WHERE m.external_id = ? ORDER BY m.removed_at IS NOT NULL, m.id"
    )))
    .bind(external_id)
    .fetch_all(pool)
    .await
    .context("loading an email")?;
    Ok(rows.into_iter().next().map(|r| mail(r).0))
}

async fn mail_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Mail>> {
    let external_id: Option<String> =
        sqlx::query_scalar("SELECT external_id FROM messages WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    match external_id {
        Some(e) => mail_by_external_id(pool, &e).await,
        None => Ok(None),
    }
}

/// The briefing for the last `days` days (1 to [`MAX_DAYS`]).
pub async fn load(pool: &SqlitePool, days: i64) -> Result<BriefingDto> {
    let days = days.clamp(1, MAX_DAYS);
    let since = since(days);
    let mail = period_mail(pool, since).await?;

    let mut categories: HashMap<MailCategory, i64> = HashMap::new();
    let mut accounts: HashMap<String, i64> = HashMap::new();
    let mut untagged = 0;
    for m in &mail {
        match m.category {
            Some(c) => *categories.entry(c).or_default() += 1,
            None => untagged += 1,
        }
        *accounts.entry(m.account.clone()).or_default() += 1;
    }
    let mut categories: Vec<CategoryCountDto> = categories
        .into_iter()
        .map(|(category, count)| CategoryCountDto { category, count })
        .collect();
    categories.sort_by_key(|c| (-c.count, c.category.as_str()));
    let mut accounts: Vec<AccountCountDto> = accounts
        .into_iter()
        .map(|(account, count)| AccountCountDto { account, count })
        .collect();
    accounts.sort_by(|a, b| b.count.cmp(&a.count).then(a.account.cmp(&b.account)));

    let mut worth: Vec<&Mail> = mail
        .iter()
        .filter(|m| m.interest.unwrap_or(0) >= WORTH_READING)
        .collect();
    worth.sort_by_key(|m| (-m.interest.unwrap_or(0), -m.received_at));
    let worth_reading = worth.into_iter().take(LIST_LIMIT).map(Mail::dto).collect();

    let ids: Vec<i64> = mail.iter().map(|m| m.id).collect();
    let own = crate::mail::own_addresses(pool).await?;
    let mut warnings = crate::mail::suspicion::for_messages(pool, &ids, &own).await?;
    let suspicious = mail
        .iter()
        .filter_map(|m| {
            let w = warnings.remove(&m.id)?;
            Some(BriefingMailDto {
                warnings: w,
                ..m.dto()
            })
        })
        .take(LIST_LIMIT)
        .collect();

    Ok(BriefingDto {
        days,
        since,
        received: mail.len() as i64,
        unread: mail.iter().filter(|m| m.unread).count() as i64,
        categories,
        untagged,
        accounts,
        worth_reading,
        watches: watches(pool, since).await?,
        new_todos: new_todos(pool, since).await?,
        resolved_todos: sqlx::query_scalar(
            "SELECT COUNT(*) FROM actions WHERE resolved_at >= ? \
               AND status NOT IN ('pending', 'auto_claimed', 'claimed')",
        )
        .bind(since)
        .fetch_one(pool)
        .await
        .context("counting resolved to-dos")?,
        suspicious,
        written: agent::stored(pool, days, &mail).await?,
    })
}

/// The open watches, then those that matched since `since`.
async fn watches(pool: &SqlitePool, since: i64) -> Result<Vec<BriefingWatchDto>> {
    let mut out: Vec<BriefingWatchDto> = crate::extract::expected::open(pool)
        .await?
        .into_iter()
        .map(|w| BriefingWatchDto {
            watch_id: w.id,
            looking_for: w.looking_for,
            open: true,
            expires_at: w.expires_at,
            matched: None,
        })
        .collect();
    let matched: Vec<(i64, String, Option<i64>)> = sqlx::query_as(
        "SELECT id, looking_for, matched_message_id FROM mail_watches \
         WHERE status = 'matched' AND closed_at >= ? ORDER BY closed_at DESC",
    )
    .bind(since)
    .fetch_all(pool)
    .await
    .context("listing matched watches")?;
    for (watch_id, looking_for, message_id) in matched {
        let matched = match message_id {
            Some(id) => mail_by_id(pool, id).await?.map(|m| m.dto()),
            None => None,
        };
        out.push(BriefingWatchDto {
            watch_id,
            looking_for,
            open: false,
            expires_at: None,
            matched,
        });
    }
    Ok(out)
}

/// Open to-dos found since `since`, soonest due first.
async fn new_todos(pool: &SqlitePool, since: i64) -> Result<Vec<BriefingTodoDto>> {
    let rows: Vec<(i64, String, Option<i64>)> = sqlx::query_as(
        "SELECT id, title, due_at FROM actions \
         WHERE extracted_at >= ? AND status IN ('pending', 'auto_claimed', 'claimed') \
         ORDER BY due_at IS NULL, due_at, extracted_at DESC LIMIT ?",
    )
    .bind(since)
    .bind(LIST_LIMIT as i64)
    .fetch_all(pool)
    .await
    .context("listing new to-dos")?;
    Ok(rows
        .into_iter()
        .map(|(action_id, title, due_at)| BriefingTodoDto {
            action_id,
            title,
            due_at,
        })
        .collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An account with an inbox, a muted folder and Sent; the user is
    /// me@x.
    pub(crate) async fn setup() -> Result<(tempfile::TempDir, SqlitePool)> {
        let tmp = tempfile::TempDir::new()?;
        let pool = crate::db::open(&tmp.path().join("t.db")).await?;
        crate::db::migrate(&pool).await?;
        sqlx::query(
            "INSERT INTO sources (id, kind, name, config_ref, created_at) VALUES (1, 'imap', 'Fastmail', 'r', 0); \
             INSERT INTO channels (id, source_id, external_id, name, kind) VALUES (1, 1, 'INBOX', 'INBOX', 'mailbox'); \
             INSERT INTO channels (id, source_id, external_id, name, kind, muted) VALUES (2, 1, 'Promos', 'Promos', 'mailbox', 1); \
             INSERT INTO channels (id, source_id, external_id, name, kind) VALUES (3, 1, 'Sent', 'Sent', 'mailbox'); \
             INSERT INTO people (id, source_id, external_id, display_name) VALUES (1, 1, 'ana@x', 'Ana'); \
             INSERT INTO people (id, source_id, external_id, display_name) VALUES (2, 1, 'news@shop', 'Shop'); \
             INSERT INTO people (id, source_id, external_id, display_name) VALUES (3, 1, 'me@x', 'Me'); \
             INSERT INTO contacts (id, display_name, relationship, created_at, updated_at) VALUES (1, 'Me', 'self', 0, 0); \
             INSERT INTO contact_identifiers (contact_id, kind, value) VALUES (1, 'email', 'me@x');",
        )
        .execute(&pool)
        .await?;
        Ok((tmp, pool))
    }

    /// An email `hours` ago.
    pub(crate) async fn arrive(
        pool: &SqlitePool,
        id: i64,
        channel: i64,
        author: i64,
        subject: &str,
        hours: i64,
    ) -> Result<()> {
        let at = Utc::now().timestamp() - hours * 3_600;
        sqlx::query(
            "INSERT INTO messages (id, channel_id, external_id, author_id, posted_at, received_at, \
                                   subject, body, body_format, ingested_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'Hello there.', 'text', 0)",
        )
        .bind(id)
        .bind(channel)
        .bind(format!("<m{id}@x>"))
        .bind(author)
        .bind(at)
        .bind(at)
        .bind(subject)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn tag(
        pool: &SqlitePool,
        id: i64,
        category: &str,
        interest: i64,
        reason: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO mail_tags (message_id, category, interest, reason, tagged_at) \
             VALUES (?, ?, ?, ?, 0)",
        )
        .bind(id)
        .bind(category)
        .bind(interest)
        .bind(reason)
        .execute(pool)
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn sums_up_the_period() -> Result<()> {
        let (_tmp, pool) = setup().await?;
        arrive(&pool, 1, 1, 1, "Dinner on Friday?", 2).await?;
        tag(&pool, 1, "person", 3, "Ana invites you to dinner").await?;
        arrive(&pool, 2, 1, 2, "50% off", 5).await?;
        tag(&pool, 2, "newsletter", 0, "a sale").await?;
        arrive(&pool, 3, 1, 2, "Your order shipped", 30).await?;
        tag(&pool, 3, "transactional", 2, "the lamp ships").await?;
        // Untagged yet, a muted folder, the user's own mail, a week ago.
        arrive(&pool, 4, 1, 1, "Re: dinner", 1).await?;
        arrive(&pool, 5, 2, 2, "Promo", 1).await?;
        arrive(&pool, 6, 3, 3, "My reply", 1).await?;
        arrive(&pool, 7, 1, 1, "Old news", 24 * 8).await?;
        sqlx::query("UPDATE messages SET flags = 1 WHERE id = 2")
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO actions (id, title, confidence, status, extracted_at) \
             VALUES (1, 'Answer Ana', 'high', 'claimed', ?)",
        )
        .bind(Utc::now().timestamp() - 3_600)
        .execute(&pool)
        .await?;

        let day = load(&pool, 1).await?;
        assert_eq!((day.received, day.unread, day.untagged), (3, 2, 1));
        let subjects: Vec<&str> = day
            .worth_reading
            .iter()
            .map(|m| m.subject.as_str())
            .collect();
        assert_eq!(subjects, ["Dinner on Friday?"]);
        assert_eq!(day.accounts[0].account, "Fastmail");
        assert_eq!(day.new_todos[0].title, "Answer Ana");
        assert!(day.written.is_none());

        let three = load(&pool, 3).await?;
        assert_eq!(three.received, 4);
        let subjects: Vec<&str> = three
            .worth_reading
            .iter()
            .map(|m| m.subject.as_str())
            .collect();
        assert_eq!(
            subjects,
            ["Dinner on Friday?", "Your order shipped"],
            "most important first"
        );
        assert!(
            three
                .categories
                .iter()
                .any(|c| c.category == MailCategory::Newsletter && c.count == 1)
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_copy_in_another_folder_counts_once() -> Result<()> {
        let (_tmp, pool) = setup().await?;
        sqlx::query(
            "INSERT INTO channels (id, source_id, external_id, name, kind) \
             VALUES (4, 1, 'Archive', 'Archive', 'mailbox')",
        )
        .execute(&pool)
        .await?;
        arrive(&pool, 1, 1, 1, "Dinner", 1).await?;
        sqlx::query(
            "INSERT INTO messages (id, channel_id, external_id, author_id, posted_at, \
                                   subject, body, body_format, ingested_at, flags) \
             SELECT 2, 4, external_id, author_id, posted_at, subject, body, body_format, 0, 1 \
             FROM messages WHERE id = 1",
        )
        .execute(&pool)
        .await?;
        tag(&pool, 2, "person", 2, "dinner").await?;
        let day = load(&pool, 1).await?;
        assert_eq!((day.received, day.unread, day.untagged), (1, 0, 0));
        assert_eq!(day.worth_reading.len(), 1);
        Ok(())
    }
}
