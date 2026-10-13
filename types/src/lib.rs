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

/// The OS-preferred color scheme, mirrored from the platform appearance source
/// (XDG portal `org.freedesktop.appearance/color-scheme` on Linux,
/// `AppleInterfaceStyle` on macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColorScheme {
    /// No preference — the frontend should fall back to `prefers-color-scheme`.
    #[default]
    NoPreference,
    Light,
    Dark,
}

/// System appearance read from the host desktop: the accent color (as
/// `"#rrggbb"`, or `None` when the OS exposes none) and the preferred color
/// scheme. The frontend injects `--accent` from this and drives the theme.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppearanceDto {
    pub accent: Option<String>,
    pub color_scheme: ColorScheme,
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
    /// From a sender that looks like an impostor: shown as a warning, not a
    /// to-do, until the user accepts it (→ pending) or dismisses it.
    Suspicious,
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
            "suspicious" => Some(Self::Suspicious),
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

/// A message's full detail for the reading pane: the complete body plus the
/// actions mnemis extracted from it. Loaded on demand (the list wire stays
/// snippet-only) when a row is selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDetailDto {
    pub id: i64,
    pub subject: Option<String>,
    /// Full body text. IMAP bodies are stored as plain `text`.
    pub body: String,
    /// `'text'` | `'markdown'` | `'html'` — currently always `'text'`.
    pub body_format: String,
    pub author_display: Option<String>,
    /// The author's handle/address (e.g. email), when known.
    pub author_addr: Option<String>,
    /// Unix seconds.
    pub posted_at: i64,
    pub channel_name: Option<String>,
    pub source_name: Option<String>,
    /// Actions extracted from this message (the "mnemis extracted an action"
    /// callout in the reading pane).
    pub actions: Vec<MessageActionRef>,
}

/// A compact reference to an action that a message produced, shown in the
/// reading pane's extracted-action callout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageActionRef {
    pub id: i64,
    pub title: String,
    pub confidence: Confidence,
    pub status: ActionStatus,
    /// Unix seconds, when the action has a reminder due date.
    pub due_at: Option<i64>,
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
    /// When on the due day the reminder alerts, in minutes after local
    /// midnight; `None`: the default morning time.
    #[serde(default)]
    pub remind_minutes: Option<i64>,
    /// Unix seconds: when the action's primary evidence message (the email it
    /// came from) was sent. `None` when it has no primary evidence.
    #[serde(default)]
    pub source_posted_at: Option<i64>,
    /// Internal id of that same message, for showing the original email.
    #[serde(default)]
    pub source_message_id: Option<i64>,
    /// The mail thread that message belongs to, shown whole with the to-do.
    #[serde(default)]
    pub source_thread_id: Option<i64>,
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

/// Which LLM API mnemis talks to for chat and extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LlmProvider {
    /// An OpenAI *Responses*-API server (`/v1/responses`), e.g. omlx.
    #[default]
    OpenaiResponses,
    /// Claude via the Anthropic Messages API.
    Anthropic,
    /// Apple's Foundation Models, on device or on Private Cloud Compute. Only
    /// the iOS app can reach them; `chat_model` picks which
    /// ([`APPLE_ON_DEVICE`] or [`APPLE_PRIVATE_CLOUD`]).
    AppleIntelligence,
}

/// `chat_model` values for [`LlmProvider::AppleIntelligence`].
pub const APPLE_ON_DEVICE: &str = "on-device";
pub const APPLE_PRIVATE_CLOUD: &str = "private-cloud-compute";

impl LlmProvider {
    /// The providers the desktop app can use (Apple Intelligence is iOS-only).
    pub const ALL: [LlmProvider; 2] = [LlmProvider::OpenaiResponses, LlmProvider::Anthropic];

    pub fn as_str(self) -> &'static str {
        match self {
            LlmProvider::OpenaiResponses => "openai-responses",
            LlmProvider::Anthropic => "anthropic",
            LlmProvider::AppleIntelligence => "apple-intelligence",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LlmProvider::OpenaiResponses => "OpenAI Responses API (omlx, …)",
            LlmProvider::Anthropic => "Claude (Anthropic API)",
            LlmProvider::AppleIntelligence => "Apple Intelligence",
        }
    }

    pub fn from_wire(s: &str) -> LlmProvider {
        match s {
            "anthropic" => LlmProvider::Anthropic,
            "apple-intelligence" => LlmProvider::AppleIntelligence,
            _ => LlmProvider::OpenaiResponses,
        }
    }
}

/// LLM config view/edit shape. Matches `engine::config::LlmSection` plus a
/// `config_path` so the UI can tell the user where edits land. The bearer
/// token is sent both ways — the form omits it when blank.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmConfigDto {
    #[serde(default)]
    pub provider: LlmProvider,
    /// Empty for the Anthropic provider means the public API endpoint.
    #[serde(default)]
    pub base_url: String,
    pub chat_model: String,
    pub bearer_token: Option<String>,
    /// Coarse chain-of-thought budget knob; always applied (defaults to
    /// [`ThinkingLevel::Medium`] when absent from an older config).
    #[serde(default)]
    pub thinking_level: ThinkingLevel,
    /// Write-only: a new Claude API key to store in the OS keychain. Never
    /// sent back to the UI; `None`/blank leaves the stored key alone.
    #[serde(default)]
    pub anthropic_api_key: Option<String>,
    /// Read-only: whether a Claude API key is available (keychain or the
    /// `ANTHROPIC_API_KEY` env var).
    #[serde(default)]
    pub anthropic_api_key_set: bool,
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

/// What an outgoing mail is: written from scratch, or answering or passing on
/// a message the user received.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftKind {
    New,
    Reply,
    ReplyAll,
    Forward,
}

impl DraftKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reply => "reply",
            Self::ReplyAll => "reply_all",
            Self::Forward => "forward",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "new" => Some(Self::New),
            "reply" => Some(Self::Reply),
            "reply_all" => Some(Self::ReplyAll),
            "forward" => Some(Self::Forward),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Draft,
    Sent,
    Discarded,
}

impl DraftStatus {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(Self::Draft),
            "sent" => Some(Self::Sent),
            "discarded" => Some(Self::Discarded),
            _ => None,
        }
    }
}

/// An outgoing mail, composed by the user or drafted by the chat agent. Only
/// the user sends it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftDto {
    pub id: i64,
    /// The account it is sent from.
    pub source_id: Option<i64>,
    pub kind: DraftKind,
    /// The message replied to or forwarded.
    pub message_id: Option<i64>,
    /// The chat whose agent drafted it.
    pub chat_id: Option<i64>,
    /// The name mail goes out under.
    pub from_name: String,
    /// The address it is sent as; empty when there is none to send as.
    pub from_address: String,
    /// The user's addresses to pick from: their profile's and each account's.
    pub from_choices: Vec<String>,
    /// Comma-separated addresses, as typed.
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub status: DraftStatus,
    /// Why the last send failed.
    pub error: Option<String>,
    /// Drafted by the chat agent rather than the user.
    pub by_agent: bool,
    /// Unix seconds.
    pub updated_at: i64,
    pub sent_at: Option<i64>,
}

/// How a mail account sends. Until saved, the values are guessed from its
/// IMAP settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpSettingsDto {
    pub source_id: i64,
    pub server: String,
    pub port: u16,
    /// STARTTLS on a plain connection instead of TLS from the start.
    pub starttls: bool,
    pub username: String,
    /// A password of its own is stored, rather than the IMAP one reused.
    pub own_password: bool,
    pub from_name: String,
    pub from_address: String,
    /// File a copy of sent mail in the account's sent folder.
    pub save_sent: bool,
    /// False while the values are only guesses.
    pub saved: bool,
}

/// What a mail folder is for, from its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxRole {
    Inbox,
    Drafts,
    Sent,
    Archive,
    Junk,
    Trash,
    Other,
}

/// A mail folder for the reader's sidebar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxDto {
    pub channel_id: i64,
    pub source_id: i64,
    pub account: String,
    /// Display name (the leaf of `path`).
    pub name: String,
    /// The full folder path, decoded; `/` or `.` separate levels.
    pub path: String,
    pub role: MailboxRole,
    pub unread: i64,
}

/// What kind of mail an email is, as the briefing sorts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailCategory {
    /// A person writing to the user.
    Person,
    Newsletter,
    /// Automated alerts, service and social notifications.
    Notification,
    /// Receipts, bills, orders, bookings, codes.
    Transactional,
    /// Mailing-list discussion.
    List,
    Other,
}

impl MailCategory {
    pub const ALL: [Self; 6] = [
        Self::Person,
        Self::Newsletter,
        Self::Notification,
        Self::Transactional,
        Self::List,
        Self::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Newsletter => "newsletter",
            Self::Notification => "notification",
            Self::Transactional => "transactional",
            Self::List => "list",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// The briefing for the last `days` days.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefingDto {
    pub days: i64,
    /// Unix seconds the period starts at; it ends now.
    pub since: i64,
    /// Emails received (a copy in a second folder counts once).
    pub received: i64,
    pub unread: i64,
    /// How many of each category, most first; emails not described yet are
    /// left out (see `untagged`).
    pub categories: Vec<CategoryCountDto>,
    pub untagged: i64,
    /// Emails per account, most first.
    pub accounts: Vec<AccountCountDto>,
    /// The emails most worth reading, most important first.
    pub worth_reading: Vec<BriefingMailDto>,
    /// Open watches, then the ones that matched in the period.
    pub watches: Vec<BriefingWatchDto>,
    /// Open to-dos that came from the period's mail.
    pub new_todos: Vec<BriefingTodoDto>,
    /// To-dos resolved in the period.
    pub resolved_todos: i64,
    pub suspicious: Vec<BriefingMailDto>,
    /// What the agent wrote about the period, once it has.
    pub written: Option<WrittenBriefingDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryCountDto {
    pub category: MailCategory,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountCountDto {
    pub account: String,
    pub count: i64,
}

/// An email as the briefing lists it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefingMailDto {
    pub message_id: i64,
    pub thread_id: Option<i64>,
    pub from: String,
    pub subject: String,
    pub received_at: i64,
    pub unread: bool,
    pub category: Option<MailCategory>,
    /// 0 noise, 1 skim, 2 worth reading, 3 important.
    pub interest: Option<i64>,
    /// Why it matters, from tagging or the briefing.
    pub reason: String,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefingWatchDto {
    pub watch_id: i64,
    pub looking_for: String,
    /// False once it matched.
    pub open: bool,
    pub expires_at: Option<i64>,
    /// The email it matched, when it did.
    pub matched: Option<BriefingMailDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefingTodoDto {
    pub action_id: i64,
    pub title: String,
    pub due_at: Option<i64>,
}

/// What the briefing agent wrote about a period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrittenBriefingDto {
    pub generated_at: i64,
    pub summary: String,
    /// The emails it says to read, most important first.
    pub highlights: Vec<BriefingMailDto>,
    /// Emails in the period that arrived after it was written.
    pub newer_mail: i64,
}

/// Which threads a list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailScope {
    /// Every account's inbox.
    AllInboxes,
    Mailbox {
        channel_id: i64,
    },
}

/// A row of a thread list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummaryDto {
    pub thread_id: i64,
    pub subject: String,
    /// Who wrote in it, earliest first; "Me" for the user.
    pub participants: Vec<String>,
    /// The start of the latest message, quotes left out.
    pub snippet: String,
    /// Unix seconds of the latest message.
    pub latest_at: i64,
    pub message_count: i64,
    pub unread_count: i64,
    pub has_attachments: bool,
    pub flagged: bool,
    /// Open to-dos that came from its mail.
    pub todo_count: i64,
    pub account: String,
    /// Why the latest message looks like it isn't from who it claims
    /// ("dated 4 months after it arrived", …); empty when nothing does.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// One message of a thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMessageDto {
    pub message_id: i64,
    pub from_name: Option<String>,
    pub from_address: Option<String>,
    /// "Name <address>" or the address, as the headers had them.
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub posted_at: i64,
    pub subject: String,
    pub snippet: String,
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub has_attachments: bool,
    /// The folders holding it.
    pub folders: Vec<String>,
    /// The full message is in the cache: it renders without the network.
    pub downloaded: bool,
    /// Written by the user.
    pub from_me: bool,
    /// Why it looks like it isn't from who it claims; empty when nothing
    /// does.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// An open to-do that came from a thread's mail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadTodoDto {
    pub action_id: i64,
    pub title: String,
    pub status: ActionStatus,
    pub due_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadDto {
    pub thread_id: i64,
    pub subject: String,
    /// Oldest first.
    pub messages: Vec<ThreadMessageDto>,
    pub todos: Vec<ThreadTodoDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentDto {
    /// The part to ask for when opening it.
    pub part: u32,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
}

/// A message as a page for a web view (see the engine's `mail::render`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedMessageDto {
    pub html: String,
    /// Remote resources held back; offer "Load images" only when non-zero.
    pub remote_resources: u32,
    pub attachments: Vec<AttachmentDto>,
    /// False when this is the stored text alone: fetch the thread, then
    /// render again.
    pub complete: bool,
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
             Fix chat_model in config."
        ),
        (Some(m), None) => {
            format!("Model \"{m}\" isn't on the LLM server — fix chat_model in config.")
        }
        _ => "The configured model isn't on the LLM server — fix chat_model \
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
        let dto: LlmConfigDto =
            serde_json::from_str(r#"{"base_url":"u","chat_model":"c","config_path":"p"}"#).unwrap();
        assert_eq!(dto.thinking_level, ThinkingLevel::Medium);
    }
}
