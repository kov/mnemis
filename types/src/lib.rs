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

/// Why a `dismissal_feedback` row exists — both feed into the extractor as
/// negative examples but the rendering differs slightly so the model knows
/// what the user objected to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind {
    /// User said "this isn't really an action item."
    Dismissed,
    /// User undid an auto-claim with a comment — the auto-claim was wrong.
    WrongAutoClaim,
}

impl FeedbackKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dismissed => "dismissed",
            Self::WrongAutoClaim => "wrong_auto_claim",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dismissed" => Some(Self::Dismissed),
            "wrong_auto_claim" => Some(Self::WrongAutoClaim),
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

/// Profile editor wire shape. `identifiers` is a flat list across kinds so
/// the UI can render and edit them as `(kind, value)` tuples; the engine
/// reconciles them against `contact_identifiers` rows on save.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserProfileDto {
    pub display_name: String,
    pub custom_prompt: Option<String>,
    pub identifiers: Vec<ProfileIdentifier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileIdentifier {
    /// `'email'|'mattermost_handle'|'discord_id'|'phone'` — anything
    /// `contact_identifiers.kind` accepts.
    pub kind: String,
    pub value: String,
}

/// LLM config view/edit shape. Matches `engine::config::LlmSection` plus a
/// `config_path` so the UI can tell the user where edits land. The bearer
/// token is sent both ways — the form omits it when blank.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmConfigDto {
    pub base_url: String,
    pub chat_model: String,
    pub embedding_model: String,
    pub bearer_token: Option<String>,
    pub config_path: String,
}

/// One row in the Settings → Sources table. Health is duplicated from
/// `SourceStatus` for convenience so the settings page doesn't need a second
/// fetch to colour the row. `muted` is true when *every* channel on the
/// source is muted — i.e. the source contributes nothing on sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRowDto {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub muted: bool,
    pub health: SourceHealth,
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
}

/// One channel under a source — surfaces the per-channel mute knob so a user
/// with a chatty mailbox can silence specific folders without disabling the
/// whole account. `message_count` lets the UI hint which channels are
/// actually pulling weight before the user decides what to silence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRowDto {
    pub id: i64,
    pub source_id: i64,
    pub external_id: String,
    pub name: String,
    pub kind: String,
    pub muted: bool,
    pub last_synced_at: Option<i64>,
    pub message_count: i64,
}

/// A resolution the extractor proposed but isn't confident enough to apply
/// on its own. The UI surfaces these in a "Suggested resolutions" panel
/// where the user can confirm (apply) or reject (discard).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingResolutionDto {
    pub action_id: i64,
    pub action_title: String,
    /// `"done"` or `"cancelled"` — what the agent proposes the new status be.
    pub suggested_status: String,
    pub confidence: Confidence,
    pub rationale: Option<String>,
    /// Unix seconds — when the agent emitted the suggestion.
    pub suggested_at: i64,
}

/// Map a long, raw error string from the engine (typically an anyhow chain
/// from `SyncOutcome.errors`) into a short, human-friendly one-liner suitable
/// for the toast. Falls back to a truncated copy of the raw text for
/// patterns we haven't seen yet — so we never hide unknown failures, just
/// shorten them.
pub fn summarize_sync_error(raw: &str) -> String {
    // Context window blown by oversize prompt.
    if let Some(s) = parse_context_window(raw) {
        return s;
    }
    // IMAP source had no credentials wired up.
    if raw.contains("missing IMAP connection settings") {
        return "Missing IMAP connection settings — check this source's config.".to_string();
    }
    // Transport-level network errors from reqwest. Check the *specific*
    // signatures before the generic "error sending request" catch-all —
    // every reqwest transport error carries that phrase, so it would
    // otherwise mask the more useful diagnoses below.
    //
    // "connection closed before message completed" means the server
    // accepted the request and then dropped the TCP connection mid-reply —
    // classically an out-of-memory crash partway through prefill. With a
    // local model that almost always means the prompt window blew past the
    // server's context limit; surface that hint.
    if raw.contains("connection closed before message completed") {
        return "The LLM server dropped the connection mid-request — it likely ran out of \
                memory (the message window may be too large for the model's context)."
            .to_string();
    }
    if raw.contains("connection refused") || raw.contains("Connection refused") {
        return "Connection refused — the LLM/embedding server isn't running or crashed."
            .to_string();
    }
    if raw.contains("error sending request for url") {
        return "Could not reach the LLM/embedding server (network or DNS).".to_string();
    }
    // Auth errors.
    if raw.contains("401") || raw.contains("Unauthorized") {
        return "Authentication failed (HTTP 401) — check bearer token.".to_string();
    }
    // Unknown: truncate so the toast stays readable but the user still sees
    // *something* actionable.
    const MAX: usize = 200;
    if raw.len() <= MAX {
        raw.to_string()
    } else {
        let mut end = MAX;
        while !raw.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &raw[..end])
    }
}

/// "Prompt too long: 154806 tokens exceeds max context window of 131072 tokens"
fn parse_context_window(raw: &str) -> Option<String> {
    let needle = "Prompt too long: ";
    let i = raw.find(needle)? + needle.len();
    let rest = &raw[i..];
    let used: u32 = rest.split(' ').next()?.parse().ok()?;
    let max_needle = "max context window of ";
    let j = raw.find(max_needle)? + max_needle.len();
    let rest = &raw[j..];
    let max: u32 = rest.split(' ').next()?.parse().ok()?;
    Some(format!(
        "Window too large ({used} > {max} tokens) — message body or backlog too big."
    ))
}

#[cfg(test)]
mod summarize_tests {
    use super::*;

    #[test]
    fn classifies_context_window_overflow() {
        let raw = "source 'fastmail' channel 'INBOX/Lembrar': extract failed: channel 6: \
                   LLM send failed on turn 0: LLM API error (HTTP 400 Bad Request): \
                   {\"error\":{\"message\":\"Prompt too long: 154806 tokens exceeds max \
                   context window of 131072 tokens\",\"type\":\"invalid_request_error\"}}";
        let s = summarize_sync_error(raw);
        assert!(s.contains("154806"), "got: {s}");
        assert!(s.contains("131072"), "got: {s}");
        assert!(s.to_lowercase().contains("too large"), "got: {s}");
    }

    #[test]
    fn classifies_missing_imap_settings() {
        let raw = "source 'test-account' (id=1): missing IMAP connection settings: \
                   no rows returned by a query that expected to return at least one row";
        let s = summarize_sync_error(raw);
        assert!(s.contains("Missing IMAP"));
        assert!(s.len() < 200);
    }

    #[test]
    fn classifies_network_failure() {
        let raw = "error sending request for url (http://alface:1234/v1/embeddings): \
                   connection error";
        let s = summarize_sync_error(raw);
        assert!(s.to_lowercase().contains("could not reach"));
    }

    #[test]
    fn classifies_connection_dropped_mid_request() {
        // The signature an OOM/context-overflow produces: the server takes
        // the request, then drops the socket before replying. Must NOT
        // collapse into the generic "could not reach" message even though
        // it also contains "error sending request for url".
        let raw = "channel 6: LLM send failed on turn 0: error sending request for url \
                   (http://alface:1234/v1/responses): client error (SendRequest): \
                   connection closed before message completed";
        let s = summarize_sync_error(raw);
        assert!(s.to_lowercase().contains("ran out of memory"), "got: {s}");
        assert!(s.to_lowercase().contains("context"), "got: {s}");
    }

    #[test]
    fn connection_refused_beats_generic_network_branch() {
        // reqwest wraps ECONNREFUSED inside "error sending request for url";
        // the specific message should still win.
        let raw = "error sending request for url (http://alface:1234/v1/embeddings): \
                   client error (Connect): tcp connect error: Connection refused (os error 111)";
        let s = summarize_sync_error(raw);
        assert!(s.to_lowercase().contains("refused"), "got: {s}");
    }

    #[test]
    fn falls_back_to_truncated_raw_for_unknown_patterns() {
        let raw = format!("some weird new failure: {}", "x".repeat(500));
        let s = summarize_sync_error(&raw);
        // 200 ASCII chars of body + 3-byte UTF-8 ellipsis.
        assert!(s.len() <= 203, "got len {}", s.len());
        assert!(s.ends_with('…'));
    }

    #[test]
    fn passes_short_unknown_errors_through_unchanged() {
        let raw = "boom";
        assert_eq!(summarize_sync_error(raw), "boom");
    }
}
