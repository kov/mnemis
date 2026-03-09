use anyhow::{Context, Result};
use async_imap::Session;
use async_native_tls::TlsStream;
use futures::TryStreamExt;
use mail_parser::MessageParser;
use serde::Serialize;
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

use crate::config::ImapConfig;

type ImapSession = Session<TlsStream<Compat<TcpStream>>>;

pub struct ImapClient {
    session: ImapSession,
    current_mailbox: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MessageSummary {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub flags: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FullMessage {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date: String,
    pub flags: Vec<String>,
    pub body: String,
}

impl ImapClient {
    pub async fn connect(config: &ImapConfig) -> Result<Self> {
        let addr = format!("{}:{}", config.server, config.port);
        let tcp = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("failed to connect to {addr}"))?;
        let tcp = tcp.compat();

        let tls = async_native_tls::connect(&config.server, tcp)
            .await
            .context("TLS handshake failed")?;

        let mut client = async_imap::Client::new(tls);
        // Read the server greeting
        client
            .read_response()
            .await
            .context("failed to read IMAP greeting")?;

        let session = client
            .login(&config.username, &config.password)
            .await
            .map_err(|(err, _)| err)
            .context("IMAP login failed")?;

        Ok(Self {
            session,
            current_mailbox: None,
        })
    }

    pub async fn list_mailboxes(&mut self) -> Result<Vec<String>> {
        let names: Vec<_> = self
            .session
            .list(Some(""), Some("*"))
            .await?
            .try_collect()
            .await?;

        Ok(names.iter().map(|n| n.name().to_string()).collect())
    }

    async fn select_mailbox(&mut self, mailbox: &str) -> Result<()> {
        if self.current_mailbox.as_deref() == Some(mailbox) {
            return Ok(());
        }
        self.session
            .select(mailbox)
            .await
            .with_context(|| format!("failed to select mailbox: {mailbox}"))?;
        self.current_mailbox = Some(mailbox.to_string());
        Ok(())
    }

    pub async fn list_messages(
        &mut self,
        mailbox: &str,
        limit: Option<usize>,
    ) -> Result<Vec<MessageSummary>> {
        self.select_mailbox(mailbox).await?;

        let uids: Vec<u32> = {
            let mut all: Vec<u32> = self
                .session
                .uid_search("ALL")
                .await
                .context("IMAP search failed")?
                .into_iter()
                .collect();
            all.sort();
            if let Some(limit) = limit {
                let start = all.len().saturating_sub(limit);
                all.split_off(start)
            } else {
                all
            }
        };

        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let uid_set = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetches: Vec<_> = self
            .session
            .uid_fetch(&uid_set, "UID FLAGS ENVELOPE")
            .await?
            .try_collect()
            .await?;

        let mut messages = Vec::new();
        for fetch in &fetches {
            let uid = fetch.uid.unwrap_or(0);
            let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();

            let (subject, from, date) = if let Some(env) = fetch.envelope() {
                let subject = env
                    .subject
                    .as_ref()
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .unwrap_or("")
                    .to_string();
                let from = env
                    .from
                    .as_ref()
                    .and_then(|addrs| addrs.first())
                    .map(|a| {
                        let mailbox = a
                            .mailbox
                            .as_ref()
                            .and_then(|m| std::str::from_utf8(m).ok())
                            .unwrap_or("");
                        let host = a
                            .host
                            .as_ref()
                            .and_then(|h| std::str::from_utf8(h).ok())
                            .unwrap_or("");
                        let name = a
                            .name
                            .as_ref()
                            .and_then(|n| std::str::from_utf8(n).ok())
                            .unwrap_or("");
                        if name.is_empty() {
                            format!("{mailbox}@{host}")
                        } else {
                            format!("{name} <{mailbox}@{host}>")
                        }
                    })
                    .unwrap_or_default();
                let date = env
                    .date
                    .as_ref()
                    .and_then(|d| std::str::from_utf8(d).ok())
                    .unwrap_or("")
                    .to_string();
                (subject, from, date)
            } else {
                (String::new(), String::new(), String::new())
            };

            messages.push(MessageSummary {
                uid,
                subject,
                from,
                date,
                flags,
            });
        }

        Ok(messages)
    }

    pub async fn read_messages(&mut self, mailbox: &str, uids: &[u32]) -> Result<Vec<FullMessage>> {
        self.select_mailbox(mailbox).await?;

        let uid_set = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetches: Vec<_> = self
            .session
            .uid_fetch(&uid_set, "UID FLAGS RFC822")
            .await?
            .try_collect()
            .await?;

        let parser = MessageParser::default();
        let mut messages = Vec::new();

        for fetch in &fetches {
            let raw = fetch.body().unwrap_or_default();
            let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();
            let msg_uid = fetch.uid.unwrap_or(0);

            let (subject, from, to, date, body) = if let Some(msg) = parser.parse(raw) {
                let subject = msg.subject().unwrap_or("").to_string();
                let from = format_address(msg.from());
                let to = format_address(msg.to());
                let date = msg.date().map(|d| d.to_rfc3339()).unwrap_or_default();
                let body = msg.body_text(0).map(|c| c.into_owned()).unwrap_or_default();
                (subject, from, to, date, body)
            } else {
                let body = String::from_utf8_lossy(raw).into_owned();
                (
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    body,
                )
            };

            messages.push(FullMessage {
                uid: msg_uid,
                subject,
                from,
                to,
                date,
                flags,
                body,
            });
        }

        Ok(messages)
    }

    pub async fn mark_as_read(&mut self, mailbox: &str, uids: &[u32]) -> Result<()> {
        self.select_mailbox(mailbox).await?;
        let uid_set = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let _: Vec<_> = self
            .session
            .uid_store(&uid_set, "+FLAGS (\\Seen)")
            .await?
            .try_collect()
            .await?;
        Ok(())
    }

    pub async fn logout(&mut self) -> Result<()> {
        self.session.logout().await?;
        Ok(())
    }
}

fn format_address(addr: Option<&mail_parser::Address>) -> String {
    use mail_parser::Address;
    match addr {
        Some(Address::List(list)) => list
            .iter()
            .map(|a| {
                if let Some(name) = &a.name {
                    format!("{name} <{}>", a.address.as_deref().unwrap_or(""))
                } else {
                    a.address.as_deref().unwrap_or("").to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        Some(Address::Group(groups)) => groups
            .iter()
            .map(|g| {
                let members = g
                    .addresses
                    .iter()
                    .map(|a| a.address.as_deref().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}: {members}", g.name.as_deref().unwrap_or(""))
            })
            .collect::<Vec<_>>()
            .join("; "),
        None => String::new(),
    }
}
