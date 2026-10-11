use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod imap;
pub mod imap_utf7;
pub mod links;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Imap,
    Mattermost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor(pub String);

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub external_id: String,
    pub name: String,
    /// 'mailbox' | 'channel' | 'dm' | 'group'
    pub kind: String,
    /// Whether a newly discovered channel starts subscribed (polled) rather
    /// than muted. Only consulted the first time a channel is seen; after that
    /// the user's choice stands.
    pub subscribed_by_default: bool,
}

#[derive(Debug, Clone)]
pub struct ImportedAuthor {
    pub external_id: String,
    pub display_name: Option<String>,
    pub handle: Option<String>,
}

/// One addressee of a message. Serialized into `messages.recipients_json` at
/// ingest and read back by the extractor's window projection, where "am I a
/// direct (to) recipient or just cc'd?" is a strong triage signal. Unlike
/// authors, recipients aren't deduped into the `people` table — they're a
/// display/triage hint, not a join target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipient {
    /// "to" | "cc"
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImportedMessage {
    pub external_id: String,
    pub parent_external_id: Option<String>,
    pub author: Option<ImportedAuthor>,
    pub posted_at: DateTime<Utc>,
    pub subject: Option<String>,
    pub body: String,
    /// 'text' | 'markdown' | 'html'
    pub body_format: String,
    /// to/cc addressees, captured at ingest. Empty when the source doesn't
    /// carry recipient headers (chat sources) or none were present.
    pub recipients: Vec<Recipient>,
    /// Where the message's links lead, for mail; None when the source has
    /// no notion of them (chat) or didn't look.
    pub links: Option<Vec<links::Link>>,
    pub raw_json: Option<String>,
    pub flags: u32,
    /// Where the mail sits on the server and how it threads; None for chat.
    pub mail: Option<MailMeta>,
    /// The full RFC 822 message, when it was fetched whole: kept in the mail
    /// cache so reading it needs no second fetch.
    pub raw: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
pub struct MailMeta {
    pub uid: Option<u32>,
    pub uidvalidity: Option<u32>,
    /// Message-IDs without angle brackets, like `external_id`.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub size: Option<u32>,
    pub has_attachments: bool,
    /// Only the headers were fetched; `body` is empty.
    pub headers_only: bool,
    /// When the server received it (IMAP INTERNALDATE).
    pub received_at: Option<DateTime<Utc>>,
    /// The receiving server's verdict on the sender, when the headers were
    /// read.
    pub auth: Option<crate::mail::suspicion::Auth>,
}

#[derive(Debug)]
pub struct PollBatch {
    pub messages: Vec<ImportedMessage>,
    pub next_cursor: Cursor,
    pub more_available: bool,
}

#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> SourceId;
    fn kind(&self) -> SourceKind;
    async fn list_channels(&self) -> Result<Vec<ChannelInfo>>;
    async fn poll(&self, channel_external_id: &str, cursor: Option<&Cursor>) -> Result<PollBatch>;
    async fn fetch(
        &self,
        channel_external_id: &str,
        message_external_id: &str,
    ) -> Result<ImportedMessage>;
}
