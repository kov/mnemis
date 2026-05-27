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
