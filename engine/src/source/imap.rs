use anyhow::{Context, Result};
use async_imap::Session;
use async_native_tls::TlsStream;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use mail_parser::MessageParser;
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

use super::{
    ChannelInfo, Cursor, ImportedAuthor, ImportedMessage, PollBatch, Source, SourceId, SourceKind,
};

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
                external_id: n.name().to_string(),
                name: n.name().to_string(),
                kind: "mailbox".to_string(),
            })
            .collect();
        let _ = session.logout().await;
        Ok(channels)
    }

    async fn poll(&self, channel_external_id: &str, cursor: Option<&Cursor>) -> Result<PollBatch> {
        let mut session = self.connect().await?;

        // EXAMINE, not SELECT: mnemis is a read-only ingester, so open every
        // mailbox read-only. This guarantees the server can never mutate flags
        // (notably the implicit `\Seen` a body FETCH would otherwise set).
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

                let parser = MessageParser::default();
                let mut out = Vec::with_capacity(fetches.len());
                for fetch in &fetches {
                    let raw = fetch.body().unwrap_or_default();
                    let uid = fetch.uid.unwrap_or(0);
                    let flags = parse_flags(fetch.flags().map(|f| format!("{f:?}")));

                    let Some(msg) = parser.parse(raw) else {
                        continue;
                    };

                    let subject = msg.subject().map(|s| s.to_string());
                    let body = msg.body_text(0).map(|c| c.into_owned()).unwrap_or_default();
                    let posted_at = msg
                        .date()
                        .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0))
                        .unwrap_or_else(Utc::now);
                    let author = parse_author(msg.from());
                    let message_id = msg
                        .message_id()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("imap-uid-{server_validity}-{uid}"));

                    out.push(ImportedMessage {
                        external_id: message_id,
                        parent_external_id: None,
                        author,
                        posted_at,
                        subject,
                        body,
                        body_format: "text".to_string(),
                        raw_json: None,
                        flags,
                    });
                }
                out
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
        // EXAMINE, not SELECT: mnemis is a read-only ingester, so open every
        // mailbox read-only. This guarantees the server can never mutate flags
        // (notably the implicit `\Seen` a body FETCH would otherwise set).
        let mailbox = session
            .examine(channel_external_id)
            .await
            .with_context(|| format!("failed to examine mailbox: {channel_external_id}"))?;
        let server_validity = mailbox.uid_validity.context("missing UIDVALIDITY")?;

        let uid = if let Some(rest) = message_external_id.strip_prefix("imap-uid-") {
            let (_, uid_str) = rest
                .split_once('-')
                .context("malformed imap-uid-VALIDITY-UID id")?;
            uid_str
                .parse::<u32>()
                .context("malformed UID in imap-uid id")?
        } else {
            let uids: Vec<u32> = session
                .uid_search(&format!("HEADER Message-ID {message_external_id}"))
                .await?
                .into_iter()
                .collect();
            *uids.first().context("message not found")?
        };

        let fetches: Vec<_> = session
            .uid_fetch(&uid.to_string(), "(UID FLAGS BODY.PEEK[])")
            .await?
            .try_collect()
            .await?;
        let fetch = fetches.first().context("no fetch result")?;
        let raw = fetch.body().unwrap_or_default();
        let flags = parse_flags(fetch.flags().map(|f| format!("{f:?}")));

        let parser = MessageParser::default();
        let msg = parser.parse(raw).context("failed to parse message")?;
        let subject = msg.subject().map(|s| s.to_string());
        let body = msg.body_text(0).map(|c| c.into_owned()).unwrap_or_default();
        let posted_at = msg
            .date()
            .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0))
            .unwrap_or_else(Utc::now);
        let author = parse_author(msg.from());
        let message_id = msg
            .message_id()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("imap-uid-{server_validity}-{uid}"));

        let _ = session.logout().await;

        Ok(ImportedMessage {
            external_id: message_id,
            parent_external_id: None,
            author,
            posted_at,
            subject,
            body,
            body_format: "text".to_string(),
            raw_json: None,
            flags,
        })
    }
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
    use super::compress_uid_set;

    #[test]
    fn compresses_contiguous_ranges() {
        assert_eq!(compress_uid_set(&[1, 2, 3, 4, 5]), "1:5");
        assert_eq!(compress_uid_set(&[1, 2, 3, 7, 9, 10, 11]), "1:3,7,9:11");
        assert_eq!(compress_uid_set(&[42]), "42");
        assert_eq!(compress_uid_set(&[]), "");
    }
}
