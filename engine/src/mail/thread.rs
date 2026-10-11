//! Grouping mail into threads. A thread owns every Message-ID its mail
//! carries (each message's own, its In-Reply-To and References), so a reply
//! finds its thread whatever order the two arrive in, and a message that
//! links two threads merges them. Replies from clients that drop the
//! threading headers join by subject, within a few weeks.

use anyhow::{Context, Result};
use sqlx::SqliteConnection;

/// How far apart a header-less reply and its thread may be.
const SUBJECT_WINDOW_SECS: i64 = 30 * 24 * 3600;

/// The thread for a message, creating or merging threads as needed; returns
/// the thread id. `ids` are the message's own Message-ID, In-Reply-To and
/// References.
pub(crate) async fn assign(
    conn: &mut SqliteConnection,
    source_id: i64,
    own_id: &str,
    linked: &[String],
    subject: Option<&str>,
    posted_at: i64,
) -> Result<i64> {
    let mut ids: Vec<&str> = std::iter::once(own_id)
        .chain(linked.iter().map(String::as_str))
        .filter(|s| !s.is_empty())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let ids_json = serde_json::to_string(&ids)?;

    let mut threads: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT thread_id FROM mail_thread_refs \
         WHERE source_id = ? AND ref IN (SELECT value FROM json_each(?)) ORDER BY thread_id",
    )
    .bind(source_id)
    .bind(&ids_json)
    .fetch_all(&mut *conn)
    .await
    .context("looking up thread refs")?;

    let key = subject.map(subject_key).unwrap_or_default();
    if threads.is_empty() && linked.is_empty() && subject.is_some_and(is_reply_subject) {
        let by_subject: Option<i64> = sqlx::query_scalar(
            "SELECT t.id FROM mail_threads t JOIN messages m ON m.thread_id = t.id \
             WHERE t.source_id = ? AND t.subject_key = ? AND t.subject_key != '' \
               AND m.posted_at BETWEEN ? AND ? \
             ORDER BY m.posted_at DESC LIMIT 1",
        )
        .bind(source_id)
        .bind(&key)
        .bind(posted_at - SUBJECT_WINDOW_SECS)
        .bind(posted_at + SUBJECT_WINDOW_SECS)
        .fetch_optional(&mut *conn)
        .await
        .context("looking up thread by subject")?;
        threads.extend(by_subject);
    }

    let thread = match threads.split_first() {
        None => sqlx::query_scalar(
            "INSERT INTO mail_threads (source_id, subject_key) VALUES (?, ?) RETURNING id",
        )
        .bind(source_id)
        .bind(&key)
        .fetch_one(&mut *conn)
        .await
        .context("creating thread")?,
        Some((&keep, merged)) => {
            for &other in merged {
                for sql in [
                    "UPDATE messages SET thread_id = ? WHERE thread_id = ?",
                    "UPDATE mail_thread_refs SET thread_id = ? WHERE thread_id = ?",
                ] {
                    sqlx::query(sql)
                        .bind(keep)
                        .bind(other)
                        .execute(&mut *conn)
                        .await
                        .context("merging threads")?;
                }
                sqlx::query("DELETE FROM mail_threads WHERE id = ?")
                    .bind(other)
                    .execute(&mut *conn)
                    .await
                    .context("merging threads")?;
            }
            keep
        }
    };

    sqlx::query(
        "INSERT OR IGNORE INTO mail_thread_refs (source_id, ref, thread_id) \
         SELECT ?, value, ? FROM json_each(?)",
    )
    .bind(source_id)
    .bind(thread)
    .bind(&ids_json)
    .execute(&mut *conn)
    .await
    .context("recording thread refs")?;
    Ok(thread)
}

/// Reply and forward prefixes, in the languages the user's mail uses most.
const PREFIXES: &[&str] = &[
    "re", "fw", "fwd", "res", "enc", "aw", "wg", "sv", "tr", "rif",
];

/// Strip one leading "Re:"-style prefix (also "Re[2]:"), if there is one.
fn strip_prefix(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let (word, rest) = s.split_once(':')?;
    let word = word.trim_end();
    let word = match word.split_once('[') {
        Some((w, n)) if n.ends_with(']') => w,
        _ => word,
    };
    PREFIXES
        .iter()
        .any(|p| word.eq_ignore_ascii_case(p))
        .then_some(rest)
}

fn is_reply_subject(subject: &str) -> bool {
    strip_prefix(subject).is_some()
}

/// The subject without its reply/forward prefixes, lowercased, whitespace
/// collapsed.
pub(crate) fn subject_key(subject: &str) -> String {
    let mut s = subject;
    while let Some(rest) = strip_prefix(s) {
        s = rest;
    }
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::subject_key;

    #[test]
    fn subject_keys_ignore_reply_prefixes() {
        assert_eq!(subject_key("Re: RES: Fwd:  Lunch  plans"), "lunch plans");
        assert_eq!(subject_key("Re[2]: Lunch plans"), "lunch plans");
        assert_eq!(subject_key("Lunch plans"), "lunch plans");
        assert_eq!(subject_key("Ticket: 42 is open"), "ticket: 42 is open");
    }
}
