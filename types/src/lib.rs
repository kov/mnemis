//! Wire-format types shared between `mnemis-engine` (backend) and the
//! Leptos WASM frontend in `mnemis-app/ui/`.
//!
//! Pure serde types only — no sqlx, no I/O, nothing that fails to compile to
//! `wasm32-unknown-unknown`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Pending,
    AutoClaimed,
    Claimed,
    Done,
    Cancelled,
    Dismissed,
}

impl ActionStatus {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "auto_claimed" => Some(Self::AutoClaimed),
            "claimed" => Some(Self::Claimed),
            "done" => Some(Self::Done),
            "cancelled" => Some(Self::Cancelled),
            "dismissed" => Some(Self::Dismissed),
            _ => None,
        }
    }
}

/// Health of a single configured source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceHealth {
    Ok,
    Warning,
    Failed,
    Disabled,
}

impl SourceHealth {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok" => Some(Self::Ok),
            "warning" => Some(Self::Warning),
            "failed" => Some(Self::Failed),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub health: SourceHealth,
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
    pub consecutive_failures: i64,
}

/// Overall application status snapshot for the status panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub sources: Vec<SourceStatus>,
    /// Pending items in the embedding queue.
    pub embed_queue_depth: i64,
    /// Most recent successful or failed extraction run, across all channels.
    pub last_extraction_at: Option<i64>,
}

/// Summary returned from a manual `sync_now` invocation. Cheap aggregate
/// counts plus per-source/per-channel errors so the UI can show "synced 3
/// sources, 12 new messages, 2 actions" and surface failures without the
/// frontend having to interpret them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub sources_synced: i64,
    pub sources_failed: i64,
    pub channels_polled: i64,
    pub messages_ingested: i64,
    pub embeddings_drained: i64,
    pub actions_created: i64,
    /// Human-readable error lines (one per failure). Empty on a clean run.
    pub errors: Vec<String>,
}

/// A single message as rendered in the inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDto {
    pub id: i64,
    pub external_id: String,
    pub subject: Option<String>,
    /// Short preview of the body — truncated server-side so the wire stays small.
    pub snippet: String,
    pub author_display: Option<String>,
    /// Unix seconds.
    pub posted_at: i64,
    pub channel_name: Option<String>,
    pub source_name: Option<String>,
    /// True if at least one action references this message as evidence.
    pub has_action: bool,
}

/// A single action as rendered in the actions list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDto {
    pub id: i64,
    pub title: String,
    pub details: Option<String>,
    pub confidence: Confidence,
    pub status: ActionStatus,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds; `None` means no due date.
    pub due_at: Option<i64>,
    /// Number of evidence messages linked.
    pub evidence_count: i64,
    /// Channel name (e.g. `INBOX`) for context.
    pub channel_name: Option<String>,
    /// Source display name (e.g. `fastmail`).
    pub source_name: Option<String>,
}
