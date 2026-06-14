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
    /// CalDAV reminder sync state, when this action is (or was) a reminder:
    /// `synced` | `dirty` | `needs_review`. `None` means it isn't a reminder.
    pub sync_status: Option<String>,
}

/// CalDAV account as shown in settings. Secrets (the app-specific password)
/// never cross this boundary — they live in the OS keychain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaldavAccountDto {
    pub base_url: String,
    pub username: String,
    /// The chosen task collection's URL, once discovered + selected.
    pub collection_url: Option<String>,
    /// Human-readable name of the chosen collection (e.g. `Reminders`).
    pub collection_name: Option<String>,
    /// True when an account is stored (so the UI can show connected vs. empty).
    pub configured: bool,
}

/// One VTODO-capable collection found during discovery, for the picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaldavCollectionDto {
    pub url: String,
    pub display_name: Option<String>,
}

/// Result of a CalDAV sync run, surfaced to the UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaldavSyncDto {
    pub created: usize,
    pub pushed: usize,
    pub pulled: usize,
    pub removed: usize,
    pub conflicts: usize,
    pub errors: Vec<String>,
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

/// How much chain-of-thought budget the model gets on every LLM call,
/// exposed as a coarse level rather than a raw token count — the right number
/// is model- and box-dependent, so the labels map to budgets the server
/// enforces. A budget is **always** sent so models that default to
/// thinking-off (e.g. Gemma) still get a chance to reason. See the
/// `omlx-server` memory for the sizing rationale (generous-enough-to-rarely-
/// truncate; hitting the cap spills reasoning into the answer and lengthens
/// generation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// 512 tokens — fastest; may truncate reasoning on harder prompts.
    Low,
    /// 1024 tokens — the default: generous for extraction, adequate for chat.
    #[default]
    Medium,
    /// 2048 tokens — headroom for multi-step chat reasoning.
    High,
    /// 4096 tokens — matches omlx's own upper default; rarely truncates.
    ExtraHigh,
}

impl ThinkingLevel {
    /// Every level in ascending order — for rendering the settings dropdown.
    pub const ALL: [ThinkingLevel; 4] = [
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::ExtraHigh,
    ];

    /// The thinking-token budget this level maps to — the hard cap omlx's
    /// `ThinkingBudgetProcessor` enforces on the thinking phase.
    pub fn budget_tokens(self) -> u32 {
        match self {
            ThinkingLevel::Low => 512,
            ThinkingLevel::Medium => 1024,
            ThinkingLevel::High => 2048,
            ThinkingLevel::ExtraHigh => 4096,
        }
    }

    /// Stable wire/config token (`snake_case`), matching the serde encoding.
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::ExtraHigh => "extra_high",
        }
    }

    /// Human label for the settings dropdown.
    pub fn label(self) -> &'static str {
        match self {
            ThinkingLevel::Low => "Low",
            ThinkingLevel::Medium => "Medium",
            ThinkingLevel::High => "High",
            ThinkingLevel::ExtraHigh => "Extra high",
        }
    }

    /// Parse a wire/config token back to a level, falling back to the default
    /// for anything unrecognized (forward-compatible with config typos).
    pub fn from_wire(s: &str) -> ThinkingLevel {
        match s {
            "low" => ThinkingLevel::Low,
            "high" => ThinkingLevel::High,
            "extra_high" => ThinkingLevel::ExtraHigh,
            _ => ThinkingLevel::Medium,
        }
    }
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
    /// Coarse chain-of-thought budget knob; always applied (defaults to
    /// [`ThinkingLevel::Medium`] when absent from an older config).
    #[serde(default)]
    pub thinking_level: ThinkingLevel,
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

/// One persistent conversation in the chat view. `seeded_from_*` records the
/// entity a "Talk about this" chat was started from (null for a blank chat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatDto {
    pub id: i64,
    /// Null until the first user message gives it a title.
    pub title: Option<String>,
    /// `'message' | 'action' | 'memory' | 'report'`, or null for a blank chat.
    pub seeded_from_kind: Option<String>,
    pub seeded_from_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Hidden from the default chat list; still listed when the user opts to
    /// show archived chats.
    pub archived: bool,
}

/// One turn in a chat transcript. A single model response may produce several
/// turns: an assistant message, plus one row per tool call (role `'assistant'`,
/// `tool_name`/`tool_call_id` set, `content` = arguments) and one per tool
/// result (role `'tool'`, `content` = output). The model's reasoning, when
/// present, rides on the assistant turn it preceded — shown in the UI but never
/// replayed into the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTurnDto {
    pub id: i64,
    /// `'user' | 'assistant' | 'tool'`.
    pub role: String,
    /// Message text, tool-call arguments, or tool output depending on `role`.
    pub content: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    /// Captured chain-of-thought for this turn, if any. Display-only.
    pub reasoning: Option<String>,
    pub created_at: i64,
}

/// A live event streamed from a `send_chat_message` turn as the agent loop
/// runs. Each is persisted to SQLite *before* it's emitted, so the channel is
/// purely a display accelerator — dropping it never loses data. Tagged by
/// `kind` so the WASM side can match without a discriminant field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatEvent {
    /// A chunk of assistant-visible text as it streams in. Transient and *not*
    /// persisted — a live-rendering accelerator. The authoritative copy arrives
    /// as the `AssistantMessage` for the completed turn, which the UI then shows
    /// from the persisted transcript; the accumulated deltas are dropped at that
    /// point so nothing renders twice.
    Delta { text: String },
    /// The model's reasoning for the turn (display-only, never replayed).
    Reasoning { text: String },
    /// Assistant-visible text.
    AssistantMessage { text: String },
    /// The agent invoked a tool with these JSON arguments.
    ToolCall { name: String, arguments: String },
    /// The tool returned (the raw JSON string the model will see next).
    ToolResult { name: String, output: String },
    /// The conversation is being condensed to fit the model's context window.
    /// Transient and *not* persisted (it carries no transcript) — a UI hint
    /// that the next answer will take a little longer. Cleared by the next
    /// real event.
    Compacting,
    /// The turn finished cleanly — no more events are coming.
    Done,
    /// The turn failed; `message` is a human-readable reason.
    Error { message: String },
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
    // A configured model that doesn't exist on the server (typo / rollout).
    if let Some(s) = parse_model_not_found(raw) {
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

/// True when an error chain is the omlx "configured model doesn't exist"
/// failure: an HTTP 404 whose body carries `not_found_error` and a message like
/// `Model 'X' not found. Available models: …`. A missing model fails every
/// channel of a source identically, so the orchestrator uses this to report it
/// once at the source level instead of once per channel.
pub fn is_model_not_found(raw: &str) -> bool {
    raw.contains("not_found_error") || raw.contains("not found. Available models")
}

/// Turn an omlx model-not-found 404 into an actionable line naming the missing
/// model and (when the server listed them) the models that *are* available.
fn parse_model_not_found(raw: &str) -> Option<String> {
    if !is_model_not_found(raw) {
        return None;
    }
    let model = slice_between(raw, "Model '", "'");
    let available = raw.find("Available models: ").map(|i| {
        let rest = &raw[i + "Available models: ".len()..];
        // The list runs to the end of the JSON string ("/}) — stop there.
        let end = rest.find(['"', '}']).unwrap_or(rest.len());
        rest[..end].trim().trim_end_matches(['.', ' ']).to_string()
    });
    let available = available.filter(|s| !s.is_empty());
    Some(match (model, available) {
        (Some(m), Some(a)) => format!(
            "Model \"{m}\" isn't on the LLM server. Available: {a}. \
             Fix chat_model / embedding_model in config."
        ),
        (Some(m), None) => format!(
            "Model \"{m}\" isn't on the LLM server — fix chat_model / embedding_model in config."
        ),
        _ => "The configured model isn't on the LLM server — fix chat_model / embedding_model \
              in config."
            .to_string(),
    })
}

/// The text between the first `start` and the next `end` after it, if both
/// are present.
fn slice_between(s: &str, start: &str, end: &str) -> Option<String> {
    let i = s.find(start)? + start.len();
    let rest = &s[i..];
    let j = rest.find(end)?;
    Some(rest[..j].to_string())
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

    #[test]
    fn classifies_model_not_found_and_names_available_models() {
        let raw = "source 'alface' channel 'INBOX': extract failed: channel 6: \
            LLM API error (HTTP 404 Not Found): \
            {\"error\":{\"type\":\"not_found_error\",\"message\":\"Model 'qwen3.5-typo' not found. \
            Available models: qwen3.5-35b-a3b-4bit, nomic-embed-text\"}}";
        let s = summarize_sync_error(raw);
        assert!(s.contains("qwen3.5-typo"), "names the missing model: {s}");
        assert!(
            s.contains("qwen3.5-35b-a3b-4bit"),
            "lists what's available: {s}"
        );
        assert!(s.to_lowercase().contains("config"), "points at config: {s}");
    }

    #[test]
    fn is_model_not_found_is_specific() {
        assert!(is_model_not_found(
            "...{\"type\":\"not_found_error\",\"message\":\"...\"}"
        ));
        assert!(is_model_not_found(
            "Model 'x' not found. Available models: y"
        ));
        assert!(!is_model_not_found("connection refused"));
        assert!(!is_model_not_found("Prompt too long: 9 exceeds"));
    }
}

#[cfg(test)]
mod thinking_level_tests {
    use super::*;

    #[test]
    fn default_is_medium_and_budgets_ascend() {
        assert_eq!(ThinkingLevel::default(), ThinkingLevel::Medium);
        let budgets: Vec<u32> = ThinkingLevel::ALL
            .iter()
            .map(|l| l.budget_tokens())
            .collect();
        assert_eq!(budgets, vec![512, 1024, 2048, 4096]);
        assert!(
            budgets.windows(2).all(|w| w[0] < w[1]),
            "must be strictly ascending"
        );
    }

    #[test]
    fn wire_token_round_trips_through_serde_and_from_wire() {
        for level in ThinkingLevel::ALL {
            // as_str matches the serde encoding ...
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(json, format!("\"{}\"", level.as_str()));
            // ... and from_wire is its inverse.
            assert_eq!(ThinkingLevel::from_wire(level.as_str()), level);
        }
        // Unknown tokens degrade to the default rather than erroring — keeps a
        // hand-edited or future-written config from bricking the form.
        assert_eq!(ThinkingLevel::from_wire("bogus"), ThinkingLevel::Medium);
    }

    #[test]
    fn dto_defaults_thinking_level_when_absent() {
        // An older config payload without `thinking_level` still deserializes,
        // landing on the default so a budget is always resolvable.
        let dto: LlmConfigDto = serde_json::from_str(
            r#"{"base_url":"u","chat_model":"c","embedding_model":"e","config_path":"p"}"#,
        )
        .unwrap();
        assert_eq!(dto.thinking_level, ThinkingLevel::Medium);
    }
}
