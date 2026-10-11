//! Searching the mail server, for what the local copy doesn't hold: mail
//! older than the reader's window, or never synced. The server searches
//! every folder (but Trash and Junk); the hits' headers are stored as
//! reading-only mail, so they can be read and cited like any other message.

use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use sqlx::SqlitePool;

use super::sync::MailServer;
use crate::source::SourceId;
use mnemis_types::MailboxRole;

/// What to search for; every given field must match.
#[derive(Debug, Default, Clone)]
pub struct Criteria {
    pub from: Option<String>,
    pub subject: Option<String>,
    /// Anywhere in the message, headers and body.
    pub text: Option<String>,
    /// On or after this day.
    pub since: Option<NaiveDate>,
    /// Before this day.
    pub before: Option<NaiveDate>,
}

impl Criteria {
    pub fn is_empty(&self) -> bool {
        self.from.is_none()
            && self.subject.is_none()
            && self.text.is_none()
            && self.since.is_none()
            && self.before.is_none()
    }

    /// The IMAP `SEARCH` keys.
    pub fn imap(&self) -> String {
        let mut keys = Vec::new();
        for (key, value) in [
            ("FROM", &self.from),
            ("SUBJECT", &self.subject),
            ("TEXT", &self.text),
        ] {
            if let Some(value) = value {
                keys.push(format!("{key} {}", quote(value)));
            }
        }
        if let Some(day) = self.since {
            keys.push(format!("SINCE {}", day.format("%d-%b-%Y")));
        }
        if let Some(day) = self.before {
            keys.push(format!("BEFORE {}", day.format("%d-%b-%Y")));
        }
        if keys.is_empty() {
            "ALL".to_string()
        } else {
            keys.join(" ")
        }
    }
}

fn quote(value: &str) -> String {
    let clean: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Search the account's folders on the server; returns the matching
/// messages' row ids, newest first, at most `limit` (headers of hits not
/// stored yet are fetched and stored).
pub async fn search_server(
    pool: &SqlitePool,
    server: &mut dyn MailServer,
    source_id: i64,
    criteria: &Criteria,
    limit: usize,
) -> Result<Vec<i64>> {
    let folders: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, external_id FROM channels WHERE source_id = ? AND kind = 'mailbox' ORDER BY id",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await
    .context("listing folders")?;
    let mut hits = Vec::new();
    for (channel_id, folder) in folders {
        if matches!(
            super::view::role(&crate::source::imap_utf7::decode_mailbox_name(&folder)),
            MailboxRole::Trash | MailboxRole::Junk
        ) {
            continue;
        }
        let status = server.open(&folder).await?;
        if status.exists == 0 {
            continue;
        }
        let mut uids = server.search(criteria).await?;
        // Newest first, as UIDs grow with arrival.
        uids.sort_unstable_by(|a, b| b.cmp(a));
        uids.truncate(limit);
        if uids.is_empty() {
            continue;
        }
        let known: HashSet<u32> = sqlx::query_scalar::<_, i64>(
            "SELECT uid FROM messages WHERE channel_id = ? AND uidvalidity = ? AND uid IS NOT NULL",
        )
        .bind(channel_id)
        .bind(status.uidvalidity as i64)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|u| u as u32)
        .collect();
        let new: Vec<u32> = uids
            .iter()
            .copied()
            .filter(|u| !known.contains(u))
            .collect();
        for chunk in new.chunks(50) {
            let messages: Vec<_> = server
                .headers(chunk)
                .await?
                .into_iter()
                .filter_map(|h| {
                    let mut m = crate::source::imap::imported(
                        &h.header,
                        h.uid,
                        status.uidvalidity,
                        h.flags,
                        true,
                    )?;
                    if let Some(mail) = m.mail.as_mut() {
                        mail.size = h.size;
                        mail.received_at = h.received_at;
                    }
                    Some(m)
                })
                .collect();
            crate::ingest::ingest_for_reading(pool, SourceId(source_id), channel_id, &messages)
                .await?;
        }
        hits.push((channel_id, status.uidvalidity, uids));
    }

    let mut found: Vec<(i64, String, i64)> = Vec::new();
    for (channel_id, uidvalidity, uids) in hits {
        for uid in uids {
            let row: Option<(i64, String, i64)> = sqlx::query_as(
                "SELECT id, external_id, posted_at FROM messages \
                 WHERE channel_id = ? AND uidvalidity = ? AND uid = ? AND removed_at IS NULL",
            )
            .bind(channel_id)
            .bind(uidvalidity as i64)
            .bind(uid as i64)
            .fetch_optional(pool)
            .await?;
            found.extend(row);
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.2));
    // One email filed in two folders is one hit.
    let mut seen = HashSet::new();
    found.retain(|(_, external_id, _)| seen.insert(external_id.clone()));
    found.truncate(limit);
    Ok(found.into_iter().map(|(id, _, _)| id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::sync::tests::{FakeServer, folder, setup};

    #[test]
    fn builds_quoted_imap_keys() {
        let c = Criteria {
            from: Some("vivo".into()),
            subject: Some("Sua \"Fatura\"".into()),
            since: NaiveDate::from_ymd_opt(2025, 4, 1),
            ..Default::default()
        };
        assert_eq!(
            c.imap(),
            r#"FROM "vivo" SUBJECT "Sua \"Fatura\"" SINCE 01-Apr-2025"#
        );
        assert_eq!(Criteria::default().imap(), "ALL");
    }

    #[tokio::test]
    async fn finds_old_mail_on_the_server_and_stores_its_headers() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        folder(&pool, source_id, "INBOX", true).await?;
        folder(&pool, source_id, "Archive", true).await?;
        folder(&pool, source_id, "Trash", true).await?;
        let mut server = FakeServer::default();
        server.add("INBOX", 1, "new@x", "Sua fatura de outubro", 3);
        server.add("Archive", 7, "old@x", "Sua fatura de abril", 200);
        server.add("Archive", 8, "other@x", "Lunch", 100);
        server.add("Trash", 1, "gone@x", "Sua fatura de maio", 150);

        let criteria = Criteria {
            subject: Some("fatura".into()),
            ..Default::default()
        };
        let ids = search_server(&pool, &mut server, source_id, &criteria, 10).await?;
        let subjects: Vec<String> = {
            let mut out = Vec::new();
            for id in &ids {
                out.push(
                    sqlx::query_scalar("SELECT subject FROM messages WHERE id = ?")
                        .bind(id)
                        .fetch_one(&pool)
                        .await?,
                );
            }
            out
        };
        assert_eq!(
            subjects,
            ["Sua fatura de outubro", "Sua fatura de abril"],
            "newest first, Trash left out"
        );
        // A second search finds the same rows rather than storing them again.
        let again = search_server(&pool, &mut server, source_id, &criteria, 10).await?;
        assert_eq!(again, ids);

        // Hits arrive as headers; reading one downloads the rest of it.
        let (has_body,): (bool,) = sqlx::query_as("SELECT has_body FROM messages WHERE id = ?")
            .bind(ids[1])
            .fetch_one(&pool)
            .await?;
        assert!(!has_body);
        crate::mail::cache::fetch_messages(&pool, &mut server, source_id, &ids[1..]).await?;
        let (has_body, body): (bool, String) =
            sqlx::query_as("SELECT has_body, body FROM messages WHERE id = ?")
                .bind(ids[1])
                .fetch_one(&pool)
                .await?;
        assert!(has_body && body.contains("The body of old@x"), "{body}");
        Ok(())
    }
}
