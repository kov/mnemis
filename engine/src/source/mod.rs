use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod imap;
pub mod imap_utf7;

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
    pub raw_json: Option<String>,
    pub flags: u32,
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
