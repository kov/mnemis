use anyhow::{Context, Result};
use async_imap::Session;
use async_imap::types::NameAttribute;
use async_native_tls::TlsStream;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use mail_parser::MessageParser;
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

use super::links::{self, Link};
use super::{
    ChannelInfo, Cursor, ImportedAuthor, ImportedMessage, MailMeta, PollBatch, Recipient, Source,
    SourceId, SourceKind,
};

/// New folders start muted except the inbox and the sent folder: what was
/// asked of the user and what they promised. The sent folder is the one the
/// server flags `\Sent` (RFC 6154), or failing that, one named like it.
fn subscribed_by_default(mailbox: &str, flagged_sent: bool) -> bool {
    if mailbox.eq_ignore_ascii_case("INBOX") || flagged_sent {
        return true;
    }
    let leaf = mailbox.rsplit(['/', '.']).next().unwrap_or(mailbox);
    ["sent", "sent items", "sent messages", "sent mail"]
        .iter()
        .any(|n| leaf.eq_ignore_ascii_case(n))
}

/// How many recent UIDs to pull on first poll or after UIDVALIDITY change.
const BOOTSTRAP_MESSAGE_COUNT: u32 = 50;

#[derive(Debug, Clone)]
pub struct ImapConfig {
    pub server: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

pub struct ImapSource {
    id: SourceId,
    config: ImapConfig,
}

type ImapSession = Session<TlsStream<Compat<TcpStream>>>;

impl ImapSource {
    pub fn new(id: SourceId, config: ImapConfig) -> Self {
        Self { id, config }
    }

    async fn connect(&self) -> Result<ImapSession> {
        let addr = format!("{}:{}", self.config.server, self.config.port);
        let tcp = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("failed to connect to {addr}"))?;
        let tcp = tcp.compat();
        let tls = async_native_tls::connect(&self.config.server, tcp)
            .await
            .context("TLS handshake failed")?;

        let mut client = async_imap::Client::new(tls);
        client
            .read_response()
            .await
            .context("failed to read IMAP greeting")?;
        let session = client
            .login(&self.config.username, &self.config.password)
            .await
            .map_err(|(err, _)| err)
            .context("IMAP login failed")?;
        Ok(session)
    }
}

fn parse_cursor(c: &Cursor) -> Result<(u32, u32)> {
    let (validity, next) = c.0.split_once(':').context("malformed IMAP cursor")?;
    let validity: u32 = validity
        .parse()
        .context("malformed UIDVALIDITY in cursor")?;
    let next: u32 = next.parse().context("malformed UIDNEXT in cursor")?;
    Ok((validity, next))
}

fn make_cursor(uidvalidity: u32, uidnext: u32) -> Cursor {
    Cursor(format!("{uidvalidity}:{uidnext}"))
}

#[async_trait]
impl Source for ImapSource {
    fn id(&self) -> SourceId {
        self.id
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Imap
    }

    async fn list_channels(&self) -> Result<Vec<ChannelInfo>> {
        let mut session = self.connect().await?;
        let names: Vec<_> = session
            .list(Some(""), Some("*"))
            .await?
            .try_collect()
            .await?;
        let channels = names
            .iter()
            .map(|n| ChannelInfo {
                // external_id is what we hand back to the server in later
                // SELECT calls — it MUST stay in the raw modified-UTF-7
                // form. `name` is for humans; decode it so accented and
                // non-Latin mailboxes don't show up as `Ita&APo-`.
                external_id: n.name().to_string(),
                name: super::imap_utf7::decode_mailbox_name(n.name()),
                kind: "mailbox".to_string(),
                subscribed_by_default: subscribed_by_default(
                    n.name(),
                    n.attributes().contains(&NameAttribute::Sent),
                ),
            })
            .collect();
        let _ = session.logout().await;
        Ok(channels)
    }

    async fn poll(&self, channel_external_id: &str, cursor: Option<&Cursor>) -> Result<PollBatch> {
        let mut session = self.connect().await?;

        // EXAMINE, not SELECT: the poll only reads, and a read-only open
        // guarantees it can never change flags (notably the implicit `\Seen`
        // a body FETCH would otherwise set).
        let mailbox = session
            .examine(channel_external_id)
            .await
            .with_context(|| format!("failed to examine mailbox: {channel_external_id}"))?;

        let server_validity = mailbox.uid_validity.context("missing UIDVALIDITY")?;
        let server_uidnext = mailbox.uid_next.context("missing UIDNEXT")?;

        let search_from_uid = match cursor {
            Some(c) => {
                let (validity, last_uidnext) = parse_cursor(c)?;
                if validity != server_validity {
                    server_uidnext
                        .saturating_sub(BOOTSTRAP_MESSAGE_COUNT)
                        .max(1)
                } else {
                    last_uidnext
                }
            }
            None => server_uidnext
                .saturating_sub(BOOTSTRAP_MESSAGE_COUNT)
                .max(1),
        };

        let messages = if search_from_uid >= server_uidnext {
            Vec::new()
        } else {
            let query = format!("UID {search_from_uid}:*");
            let uids: Vec<u32> = session
                .uid_search(&query)
                .await
                .context("IMAP search failed")?
                .into_iter()
                .filter(|uid| *uid >= search_from_uid)
                .collect();

            if uids.is_empty() {
                Vec::new()
            } else {
                let mut sorted = uids;
                sorted.sort_unstable();
                let uid_set = compress_uid_set(&sorted);
                let fetches: Vec<_> = session
                    .uid_fetch(&uid_set, "(UID FLAGS BODY.PEEK[])")
                    .await?
                    .try_collect()
                    .await?;

                fetches
                    .iter()
                    .filter_map(|fetch| {
                        imported(
                            fetch.body().unwrap_or_default(),
                            fetch.uid.unwrap_or(0),
                            server_validity,
                            parse_flags(fetch.flags().map(|f| format!("{f:?}"))),
                            false,
                        )
                    })
                    .collect()
            }
        };

        let _ = session.logout().await;

        Ok(PollBatch {
            messages,
            next_cursor: make_cursor(server_validity, server_uidnext),
            more_available: false,
        })
    }

    async fn fetch(
        &self,
        channel_external_id: &str,
        message_external_id: &str,
    ) -> Result<ImportedMessage> {
        let mut session = self.connect().await?;
        // EXAMINE, not SELECT: the poll only reads, and a read-only open
        // guarantees it can never change flags (notably the implicit `\Seen`
        // a body FETCH would otherwise set).
        let mailbox = session
            .examine(channel_external_id)
            .await
            .with_context(|| format!("failed to examine mailbox: {channel_external_id}"))?;
        let server_validity = mailbox.uid_validity.context("missing UIDVALIDITY")?;

        let uid = find_uid(&mut session, message_external_id).await?;

        let fetches: Vec<_> = session
            .uid_fetch(&uid.to_string(), "(UID FLAGS BODY.PEEK[])")
            .await?
            .try_collect()
            .await?;
        let fetch = fetches.first().context("no fetch result")?;
        let message = imported(
            fetch.body().unwrap_or_default(),
            uid,
            server_validity,
            parse_flags(fetch.flags().map(|f| format!("{f:?}"))),
            false,
        )
        .context("failed to parse message")?;
        let _ = session.logout().await;
        Ok(message)
    }
}

/// An `ImportedMessage` from a fetched message: the whole RFC 822 text, or
/// with `headers_only` just its header block (the body stays empty until the
/// full message is fetched).
pub(crate) fn imported(
    raw: &[u8],
    uid: u32,
    uidvalidity: u32,
    flags: u32,
    headers_only: bool,
) -> Option<ImportedMessage> {
    use mail_parser::MimeHeaders;
    let msg = MessageParser::default().parse(raw)?;
    let subject = msg.subject().map(|s| s.to_string());
    let posted_at = msg
        .date()
        .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0))
        .unwrap_or_else(Utc::now);
    let mut recipients = parse_recipients(msg.to(), "to");
    recipients.extend(parse_recipients(msg.cc(), "cc"));
    let external_id = msg
        .message_id()
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("imap-uid-{uidvalidity}-{uid}"));
    let ids = |v: &mail_parser::HeaderValue| -> Vec<String> {
        v.as_text_list()
            .unwrap_or_default()
            .iter()
            .map(|s| s.trim_matches(['<', '>', ' ']).to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let has_attachments = if headers_only {
        msg.content_type().is_some_and(|t| {
            t.ctype().eq_ignore_ascii_case("multipart")
                && t.subtype().is_some_and(|s| s.eq_ignore_ascii_case("mixed"))
        })
    } else {
        // Inline images the HTML shows aren't what the paperclip means.
        msg.attachments().any(|p| {
            p.content_disposition().is_some_and(|d| d.is_attachment()) || p.content_id().is_none()
        })
    };
    let (body, links, raw) = if headers_only {
        (String::new(), None, None)
    } else {
        (
            msg.body_text(0).map(|c| c.into_owned()).unwrap_or_default(),
            Some(links::from_message(&msg)),
            Some(raw.to_vec()),
        )
    };
    Some(ImportedMessage {
        external_id,
        parent_external_id: None,
        author: parse_author(msg.from()),
        posted_at,
        subject,
        body,
        body_format: "text".to_string(),
        recipients,
        links,
        raw_json: None,
        flags,
        mail: Some(MailMeta {
            uid: Some(uid),
            uidvalidity: Some(uidvalidity),
            in_reply_to: ids(msg.in_reply_to()).pop(),
            references: ids(msg.references()),
            size: raw.as_ref().map(|r| r.len() as u32),
            has_attachments,
            headers_only,
        }),
        raw,
    })
}

impl ImapSource {
    /// A message's raw RFC 822 bytes, for replying with its headers and
    /// forwarding it whole.
    pub async fn fetch_raw(
        &self,
        channel_external_id: &str,
        message_external_id: &str,
    ) -> Result<Vec<u8>> {
        let mut session = self.connect().await?;
        session
            .examine(channel_external_id)
            .await
            .with_context(|| format!("failed to examine mailbox: {channel_external_id}"))?;
        let uid = find_uid(&mut session, message_external_id).await?;
        let fetches: Vec<_> = session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .await?
            .try_collect()
            .await?;
        let raw = fetches
            .first()
            .and_then(|f| f.body())
            .context("no fetch result")?
            .to_vec();
        let _ = session.logout().await;
        Ok(raw)
    }

    /// File a copy of mail the user sent in their sent folder, marked read.
    /// Only when the account asks for it: SMTP servers don't all keep what
    /// they relay.
    pub async fn append_to_sent(&self, raw: &[u8]) -> Result<()> {
        let mut session = self.connect().await?;
        let names: Vec<_> = session
            .list(Some(""), Some("*"))
            .await?
            .try_collect()
            .await?;
        let sent = names
            .iter()
            .find(|n| n.attributes().contains(&NameAttribute::Sent))
            .or_else(|| {
                names.iter().find(|n| {
                    subscribed_by_default(n.name(), false)
                        && !n.name().eq_ignore_ascii_case("INBOX")
                })
            })
            .map(|n| n.name().to_string())
            .context("no sent folder on the server")?;
        session
            .append(&sent, Some("(\\Seen)"), None, raw)
            .await
            .with_context(|| format!("failed to file the sent copy in {sent}"))?;
        let _ = session.logout().await;
        Ok(())
    }

    /// The links of every message in a mailbox dated `since` or later, by
    /// Message-ID: one session, bodies fetched in chunks. For filling in
    /// messages ingested before links were kept.
    pub async fn links_since(
        &self,
        channel_external_id: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<(String, Vec<Link>)>> {
        const CHUNK: usize = 50;
        let mut session = self.connect().await?;
        session
            .examine(channel_external_id)
            .await
            .with_context(|| format!("failed to examine mailbox: {channel_external_id}"))?;
        let mut uids: Vec<u32> = session
            .uid_search(format!("SINCE {}", since.format("%d-%b-%Y")))
            .await
            .context("IMAP search failed")?
            .into_iter()
            .collect();
        uids.sort_unstable();
        let parser = MessageParser::default();
        let mut out = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(CHUNK) {
            let fetches: Vec<_> = session
                .uid_fetch(compress_uid_set(chunk), "(UID BODY.PEEK[])")
                .await?
                .try_collect()
                .await?;
            for fetch in &fetches {
                let Some(msg) = parser.parse(fetch.body().unwrap_or_default()) else {
                    continue;
                };
                if let Some(id) = msg.message_id() {
                    out.push((id.to_string(), links::from_message(&msg)));
                }
            }
        }
        let _ = session.logout().await;
        Ok(out)
    }
}

impl ImapSource {
    /// One session for the reader's sync of every folder.
    pub async fn mail_session(&self) -> Result<ImapMailSession> {
        let mut session = self.connect().await?;
        let condstore = session
            .capabilities()
            .await
            .context("reading server capabilities")?
            .has_str("CONDSTORE");
        Ok(ImapMailSession { session, condstore })
    }
}

pub struct ImapMailSession {
    session: ImapSession,
    condstore: bool,
}

impl ImapMailSession {
    pub async fn logout(mut self) {
        let _ = self.session.logout().await;
    }
}

#[async_trait]
impl crate::mail::sync::MailServer for ImapMailSession {
    async fn open(&mut self, folder: &str) -> Result<crate::mail::sync::FolderStatus> {
        // SELECT, not EXAMINE: the reader marks mail read. Bodies are still
        // fetched with BODY.PEEK so fetching never marks anything.
        let mailbox = if self.condstore {
            self.session.select_condstore(folder).await
        } else {
            self.session.select(folder).await
        }
        .with_context(|| format!("failed to open folder {folder}"))?;
        Ok(crate::mail::sync::FolderStatus {
            exists: mailbox.exists,
            uidvalidity: mailbox.uid_validity.context("missing UIDVALIDITY")?,
            uidnext: mailbox.uid_next.context("missing UIDNEXT")?,
            highest_modseq: mailbox.highest_modseq.filter(|_| self.condstore),
        })
    }

    async fn uids(&mut self, since: Option<DateTime<Utc>>) -> Result<Vec<u32>> {
        let query = match since {
            Some(date) => format!("SINCE {}", date.format("%d-%b-%Y")),
            None => "ALL".to_string(),
        };
        Ok(self
            .session
            .uid_search(query)
            .await
            .context("IMAP search failed")?
            .into_iter()
            .collect())
    }

    async fn headers(&mut self, uids: &[u32]) -> Result<Vec<crate::mail::sync::FetchedHeaders>> {
        let mut sorted = uids.to_vec();
        sorted.sort_unstable();
        let fetches: Vec<_> = self
            .session
            .uid_fetch(
                compress_uid_set(&sorted),
                "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])",
            )
            .await?
            .try_collect()
            .await?;
        Ok(fetches
            .iter()
            .filter_map(|f| {
                Some(crate::mail::sync::FetchedHeaders {
                    uid: f.uid?,
                    flags: parse_flags(f.flags().map(|f| format!("{f:?}"))),
                    size: f.size,
                    header: f.header()?.to_vec(),
                })
            })
            .collect())
    }

    async fn flags(&mut self, changed_since: Option<u64>) -> Result<Vec<(u32, u32)>> {
        let query = match changed_since {
            Some(modseq) => format!("(UID FLAGS) (CHANGEDSINCE {modseq})"),
            None => "(UID FLAGS)".to_string(),
        };
        let fetches: Vec<_> = self
            .session
            .uid_fetch("1:*", query)
            .await?
            .try_collect()
            .await?;
        Ok(fetches
            .iter()
            .filter_map(|f| Some((f.uid?, parse_flags(f.flags().map(|f| format!("{f:?}"))))))
            .collect())
    }

    async fn store_seen(&mut self, uids: &[u32], seen: bool) -> Result<()> {
        let mut sorted = uids.to_vec();
        sorted.sort_unstable();
        let change = if seen {
            "+FLAGS.SILENT (\\Seen)"
        } else {
            "-FLAGS.SILENT (\\Seen)"
        };
        let _: Vec<_> = self
            .session
            .uid_store(compress_uid_set(&sorted), change)
            .await?
            .try_collect()
            .await?;
        Ok(())
    }
}

/// The UID of a message in the examined mailbox, from its external id: the
/// UID itself for `imap-uid-VALIDITY-UID` ids, else a Message-ID search.
async fn find_uid(session: &mut ImapSession, message_external_id: &str) -> Result<u32> {
    if let Some(rest) = message_external_id.strip_prefix("imap-uid-") {
        let (_, uid_str) = rest
            .split_once('-')
            .context("malformed imap-uid-VALIDITY-UID id")?;
        return uid_str
            .parse::<u32>()
            .context("malformed UID in imap-uid id");
    }
    let uids: Vec<u32> = session
        .uid_search(format!("HEADER Message-ID {message_external_id}"))
        .await?
        .into_iter()
        .collect();
    uids.first().copied().context("message not found")
}

fn parse_author(addr: Option<&mail_parser::Address>) -> Option<ImportedAuthor> {
    use mail_parser::Address;
    let first = match addr? {
        Address::List(list) => list.first()?,
        Address::Group(groups) => groups.first()?.addresses.first()?,
    };
    Some(ImportedAuthor {
        external_id: first.address.as_deref().unwrap_or("").to_string(),
        display_name: first.name.as_deref().map(|s| s.to_string()),
        handle: None,
    })
}

/// Flatten a To/Cc header (list or group form) into `Recipient`s tagged with
/// `kind`. Returns empty when the header is absent.
fn parse_recipients(addr: Option<&mail_parser::Address>, kind: &str) -> Vec<Recipient> {
    use mail_parser::Address;
    let Some(addr) = addr else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match addr {
        Address::List(list) => {
            for a in list.iter() {
                out.push(Recipient {
                    kind: kind.to_string(),
                    name: a.name.as_deref().map(str::to_string),
                    address: a.address.as_deref().map(str::to_string),
                });
            }
        }
        Address::Group(groups) => {
            for g in groups.iter() {
                for a in g.addresses.iter() {
                    out.push(Recipient {
                        kind: kind.to_string(),
                        name: a.name.as_deref().map(str::to_string),
                        address: a.address.as_deref().map(str::to_string),
                    });
                }
            }
        }
    }
    out
}

fn parse_flags(flags: impl Iterator<Item = String>) -> u32 {
    let mut bits = 0u32;
    for f in flags {
        match f.as_str() {
            "Seen" => bits |= 0x01,
            "Answered" => bits |= 0x02,
            "Flagged" => bits |= 0x04,
            "Deleted" => bits |= 0x08,
            "Draft" => bits |= 0x10,
            _ => {}
        }
    }
    bits
}

/// Compress a sorted list of UIDs into IMAP range notation (e.g. "1:5,7,9:12").
fn compress_uid_set(uids: &[u32]) -> String {
    if uids.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    let mut start = uids[0];
    let mut end = uids[0];
    for &uid in &uids[1..] {
        if uid == end + 1 {
            end = uid;
        } else {
            if start == end {
                parts.push(start.to_string());
            } else {
                parts.push(format!("{start}:{end}"));
            }
            start = uid;
            end = uid;
        }
    }
    if start == end {
        parts.push(start.to_string());
    } else {
        parts.push(format!("{start}:{end}"));
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::{compress_uid_set, subscribed_by_default};

    #[test]
    fn only_the_inbox_and_sent_folder_are_subscribed_by_default() {
        assert!(subscribed_by_default("INBOX", false));
        assert!(subscribed_by_default("Inbox", false));
        assert!(
            subscribed_by_default("Enviadas", true),
            "the server's \\Sent flag wins"
        );
        assert!(subscribed_by_default("Sent", false));
        assert!(subscribed_by_default("INBOX.Sent Items", false));
        assert!(subscribed_by_default("[Gmail]/Sent Mail", false));
        for other in [
            "Archive",
            "Trash",
            "Junk Mail",
            "Drafts",
            "INBOX/Lembrar",
            "Sentinel",
        ] {
            assert!(!subscribed_by_default(other, false), "{other}");
        }
    }

    #[test]
    fn compresses_contiguous_ranges() {
        assert_eq!(compress_uid_set(&[1, 2, 3, 4, 5]), "1:5");
        assert_eq!(compress_uid_set(&[1, 2, 3, 7, 9, 10, 11]), "1:3,7,9:11");
        assert_eq!(compress_uid_set(&[42]), "42");
        assert_eq!(compress_uid_set(&[]), "");
    }
}
