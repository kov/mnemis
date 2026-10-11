//! What the reader shows: the folders of every account, a folder's threads
//! (or every inbox's), and a thread's messages. Mail that left its folder
//! (`removed_at`) is never shown; the same email in two folders is one
//! message.

use std::collections::HashMap;

use anyhow::{Context, Result};
use mnemis_types::{
    ActionStatus, MailScope, MailboxDto, MailboxRole, ThreadDto, ThreadMessageDto,
    ThreadSummaryDto, ThreadTodoDto,
};
use sqlx::SqlitePool;

use crate::source::Recipient;

const SEEN: i64 = 0x01;
const ANSWERED: i64 = 0x02;
const FLAGGED: i64 = 0x04;
const SNIPPET_LEN: usize = 160;
const OPEN: &str = "('pending', 'auto_claimed', 'claimed')";

/// Every mail folder, account by account, as a tree: the inbox first, then
/// the other special folders, then the rest by path.
pub async fn mailboxes(pool: &SqlitePool) -> Result<Vec<MailboxDto>> {
    let rows: Vec<(i64, i64, String, String, i64)> = sqlx::query_as(
        "SELECT c.id, c.source_id, s.name, c.name, \
                (SELECT COUNT(DISTINCT m.external_id) FROM messages m \
                 WHERE m.channel_id = c.id AND m.removed_at IS NULL AND m.flags & 1 = 0) \
         FROM channels c JOIN sources s ON s.id = c.source_id \
         WHERE s.kind = 'imap' AND c.kind = 'mailbox' ORDER BY s.id",
    )
    .fetch_all(pool)
    .await
    .context("listing folders")?;
    let mut out: Vec<MailboxDto> = rows
        .into_iter()
        .map(|(channel_id, source_id, account, path, unread)| {
            // Folders found before names were decoded on discovery
            // still hold the server's modified UTF-7.
            let path = crate::source::imap_utf7::decode_mailbox_name(&path);
            MailboxDto {
                channel_id,
                source_id,
                account,
                name: leaf(&path).to_string(),
                role: role(&path),
                path,
                unread,
            }
        })
        .collect();
    // A tree: top-level folders by role then name, each followed by its
    // subfolders, so INBOX's children sit under INBOX.
    out.sort_by_cached_key(|b| {
        let levels: Vec<String> = levels(&b.path).iter().map(|l| l.to_lowercase()).collect();
        let top = levels.first().map(String::as_str).unwrap_or_default();
        (b.source_id, rank(role(top)), levels)
    });
    Ok(out)
}

/// A folder path's levels, top first (see [`leaf`] for the separators).
fn levels(path: &str) -> Vec<&str> {
    if path.contains('/') {
        path.split('/').collect()
    } else if path.len() > 6 && path[..6].eq_ignore_ascii_case("INBOX.") {
        path.split('.').collect()
    } else {
        vec![path]
    }
}

/// The last level of a folder path. Servers separate levels with `/` or,
/// under a Dovecot-style `INBOX.` namespace, with `.`.
fn leaf(path: &str) -> &str {
    if path.contains('/') {
        path.rsplit('/').next().unwrap_or(path)
    } else if path.len() > 6 && path[..6].eq_ignore_ascii_case("INBOX.") {
        path.rsplit('.').next().unwrap_or(path)
    } else {
        path
    }
}

pub(crate) fn role(path: &str) -> MailboxRole {
    if path.eq_ignore_ascii_case("INBOX") {
        return MailboxRole::Inbox;
    }
    let name = leaf(path).to_lowercase();
    let is = |names: &[&str]| names.contains(&name.as_str());
    if is(&["drafts", "draft", "rascunhos"]) {
        MailboxRole::Drafts
    } else if is(&[
        "sent",
        "sent items",
        "sent mail",
        "sent messages",
        "enviados",
        "enviadas",
        "itens enviados",
    ]) {
        MailboxRole::Sent
    } else if is(&["archive", "archives", "all mail", "arquivo", "arquivados"]) {
        MailboxRole::Archive
    } else if is(&[
        "junk",
        "spam",
        "junk mail",
        "junk e-mail",
        "lixo eletrônico",
    ]) {
        MailboxRole::Junk
    } else if is(&[
        "trash",
        "deleted",
        "deleted items",
        "deleted messages",
        "bin",
        "lixeira",
        "lixo",
    ]) {
        MailboxRole::Trash
    } else {
        MailboxRole::Other
    }
}

fn rank(role: MailboxRole) -> u8 {
    match role {
        MailboxRole::Inbox => 0,
        MailboxRole::Drafts => 1,
        MailboxRole::Sent => 2,
        MailboxRole::Archive => 3,
        MailboxRole::Junk => 4,
        MailboxRole::Trash => 5,
        MailboxRole::Other => 6,
    }
}

/// A page of threads with mail in `scope`, latest activity first; `before`
/// (a `latest_at`) continues a previous page.
pub async fn threads(
    pool: &SqlitePool,
    scope: MailScope,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<ThreadSummaryDto>> {
    let (all_inboxes, channel_id) = match scope {
        MailScope::AllInboxes => (true, None),
        MailScope::Mailbox { channel_id } => (false, Some(channel_id)),
    };
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        i64,
        i64,
        i64,
        bool,
        bool,
        i64,
        Option<String>,
        Option<String>,
        Option<i64>,
    )> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH scoped AS ( \
                 SELECT DISTINCT m.thread_id FROM messages m JOIN channels c ON c.id = m.channel_id \
                 WHERE m.removed_at IS NULL AND m.thread_id IS NOT NULL \
                   AND ((?1 AND upper(c.external_id) = 'INBOX') OR c.id = ?2)), \
             live AS ( \
                 SELECT * FROM messages \
                 WHERE thread_id IN (SELECT thread_id FROM scoped) AND removed_at IS NULL) \
             SELECT t.id, s.name, MAX(l.posted_at) AS latest, \
                    COUNT(DISTINCT l.external_id), \
                    COUNT(DISTINCT CASE WHEN l.flags & 1 = 0 THEN l.external_id END), \
                    MAX(l.has_attachments), MAX(l.flags & 4 != 0), \
                    (SELECT COUNT(DISTINCT a.id) FROM action_evidence ae \
                     JOIN actions a ON a.id = ae.action_id \
                     JOIN messages mm ON mm.id = ae.message_id \
                     WHERE mm.thread_id = t.id AND a.status IN {OPEN}), \
                    (SELECT l2.subject FROM live l2 WHERE l2.thread_id = t.id \
                     ORDER BY l2.posted_at DESC LIMIT 1), \
                    (SELECT l2.body FROM live l2 WHERE l2.thread_id = t.id \
                     ORDER BY l2.posted_at DESC LIMIT 1), \
                    (SELECT l2.received_at FROM live l2 WHERE l2.thread_id = t.id \
                     ORDER BY l2.posted_at DESC LIMIT 1) \
             FROM live l JOIN mail_threads t ON t.id = l.thread_id \
             JOIN sources s ON s.id = t.source_id \
             GROUP BY t.id HAVING ?3 IS NULL OR latest < ?3 \
             ORDER BY latest DESC LIMIT ?4"
        )))
        .bind(all_inboxes)
        .bind(channel_id)
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("listing threads")?;

    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let mine = own(pool).await?;
    let authors: Vec<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT m.thread_id, p.display_name, p.external_id FROM messages m \
         LEFT JOIN people p ON p.id = m.author_id \
         WHERE m.thread_id IN (SELECT value FROM json_each(?)) AND m.removed_at IS NULL \
         ORDER BY m.posted_at, m.id",
    )
    .bind(serde_json::to_string(&ids)?)
    .fetch_all(pool)
    .await
    .context("listing thread participants")?;
    let mut participants: HashMap<i64, Vec<String>> = HashMap::new();
    for (thread, name, address) in authors {
        let shown = if address
            .as_deref()
            .is_some_and(|a| mine.contains(&a.to_lowercase()))
        {
            "Me".to_string()
        } else {
            name.filter(|n| !n.trim().is_empty())
                .or(address)
                .unwrap_or_else(|| "Unknown".to_string())
        };
        let names = participants.entry(thread).or_default();
        if !names.contains(&shown) {
            names.push(shown);
        }
    }

    Ok(rows
        .into_iter()
        .map(
            |(
                thread_id,
                account,
                latest_at,
                count,
                unread,
                attachments,
                flagged,
                todos,
                subject,
                body,
                received_at,
            )| {
                ThreadSummaryDto {
                    thread_id,
                    subject: subject.unwrap_or_default(),
                    participants: participants.remove(&thread_id).unwrap_or_default(),
                    snippet: snippet(&body.unwrap_or_default()),
                    latest_at,
                    message_count: count,
                    unread_count: unread,
                    has_attachments: attachments,
                    flagged,
                    todo_count: todos,
                    account,
                    misdated: super::misdated(latest_at, received_at),
                }
            },
        )
        .collect())
}

/// A thread's messages, oldest first, with the open to-dos from its mail.
pub async fn thread(pool: &SqlitePool, thread_id: i64) -> Result<ThreadDto> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        String,
        i64,
        bool,
        String,
        bool,
        Option<i64>,
    )> = sqlx::query_as(
        "SELECT m.id, m.external_id, p.display_name, p.external_id, m.recipients_json, \
                m.posted_at, m.subject, m.body, m.flags, m.has_attachments, c.name, \
                EXISTS (SELECT 1 FROM cache.mail_raw r \
                        WHERE r.source_id = c.source_id AND r.external_id = m.external_id), \
                m.received_at \
         FROM messages m JOIN channels c ON c.id = m.channel_id \
         LEFT JOIN people p ON p.id = m.author_id \
         WHERE m.thread_id = ? AND m.removed_at IS NULL \
         ORDER BY m.posted_at, m.id",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await
    .context("loading the thread")?;
    let mine = own(pool).await?;

    let mut messages: Vec<ThreadMessageDto> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for (
        id,
        external_id,
        name,
        address,
        recipients,
        posted_at,
        subject,
        body,
        flags,
        attachments,
        folder,
        downloaded,
        received_at,
    ) in rows
    {
        let folder = leaf(&crate::source::imap_utf7::decode_mailbox_name(&folder)).to_string();
        if let Some(&i) = by_id.get(&external_id) {
            let m = &mut messages[i];
            m.folders.push(folder);
            m.seen |= flags & SEEN != 0;
            m.flagged |= flags & FLAGGED != 0;
            m.answered |= flags & ANSWERED != 0;
            continue;
        }
        let recipients: Vec<Recipient> = recipients
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_default();
        let shown = |kind: &str| -> Vec<String> {
            recipients
                .iter()
                .filter(|r| r.kind == kind)
                .filter_map(|r| match (&r.name, &r.address) {
                    (Some(n), Some(a)) if !n.trim().is_empty() => Some(format!("{n} <{a}>")),
                    (_, Some(a)) => Some(a.clone()),
                    (Some(n), None) => Some(n.clone()),
                    (None, None) => None,
                })
                .collect()
        };
        by_id.insert(external_id, messages.len());
        let from_me = address
            .as_deref()
            .is_some_and(|a| mine.contains(&a.to_lowercase()));
        messages.push(ThreadMessageDto {
            message_id: id,
            from_me,
            // The user's own mail is theirs, whatever its copies say.
            misdated: super::misdated(posted_at, received_at).filter(|_| !from_me),
            from_name: name,
            from_address: address.filter(|a| a.contains('@')),
            to: shown("to"),
            cc: shown("cc"),
            posted_at,
            subject: subject.unwrap_or_default(),
            snippet: snippet(&body),
            seen: flags & SEEN != 0,
            flagged: flags & FLAGGED != 0,
            answered: flags & ANSWERED != 0,
            has_attachments: attachments,
            folders: vec![folder],
            downloaded,
        });
    }

    let todos: Vec<(i64, String, String, Option<i64>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT a.id, a.title, a.status, a.due_at FROM actions a \
         JOIN action_evidence ae ON ae.action_id = a.id \
         JOIN messages m ON m.id = ae.message_id \
         WHERE m.thread_id = ? AND a.status IN {OPEN} ORDER BY a.extracted_at"
        )))
        .bind(thread_id)
        .fetch_all(pool)
        .await
        .context("loading the thread's to-dos")?;

    Ok(ThreadDto {
        thread_id,
        subject: messages
            .last()
            .map(|m| m.subject.clone())
            .unwrap_or_default(),
        messages,
        todos: todos
            .into_iter()
            .map(|(action_id, title, status, due_at)| ThreadTodoDto {
                action_id,
                title,
                status: ActionStatus::parse(&status).unwrap_or(ActionStatus::Pending),
                due_at,
            })
            .collect(),
    })
}

async fn own(pool: &SqlitePool) -> Result<Vec<String>> {
    super::own_addresses(pool).await
}

/// The start of a message for lists: quoted lines and the "On … wrote:"
/// line before them left out, whitespace collapsed.
fn snippet(body: &str) -> String {
    let lines: Vec<&str> = body.lines().map(str::trim).collect();
    let mut words = String::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with('>') {
            continue;
        }
        let introduces_quote = lines[i + 1..]
            .iter()
            .find(|l| !l.is_empty())
            .is_some_and(|next| next.starts_with('>'))
            && line.ends_with(':');
        if line.is_empty() || introduces_quote {
            continue;
        }
        if !words.is_empty() {
            words.push(' ');
        }
        words.push_str(&line.split_whitespace().collect::<Vec<_>>().join(" "));
        if words.chars().count() > SNIPPET_LEN {
            break;
        }
    }
    if words.chars().count() > SNIPPET_LEN {
        let cut: String = words.chars().take(SNIPPET_LEN).collect();
        format!("{}…", cut.trim_end())
    } else {
        words
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::sync::sync_folders;
    use crate::mail::sync::tests::{FakeServer, folder, setup};

    #[test]
    fn snippets_skip_quotes() {
        assert_eq!(
            snippet("Sounds good.\n\nOn Tue, Ana wrote:\n> Lunch?\n> Tomorrow\n"),
            "Sounds good."
        );
        assert_eq!(snippet("  Hello\n   there  "), "Hello there");
    }

    #[test]
    fn folders_are_named_and_ranked_by_what_they_are_for() {
        assert_eq!(role("INBOX"), MailboxRole::Inbox);
        assert_eq!(role("INBOX.Sent Items"), MailboxRole::Sent);
        assert_eq!(role("[Gmail]/All Mail"), MailboxRole::Archive);
        assert_eq!(role("Lixeira"), MailboxRole::Trash);
        assert_eq!(role("INBOX/Lembrar"), MailboxRole::Other);
        assert_eq!(leaf("INBOX/Lembrar"), "Lembrar");
        assert_eq!(leaf("INBOX.Sent Items"), "Sent Items");
        assert_eq!(leaf("Archive"), "Archive");
    }

    #[tokio::test]
    async fn folders_list_as_a_tree_with_decoded_names() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        for path in [
            "Gnome",
            "INBOX/Condom&AO0-nio",
            "Sent",
            "INBOX",
            "Archive",
            "Gnome/Bugs",
        ] {
            folder(&pool, source_id, path, true).await?;
        }
        let paths: Vec<String> = mailboxes(&pool)
            .await?
            .into_iter()
            .map(|b| b.path)
            .collect();
        assert_eq!(
            paths,
            [
                "INBOX",
                "INBOX/Condomínio",
                "Sent",
                "Archive",
                "Gnome",
                "Gnome/Bugs"
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn lists_threads_across_folders_and_shows_each_email_once() -> Result<()> {
        let (_tmp, pool, source_id) = setup().await?;
        let inbox = folder(&pool, source_id, "INBOX", true).await?;
        let archive = folder(&pool, source_id, "Archive", true).await?;
        folder(&pool, source_id, "Sent", true).await?;
        let mut server = FakeServer::default();
        server.add("INBOX", 1, "a@x", "Lunch", 3);
        server.add("Archive", 1, "a@x", "Lunch", 3);
        server.add("INBOX", 2, "other@x", "Other", 1);
        server.add("Archive", 2, "old@x", "Archived", 5);
        sync_folders(&pool, &mut server, source_id).await?;
        let me: i64 = sqlx::query_scalar(
            "INSERT INTO contacts (display_name, relationship, created_at, updated_at) \
             VALUES ('Me', 'self', 0, 0) RETURNING id",
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query("INSERT INTO contact_identifiers (contact_id, kind, value) VALUES (?, 'email', 'me@example.com')")
            .bind(me)
            .execute(&pool)
            .await?;
        // The user's reply, in Sent, answers a@x.
        let mut reply = crate::source::imap::imported(
            b"Message-ID: <b@x>\r\nIn-Reply-To: <a@x>\r\nSubject: Re: Lunch\r\n\
              From: Me <me@example.com>\r\n\r\n",
            3,
            1,
            0x01,
            true,
        )
        .unwrap();
        reply.posted_at = chrono::Utc::now();
        let sent: i64 = sqlx::query_scalar("SELECT id FROM channels WHERE external_id = 'Sent'")
            .fetch_one(&pool)
            .await?;
        crate::ingest::ingest_for_reading(
            &pool,
            crate::source::SourceId(source_id),
            sent,
            &[reply],
        )
        .await?;

        let inboxes = threads(&pool, MailScope::AllInboxes, None, 50).await?;
        let subjects: Vec<&str> = inboxes.iter().map(|t| t.subject.as_str()).collect();
        assert_eq!(
            subjects,
            ["Re: Lunch", "Other"],
            "latest activity first, archive-only left out"
        );
        let lunch = &inboxes[0];
        assert_eq!(
            lunch.message_count, 2,
            "a@x counts once though it's in two folders"
        );
        assert_eq!(lunch.unread_count, 1);
        assert_eq!(lunch.participants, ["Ana", "Me"]);

        let archived = threads(
            &pool,
            MailScope::Mailbox {
                channel_id: archive,
            },
            None,
            50,
        )
        .await?;
        assert_eq!(archived.len(), 2);
        let page = threads(
            &pool,
            MailScope::Mailbox {
                channel_id: archive,
            },
            Some(archived[0].latest_at),
            50,
        )
        .await?;
        assert_eq!(page.len(), 1, "the next page continues after the first");

        let t = thread(&pool, lunch.thread_id).await?;
        assert_eq!(t.messages.len(), 2);
        assert_eq!(t.messages[0].folders, ["INBOX", "Archive"]);
        assert!(t.messages[1].from_me && t.messages[1].seen);

        let boxes = mailboxes(&pool).await?;
        let names: Vec<&str> = boxes.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["INBOX", "Sent", "Archive"]);
        assert_eq!(boxes[0].unread, 2);
        assert_eq!(boxes[0].channel_id, inbox);
        Ok(())
    }
}
