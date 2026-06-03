use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;

use crate::llm::ToolDef;

/// How wide the message-touching tools may look. Extraction runs are bound to
/// the one source they're processing (`Source`); the chat agent ranges over
/// everything the user has ingested (`Global`). Each handler turns this into an
/// optional `source_id` SQL filter (`None` = no restriction).
#[derive(Debug, Clone, Copy)]
pub enum ToolScope {
    Source(i64),
    Global,
}

impl ToolScope {
    /// The `source_id` to filter message lookups by, or `None` for no
    /// restriction. Bound into `(? IS NULL OR c.source_id = ?)` clauses.
    fn source_filter(self) -> Option<i64> {
        match self {
            ToolScope::Source(id) => Some(id),
            ToolScope::Global => None,
        }
    }
}

/// Per-run cap on how many characters of message bodies the agent may pull via
/// `fetch_messages`. Because turns are threaded server-side (append-only via
/// `previous_response_id`), every fetched body stays in context for the rest of
/// the run; without a cap a fetch-happy run would re-create the context
/// overflow the batching is meant to prevent. `split_into_batches` already
/// sizes each batch so all its bodies fit under this cap, so in the common case
/// the model can fetch its entire batch — this is the backstop against
/// pathological cases (re-fetches, pulling many `search_messages` hits).
#[derive(Debug)]
pub struct FetchBudget {
    used_chars: usize,
    cap_chars: usize,
}

impl FetchBudget {
    pub fn new(cap_chars: usize) -> Self {
        Self {
            used_chars: 0,
            cap_chars,
        }
    }

    /// An effectively unlimited budget — for tests and direct tool calls that
    /// don't exercise the fetch path.
    pub fn unlimited() -> Self {
        Self::new(usize::MAX)
    }

    /// Charge `chars` against the budget, returning whether it fit (and
    /// recording it if so). The first charge of a run always succeeds — even if
    /// it alone exceeds the cap — so a run can always make progress on at least
    /// one body (mirrors `split_into_batches`' lone-oversize-message rule).
    fn try_charge(&mut self, chars: usize) -> bool {
        if self.used_chars > 0 && self.used_chars.saturating_add(chars) > self.cap_chars {
            return false;
        }
        self.used_chars = self.used_chars.saturating_add(chars);
        true
    }
}

// Tool parameter schemas, shared by the extraction and chat tool sets so the
// two can't drift. The JSON Schemas are identical across surfaces; only the
// human-readable descriptions differ (see `definitions` vs `chat_definitions`).

fn search_messages_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"]
    })
}

fn fetch_messages_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "external_ids": { "type": "array", "items": { "type": "string" }, "minItems": 1 }
        },
        "required": ["external_ids"]
    })
}

fn list_messages_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "since": {
                "type": "string",
                "description": "ISO 8601 date (2026-05-28) or datetime; only messages at or after this. Omit for no lower bound."
            },
            "until": {
                "type": "string",
                "description": "ISO 8601 date or datetime; only messages strictly before this. Omit for no upper bound."
            },
            "before": {
                "type": "string",
                "description": "Paging cursor: pass the `next_before` from the previous call to get the next, older page. Omit on the first call."
            },
            "limit": {
                "type": "integer",
                "description": "Max messages to return (default 25, max 100)."
            }
        }
    })
}

fn record_action_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "title":      { "type": "string", "description": "imperative, ≤80 chars" },
            "details":    { "type": "string", "description": "1-3 sentences of context" },
            "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
            "rationale":  { "type": "string", "description": "≤200 chars, why this is an action" },
            "due_at":     { "type": ["string", "null"], "description": "ISO 8601 timestamp or null" },
            "evidence_external_ids": { "type": "array", "items": { "type": "string" }, "minItems": 1 }
        },
        "required": ["title", "details", "confidence", "rationale", "evidence_external_ids"]
    })
}

fn resolve_action_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "action_id":  { "type": "string", "description": "A-N id" },
            "status":     { "type": "string", "enum": ["done", "cancelled"] },
            "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
            "rationale":  { "type": "string", "description": "≤200 chars, what proves it's done" },
            "evidence_external_ids": { "type": "array", "items": { "type": "string" }, "minItems": 1 }
        },
        "required": ["action_id", "status", "confidence", "rationale", "evidence_external_ids"]
    })
}

fn update_action_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "action_id":  { "type": "string", "description": "the A-N id returned by record_action or shown in Existing actions" },
            "title":      { "type": "string" },
            "details":    { "type": "string" },
            "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
            "rationale":  { "type": "string" },
            "due_at":     { "type": ["string", "null"], "description": "ISO 8601 timestamp or null" },
            "evidence_external_ids": {
                "type": "array",
                "items": { "type": "string" },
                "description": "additional evidence to append; not replaced"
            }
        },
        "required": ["action_id"]
    })
}

fn get_action_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "action_id": { "type": "string", "description": "A-N id (or bare N)" }
        },
        "required": ["action_id"]
    })
}

fn list_actions_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "include_resolved": {
                "type": "boolean",
                "description": "also include done/cancelled/dismissed (default false)"
            }
        }
    })
}

/// The extraction agent's tool set. It has no `get_action` — the existing
/// actions for the channel are injected into its prompt directly.
pub fn definitions() -> Vec<ToolDef> {
    vec![
        ToolDef::function(
            "search_messages".to_string(),
            "Keyword search across messages in this source. Returns up to 10 matches \
             with external_id, channel, posted_at, and a short snippet."
                .to_string(),
            search_messages_params(),
        ),
        ToolDef::function(
            "fetch_messages".to_string(),
            "Fetch the full bodies of one or more messages by external_id. Batch the ids — \
             pass several at once to save round-trips. The window shows only snippets, so \
             call this before recording an action whenever a snippet alone doesn't confirm \
             the ask. Returns {\"messages\": [...], \"not_found\": [...]} plus \"over_budget\" \
             ids and a notice if the per-run fetch budget is hit."
                .to_string(),
            fetch_messages_params(),
        ),
        ToolDef::function(
            "record_action".to_string(),
            "Record one action item. evidence_external_ids must reference at least one \
             message visible in the window or fetched via fetch_messages. Returns \
             {\"action_id\": \"A-N\", \"status\": ...} — keep the action_id if you may \
             want to amend the same action later in this response (use update_action)."
                .to_string(),
            record_action_params(),
        ),
        ToolDef::function(
            "resolve_action".to_string(),
            "Mark a prior action as done or cancelled because the window proves it. \
             Identify the action by its A-N id; provide evidence_external_ids that \
             show the resolution (≥1). high-confidence applies immediately; medium/low \
             queue the suggestion for the user to confirm — same gating as record_action."
                .to_string(),
            resolve_action_params(),
        ),
        ToolDef::function(
            "update_action".to_string(),
            "Amend an existing action (one you just recorded, or one from the Existing list). \
             Identify it by its A-N id. Only the fields you pass are changed; new evidence \
             is appended, not replaced. Use this instead of calling record_action a second \
             time for the same underlying item."
                .to_string(),
            update_action_params(),
        ),
    ]
}

/// The chat agent's tool set: read-and-act across *all* sources, plus
/// `get_action` to inspect a specific action the user references. The mutating
/// tools share the extraction handlers (and their confidence gating + audit
/// events) — only the descriptions are tuned for an interactive conversation.
pub fn chat_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef::function(
            "search_messages".to_string(),
            "Keyword search across ALL the user's ingested messages (every source). Returns \
             up to 10 matches with external_id, channel, posted_at, and a short snippet."
                .to_string(),
            search_messages_params(),
        ),
        ToolDef::function(
            "fetch_messages".to_string(),
            "Fetch the full bodies of one or more messages by external_id — ids you got from \
             search_messages, list_messages, an action's evidence, or that the user named. Batch \
             them. Returns {\"messages\": [...], \"not_found\": [...]}."
                .to_string(),
            fetch_messages_params(),
        ),
        ToolDef::function(
            "list_messages".to_string(),
            "List the user's messages newest-first, optionally within a time window. Use this for \
             \"what came in recently / in the last N days\" questions — search_messages is for \
             keyword lookups, NOT time ranges. `since`/`until` are ISO 8601 dates or datetimes \
             (resolve relative dates against the current time you were given). Returns up to \
             `limit` summaries (external_id, posted_at, author, subject, snippet, channel) and \
             `has_more`; when more remain it includes `next_before` — call again with that as \
             `before` for the next, older page. Then fetch_messages by external_id for full bodies."
                .to_string(),
            list_messages_params(),
        ),
        ToolDef::function(
            "list_actions".to_string(),
            "List the user's action items — open ones by default (pending, auto-claimed, claimed), \
             most-urgent first. Returns each action's A-N id, title, status, confidence, and due \
             date. Start here when the user asks what they have outstanding, then use get_action \
             for detail on specific ones. Pass include_resolved=true to also see done/cancelled."
                .to_string(),
            list_actions_params(),
        ),
        ToolDef::function(
            "get_action".to_string(),
            "Look up one action by its A-N id: title, details, status, confidence, due date, \
             the messages it cites as evidence, and its recent history. Call this whenever the \
             user refers to an action so your answer is grounded in what it's actually based on."
                .to_string(),
            get_action_params(),
        ),
        ToolDef::function(
            "record_action".to_string(),
            "Create a new action for the user. evidence_external_ids must cite at least one real \
             message (find it with search_messages/fetch_messages). Use high confidence when the \
             user explicitly asks you to track something — it is auto-claimed; use medium/low for \
             your own suggestions, which the user confirms. Only for a concrete thing the user must \
             do — never to note that there is nothing to do."
                .to_string(),
            record_action_params(),
        ),
        ToolDef::function(
            "resolve_action".to_string(),
            "Mark an action done or cancelled by its A-N id, citing evidence_external_ids that show \
             it. Use high confidence when the user explicitly tells you it's done (applies \
             immediately); medium/low queues it for the user to confirm."
                .to_string(),
            resolve_action_params(),
        ),
        ToolDef::function(
            "update_action".to_string(),
            "Amend an existing action by its A-N id. Only the fields you pass change; new evidence \
             is appended, not replaced. Prefer this over recording a duplicate."
                .to_string(),
            update_action_params(),
        ),
        ToolDef::function(
            "search_conversation".to_string(),
            "Keyword search THIS conversation's own earlier turns — use it when the summary at \
             the top of the conversation isn't specific enough and you need to recall what was \
             actually said before. Returns up to 10 matches with a turn_id, role, timestamp, and \
             snippet. Follow up with recall_turns to read the full text. This searches the chat \
             history, not the user's ingested messages (that's search_messages)."
                .to_string(),
            search_conversation_params(),
        ),
        ToolDef::function(
            "recall_turns".to_string(),
            "Fetch the full text of specific earlier turns in THIS conversation by their turn_id \
             (from search_conversation). Use this to recover exact wording the summary condensed \
             away. Batch the ids. Long turns are truncated and the total is capped per call."
                .to_string(),
            recall_turns_params(),
        ),
    ]
}

fn search_conversation_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "keyword(s) to find in earlier turns" }
        },
        "required": ["query"]
    })
}

fn recall_turns_params() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "turn_ids": {
                "type": "array",
                "items": { "type": "integer" },
                "minItems": 1,
                "description": "turn_id values returned by search_conversation"
            }
        },
        "required": ["turn_ids"]
    })
}

/// Result of dispatching a single tool call.
pub struct DispatchOutput {
    /// JSON string returned to the model as the function_call_output.
    pub output: String,
    /// Whether this call recorded a new action (for run summary).
    pub recorded_action: bool,
}

pub async fn dispatch(
    pool: &SqlitePool,
    scope: ToolScope,
    fetch_budget: &mut FetchBudget,
    name: &str,
    arguments: &str,
) -> DispatchOutput {
    let result = match name {
        "search_messages" => search_messages(pool, scope, arguments).await,
        "fetch_messages" => fetch_messages(pool, scope, fetch_budget, arguments).await,
        "list_messages" => list_messages(pool, scope, arguments).await,
        "list_actions" => list_actions(pool, arguments).await,
        "get_action" => get_action(pool, arguments).await,
        "record_action" => match record_action(pool, scope, arguments).await {
            Ok(json) => {
                return DispatchOutput {
                    output: json,
                    recorded_action: true,
                };
            }
            Err(e) => Err(e),
        },
        "update_action" => update_action(pool, scope, arguments).await,
        "resolve_action" => resolve_action(pool, scope, arguments).await,
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    };

    let output = match result {
        Ok(json) => json,
        // `{e:#}` flattens the anyhow context chain (e.g. "FTS search failed
        // for …: no such column: from") so the model gets an actionable error
        // instead of just the opaque top-level message.
        Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
    };
    DispatchOutput {
        output,
        recorded_action: false,
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
}

#[derive(Serialize)]
struct SearchHit {
    external_id: String,
    posted_at: String,
    snippet: String,
    channel: String,
}

/// Turn an arbitrary user/model query into a safe FTS5 MATCH expression. FTS5
/// treats `:`, `-`, `"`, `*`, `(`, and bareword `AND/OR/NOT/NEAR` as operators,
/// so a natural-language query — or a `from:date` guess like the model reaches
/// for on "last 2 days" questions — raises a hard syntax error (e.g. `no such
/// column: from`). We extract bareword tokens and quote each as an FTS5 phrase,
/// ANDed together: a forgiving keyword search that never errors. Empty when the
/// query has no word characters at all.
fn sanitize_fts_query(raw: &str) -> String {
    raw.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn search_messages(pool: &SqlitePool, scope: ToolScope, arguments: &str) -> Result<String> {
    let args: SearchArgs =
        serde_json::from_str(arguments).context("parsing search_messages args")?;
    let match_query = sanitize_fts_query(&args.query);
    if match_query.is_empty() {
        // No searchable terms (e.g. a date-only or punctuation query) — return
        // empty rather than erroring; the model should use list_messages for
        // time windows.
        return Ok(serde_json::to_string(&Vec::<SearchHit>::new())?);
    }
    let filter = scope.source_filter();
    let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
        "SELECT m.external_id, m.posted_at, m.body, c.name \
         FROM messages_fts \
         JOIN messages m ON m.id = messages_fts.rowid \
         JOIN channels c ON c.id = m.channel_id \
         WHERE messages_fts MATCH ? AND (? IS NULL OR c.source_id = ?) \
         ORDER BY rank LIMIT 10",
    )
    .bind(&match_query)
    .bind(filter)
    .bind(filter)
    .fetch_all(pool)
    .await
    .with_context(|| format!("FTS search failed for {match_query:?}"))?;

    let hits: Vec<SearchHit> = rows
        .into_iter()
        .map(|(external_id, posted_at, body, channel)| SearchHit {
            external_id,
            posted_at: DateTime::<Utc>::from_timestamp(posted_at, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default(),
            snippet: snippet(&body, 200),
            channel,
        })
        .collect();
    Ok(serde_json::to_string(&hits)?)
}

#[derive(Deserialize)]
struct FetchArgs {
    external_ids: Vec<String>,
}

#[derive(Serialize)]
struct FetchedMessage {
    external_id: String,
    posted_at: String,
    author: Option<String>,
    subject: Option<String>,
    body: String,
}

#[allow(clippy::type_complexity)]
async fn fetch_messages(
    pool: &SqlitePool,
    scope: ToolScope,
    budget: &mut FetchBudget,
    arguments: &str,
) -> Result<String> {
    let args: FetchArgs = serde_json::from_str(arguments).context("parsing fetch_messages args")?;
    let filter = scope.source_filter();

    let mut messages = Vec::new();
    let mut not_found = Vec::new();
    let mut over_budget = Vec::new();

    for ext_id in &args.external_ids {
        let row: Option<(String, i64, Option<String>, Option<String>, String)> = sqlx::query_as(
            "SELECT m.external_id, m.posted_at, p.display_name, m.subject, m.body \
             FROM messages m \
             LEFT JOIN people p ON p.id = m.author_id \
             JOIN channels c ON c.id = m.channel_id \
             WHERE m.external_id = ? AND (? IS NULL OR c.source_id = ?)",
        )
        .bind(ext_id)
        .bind(filter)
        .bind(filter)
        .fetch_optional(pool)
        .await
        .context("fetch_messages lookup failed")?;

        match row {
            None => not_found.push(ext_id.clone()),
            Some((external_id, posted_at, author, subject, body)) => {
                if !budget.try_charge(body.len()) {
                    over_budget.push(ext_id.clone());
                    continue;
                }
                messages.push(FetchedMessage {
                    external_id,
                    posted_at: DateTime::<Utc>::from_timestamp(posted_at, 0)
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_default(),
                    author,
                    subject,
                    body,
                });
            }
        }
    }

    let mut result = json!({
        "messages": messages,
        "not_found": not_found,
    });
    if !over_budget.is_empty() {
        result["over_budget"] = json!(over_budget);
        result["notice"] = json!(
            "Per-run fetch budget exhausted — decide on the remaining messages from their \
             metadata, or record from what you've already read."
        );
    }
    Ok(serde_json::to_string(&result)?)
}

/// Parse an ISO 8601 date or datetime into a Unix timestamp (UTC). Accepts a
/// full RFC 3339 datetime (`2026-05-28T08:00:00Z`), a zone-less datetime
/// (assumed UTC), or a bare date (`2026-05-28`, treated as UTC midnight).
fn parse_iso_to_ts(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp());
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(ndt.and_utc().timestamp());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d
            .and_hms_opt(0, 0, 0)
            .expect("midnight is valid")
            .and_utc()
            .timestamp());
    }
    anyhow::bail!("expected an ISO 8601 date or datetime, got {s:?}")
}

const LIST_MESSAGES_DEFAULT_LIMIT: i64 = 25;
const LIST_MESSAGES_MAX_LIMIT: i64 = 100;

#[derive(Deserialize, Default)]
struct ListMessagesArgs {
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Serialize)]
struct ListedMessage {
    external_id: String,
    posted_at: String,
    author: Option<String>,
    subject: Option<String>,
    snippet: String,
    channel: String,
}

/// List messages newest-first, optionally within a `[since, until)` window, with
/// time-cursor paging (`before`). Returns up to `limit` summaries plus
/// `has_more`; when more remain, `next_before` is the `posted_at` to pass as
/// `before` on the next call. `until` and `before` are both exclusive upper
/// bounds and compose (the tighter one wins).
async fn list_messages(pool: &SqlitePool, scope: ToolScope, arguments: &str) -> Result<String> {
    let args: ListMessagesArgs = if arguments.trim().is_empty() {
        ListMessagesArgs::default()
    } else {
        serde_json::from_str(arguments).context("parsing list_messages args")?
    };
    let filter = scope.source_filter();
    let limit = args
        .limit
        .unwrap_or(LIST_MESSAGES_DEFAULT_LIMIT)
        .clamp(1, LIST_MESSAGES_MAX_LIMIT);

    let since_ts = args
        .since
        .as_deref()
        .map(parse_iso_to_ts)
        .transpose()
        .context("invalid `since`")?;
    let until_ts = args
        .until
        .as_deref()
        .map(parse_iso_to_ts)
        .transpose()
        .context("invalid `until`")?;
    let before_ts = args
        .before
        .as_deref()
        .map(parse_iso_to_ts)
        .transpose()
        .context("invalid `before`")?;

    // Fetch one extra row to detect whether a further (older) page exists.
    #[allow(clippy::type_complexity)]
    let mut rows: Vec<(String, i64, Option<String>, Option<String>, String, String)> =
        sqlx::query_as(
            "SELECT m.external_id, m.posted_at, p.display_name, m.subject, m.body, c.name \
             FROM messages m \
             LEFT JOIN people p ON p.id = m.author_id \
             JOIN channels c ON c.id = m.channel_id \
             WHERE (? IS NULL OR m.posted_at >= ?) \
               AND (? IS NULL OR m.posted_at <  ?) \
               AND (? IS NULL OR m.posted_at <  ?) \
               AND (? IS NULL OR c.source_id  =  ?) \
             ORDER BY m.posted_at DESC, m.id DESC \
             LIMIT ?",
        )
        .bind(since_ts)
        .bind(since_ts)
        .bind(until_ts)
        .bind(until_ts)
        .bind(before_ts)
        .bind(before_ts)
        .bind(filter)
        .bind(filter)
        .bind(limit + 1)
        .fetch_all(pool)
        .await
        .context("listing messages")?;

    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);

    // Cursor for the next page: the posted_at of the last (oldest) row here.
    let next_before = if has_more {
        rows.last().map(|r| iso(r.1))
    } else {
        None
    };

    let messages: Vec<ListedMessage> = rows
        .into_iter()
        .map(
            |(external_id, posted_at, author, subject, body, channel)| ListedMessage {
                external_id,
                posted_at: iso(posted_at),
                author,
                subject,
                snippet: snippet(&body, 200),
                channel,
            },
        )
        .collect();

    let returned = messages.len();
    let mut result = json!({
        "messages": messages,
        "returned": returned,
        "has_more": has_more,
    });
    if let Some(nb) = next_before {
        result["next_before"] = json!(nb);
    }
    Ok(serde_json::to_string(&result)?)
}

#[derive(Deserialize)]
struct RecordArgs {
    title: String,
    details: String,
    confidence: String,
    rationale: String,
    #[serde(default)]
    due_at: Option<String>,
    evidence_external_ids: Vec<String>,
}

async fn record_action(pool: &SqlitePool, scope: ToolScope, arguments: &str) -> Result<String> {
    let args: RecordArgs = serde_json::from_str(arguments).context("parsing record_action args")?;

    if !["high", "medium", "low"].contains(&args.confidence.as_str()) {
        anyhow::bail!("invalid confidence: {}", args.confidence);
    }
    if args.evidence_external_ids.is_empty() {
        anyhow::bail!("evidence_external_ids must contain at least one id");
    }

    let now = Utc::now().timestamp();
    let due_at_ts = args
        .due_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc).timestamp());

    let initial_status = if args.confidence == "high" {
        "auto_claimed"
    } else {
        "pending"
    };
    let actor = if args.confidence == "high" {
        "agent_auto"
    } else {
        "agent_queued"
    };

    let mut tx = pool.begin().await?;

    let (action_id,): (i64,) = sqlx::query_as(
        "INSERT INTO actions \
         (title, details, confidence, rationale, status, due_at, extracted_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&args.title)
    .bind(&args.details)
    .bind(&args.confidence)
    .bind(&args.rationale)
    .bind(initial_status)
    .bind(due_at_ts)
    .bind(now)
    .fetch_one(&mut *tx)
    .await
    .context("inserting action")?;

    // Resolve evidence message ids. Extraction is scoped to its source; chat
    // ranges over all of them (Global → no source filter).
    let filter = scope.source_filter();
    let mut evidence_ids = Vec::new();
    for (i, ext_id) in args.evidence_external_ids.iter().enumerate() {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT m.id FROM messages m \
             JOIN channels c ON c.id = m.channel_id \
             WHERE m.external_id = ? AND (? IS NULL OR c.source_id = ?) \
             LIMIT 1",
        )
        .bind(ext_id)
        .bind(filter)
        .bind(filter)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((mid,)) = row {
            sqlx::query(
                "INSERT INTO action_evidence (action_id, message_id, kind, is_primary) \
                 VALUES (?, ?, 'source', ?)",
            )
            .bind(action_id)
            .bind(mid)
            .bind(if i == 0 { 1 } else { 0 })
            .execute(&mut *tx)
            .await
            .context("inserting action_evidence")?;
            evidence_ids.push(mid);
        }
    }

    if evidence_ids.is_empty() {
        // No real evidence found — roll back.
        tx.rollback().await.ok();
        anyhow::bail!("none of the evidence_external_ids resolved to known messages");
    }

    let event_data = json!({
        "title": args.title,
        "confidence": args.confidence,
        "rationale": args.rationale,
    })
    .to_string();
    let evidence_json = serde_json::to_string(&args.evidence_external_ids)?;
    sqlx::query(
        "INSERT INTO action_events \
         (action_id, event_kind, actor, data_json, evidence_external_ids, occurred_at) \
         VALUES (?, 'created', ?, ?, ?, ?)",
    )
    .bind(action_id)
    .bind(actor)
    .bind(&event_data)
    .bind(&evidence_json)
    .bind(now)
    .execute(&mut *tx)
    .await
    .context("inserting action_events")?;

    // Enqueue embed for the action (for cross-channel dedup later).
    let action_text = format!("{}\n\n{}", args.title, args.details);
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(action_text.as_bytes());
    let hash = hex::encode(hasher.finalize());
    sqlx::query(
        "INSERT INTO embed_queue (target_kind, target_id, text_hash, enqueued_at) \
         VALUES ('action', ?, ?, ?) \
         ON CONFLICT(target_kind, target_id) DO UPDATE SET \
             text_hash = excluded.text_hash, enqueued_at = excluded.enqueued_at, \
             attempts = 0, last_error = NULL \
         WHERE embed_queue.text_hash != excluded.text_hash",
    )
    .bind(action_id)
    .bind(&hash)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(json!({
        "action_id": format!("A-{action_id}"),
        "status": initial_status,
    })
    .to_string())
}

#[derive(Deserialize)]
struct UpdateArgs {
    action_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    details: Option<String>,
    #[serde(default)]
    confidence: Option<String>,
    #[serde(default)]
    rationale: Option<String>,
    #[serde(default)]
    due_at: Option<String>,
    #[serde(default)]
    evidence_external_ids: Option<Vec<String>>,
}

/// Parse "A-N" (preferred) or bare numeric form.
fn parse_action_ref(s: &str) -> Result<i64> {
    let trimmed = s.trim();
    let numeric = trimmed.strip_prefix("A-").unwrap_or(trimmed);
    numeric
        .parse::<i64>()
        .with_context(|| format!("invalid action_id: {s:?} (expected A-N or N)"))
}

async fn update_action(pool: &SqlitePool, scope: ToolScope, arguments: &str) -> Result<String> {
    let args: UpdateArgs = serde_json::from_str(arguments).context("parsing update_action args")?;
    let action_id = parse_action_ref(&args.action_id)?;

    if let Some(c) = &args.confidence
        && !["high", "medium", "low"].contains(&c.as_str())
    {
        anyhow::bail!("invalid confidence: {c}");
    }

    let mut tx = pool.begin().await?;

    // Make sure the action exists. Scope check: it must have at least one
    // evidence message from this source, OR no evidence yet (newly created
    // in this same loop). This keeps the extractor from amending unrelated
    // actions, while still letting it tweak its own work.
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT title, status FROM actions WHERE id = ? AND status NOT IN ('done', 'dismissed')",
    )
    .bind(action_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (_old_title, old_status) = match row {
        Some(r) => r,
        None => {
            tx.rollback().await.ok();
            anyhow::bail!("action A-{action_id} not found or already resolved");
        }
    };

    // Patch the provided fields. confidence bumps may promote pending →
    // auto_claimed; we don't demote in the other direction here (would
    // surprise the user).
    let new_status = match (old_status.as_str(), args.confidence.as_deref()) {
        ("pending", Some("high")) => Some("auto_claimed"),
        _ => None,
    };
    if let Some(t) = &args.title {
        sqlx::query("UPDATE actions SET title = ? WHERE id = ?")
            .bind(t)
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(d) = &args.details {
        sqlx::query("UPDATE actions SET details = ? WHERE id = ?")
            .bind(d)
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(c) = &args.confidence {
        sqlx::query("UPDATE actions SET confidence = ? WHERE id = ?")
            .bind(c)
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(r) = &args.rationale {
        sqlx::query("UPDATE actions SET rationale = ? WHERE id = ?")
            .bind(r)
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(s) = &args.due_at {
        let ts = DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc).timestamp());
        sqlx::query("UPDATE actions SET due_at = ? WHERE id = ?")
            .bind(ts)
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(new_status) = new_status {
        sqlx::query("UPDATE actions SET status = ? WHERE id = ?")
            .bind(new_status)
            .bind(action_id)
            .execute(&mut *tx)
            .await?;
    }

    // Append (not replace) any extra evidence.
    let filter = scope.source_filter();
    let mut appended_evidence = Vec::new();
    if let Some(extras) = &args.evidence_external_ids {
        for ext_id in extras {
            let row: Option<(i64,)> = sqlx::query_as(
                "SELECT m.id FROM messages m \
                 JOIN channels c ON c.id = m.channel_id \
                 WHERE m.external_id = ? AND (? IS NULL OR c.source_id = ?) LIMIT 1",
            )
            .bind(ext_id)
            .bind(filter)
            .bind(filter)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((mid,)) = row else { continue };
            // Skip if already linked.
            let (existing,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM action_evidence WHERE action_id = ? AND message_id = ?",
            )
            .bind(action_id)
            .bind(mid)
            .fetch_one(&mut *tx)
            .await?;
            if existing > 0 {
                continue;
            }
            sqlx::query(
                "INSERT INTO action_evidence (action_id, message_id, kind, is_primary) \
                 VALUES (?, ?, 'source', 0)",
            )
            .bind(action_id)
            .bind(mid)
            .execute(&mut *tx)
            .await?;
            appended_evidence.push(ext_id.clone());
        }
    }

    let now = Utc::now().timestamp();
    let event_data = json!({
        "title": args.title,
        "details": args.details,
        "confidence": args.confidence,
        "rationale": args.rationale,
        "due_at": args.due_at,
        "status": new_status,
    })
    .to_string();
    let evidence_json = serde_json::to_string(&appended_evidence)?;
    sqlx::query(
        "INSERT INTO action_events \
         (action_id, event_kind, actor, data_json, evidence_external_ids, occurred_at) \
         VALUES (?, 'amended', 'agent_amend', ?, ?, ?)",
    )
    .bind(action_id)
    .bind(&event_data)
    .bind(&evidence_json)
    .bind(now)
    .execute(&mut *tx)
    .await
    .context("inserting action_events amend row")?;

    tx.commit().await?;

    Ok(json!({
        "action_id": format!("A-{action_id}"),
        "amended": true,
    })
    .to_string())
}

#[derive(Deserialize)]
struct ResolveArgs {
    action_id: String,
    status: String,
    confidence: String,
    rationale: String,
    evidence_external_ids: Vec<String>,
}

async fn resolve_action(pool: &SqlitePool, scope: ToolScope, arguments: &str) -> Result<String> {
    let args: ResolveArgs =
        serde_json::from_str(arguments).context("parsing resolve_action args")?;
    let action_id = parse_action_ref(&args.action_id)?;

    if !["done", "cancelled"].contains(&args.status.as_str()) {
        anyhow::bail!("invalid status: {}", args.status);
    }
    if !["high", "medium", "low"].contains(&args.confidence.as_str()) {
        anyhow::bail!("invalid confidence: {}", args.confidence);
    }
    if args.evidence_external_ids.is_empty() {
        anyhow::bail!("evidence_external_ids must contain at least one id");
    }

    let mut tx = pool.begin().await?;

    let row: Option<(String,)> = sqlx::query_as(
        "SELECT status FROM actions WHERE id = ? AND status NOT IN ('done', 'dismissed', 'cancelled')",
    )
    .bind(action_id)
    .fetch_optional(&mut *tx)
    .await?;
    if row.is_none() {
        tx.rollback().await.ok();
        anyhow::bail!("action A-{action_id} not found or already resolved");
    }

    // Resolution evidence (kind='resolution') is attached either way — even
    // for the queued case, it's useful for the user to see what the agent
    // would have used to justify the resolution.
    let filter = scope.source_filter();
    let mut attached = Vec::new();
    for ext_id in &args.evidence_external_ids {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT m.id FROM messages m \
             JOIN channels c ON c.id = m.channel_id \
             WHERE m.external_id = ? AND (? IS NULL OR c.source_id = ?) LIMIT 1",
        )
        .bind(ext_id)
        .bind(filter)
        .bind(filter)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((mid,)) = row else { continue };
        let (existing,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM action_evidence \
             WHERE action_id = ? AND message_id = ? AND kind = 'resolution'",
        )
        .bind(action_id)
        .bind(mid)
        .fetch_one(&mut *tx)
        .await?;
        if existing > 0 {
            continue;
        }
        sqlx::query(
            "INSERT INTO action_evidence (action_id, message_id, kind, is_primary) \
             VALUES (?, ?, 'resolution', 0)",
        )
        .bind(action_id)
        .bind(mid)
        .execute(&mut *tx)
        .await?;
        attached.push(ext_id.clone());
    }
    if attached.is_empty() {
        tx.rollback().await.ok();
        anyhow::bail!("none of the evidence_external_ids resolved to known messages");
    }

    let now = Utc::now().timestamp();
    let auto = args.confidence == "high";
    let (event_kind, actor) = if auto {
        ("resolved", "agent_auto")
    } else {
        ("suggested_resolution", "agent_queued")
    };

    if auto {
        sqlx::query(
            "UPDATE actions SET status = ?, resolved_at = ? \
             WHERE id = ?",
        )
        .bind(&args.status)
        .bind(now)
        .bind(action_id)
        .execute(&mut *tx)
        .await
        .context("applying high-confidence resolution")?;
    }

    let event_data = json!({
        "status": args.status,
        "confidence": args.confidence,
        "rationale": args.rationale,
    })
    .to_string();
    let evidence_json = serde_json::to_string(&attached)?;
    sqlx::query(
        "INSERT INTO action_events \
         (action_id, event_kind, actor, data_json, evidence_external_ids, occurred_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(action_id)
    .bind(event_kind)
    .bind(actor)
    .bind(&event_data)
    .bind(&evidence_json)
    .bind(now)
    .execute(&mut *tx)
    .await
    .context("inserting action_events resolve row")?;

    tx.commit().await?;

    Ok(json!({
        "action_id": format!("A-{action_id}"),
        "applied": auto,
        "status": if auto { args.status } else { "queued".to_string() },
    })
    .to_string())
}

#[derive(Deserialize, Default)]
struct ListActionsArgs {
    #[serde(default)]
    include_resolved: bool,
}

/// List the user's actions (open ones by default), most-urgent first — the
/// entry point for "what do I have outstanding?". Chat-only.
async fn list_actions(pool: &SqlitePool, arguments: &str) -> Result<String> {
    let args: ListActionsArgs = if arguments.trim().is_empty() {
        ListActionsArgs::default()
    } else {
        serde_json::from_str(arguments).context("parsing list_actions args")?
    };

    let rows: Vec<(i64, String, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT id, title, status, confidence, due_at FROM actions \
         WHERE (? OR status IN ('pending', 'auto_claimed', 'claimed')) \
         ORDER BY (due_at IS NULL), due_at ASC, extracted_at DESC LIMIT 50",
    )
    .bind(args.include_resolved)
    .fetch_all(pool)
    .await
    .context("list_actions query failed")?;

    let actions: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(id, title, status, confidence, due_at)| {
            json!({
                "action_id": format!("A-{id}"),
                "title": title,
                "status": status,
                "confidence": confidence,
                "due_at": due_at.map(iso),
            })
        })
        .collect();
    Ok(json!({ "actions": actions }).to_string())
}

#[derive(Deserialize)]
struct GetActionArgs {
    action_id: String,
}

/// Convert a unix timestamp to an RFC 3339 string (empty on overflow).
fn iso(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

/// Look up one action with its evidence messages and recent event history, so
/// the chat agent can ground answers about a specific action the user names.
/// Chat-only (the extractor gets existing actions injected into its prompt), so
/// there's no source scope — actions aren't source-bound.
async fn get_action(pool: &SqlitePool, arguments: &str) -> Result<String> {
    let args: GetActionArgs = serde_json::from_str(arguments).context("parsing get_action args")?;
    let action_id = parse_action_ref(&args.action_id)?;

    #[allow(clippy::type_complexity)]
    let row: Option<(
        String,
        Option<String>,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
    )> = sqlx::query_as(
        "SELECT title, details, confidence, status, rationale, due_at, resolved_at \
         FROM actions WHERE id = ?",
    )
    .bind(action_id)
    .fetch_optional(pool)
    .await
    .context("get_action lookup failed")?;

    let Some((title, details, confidence, status, rationale, due_at, resolved_at)) = row else {
        anyhow::bail!("action A-{action_id} not found");
    };

    let ev_rows: Vec<(String, String, i64, String)> = sqlx::query_as(
        "SELECT m.external_id, ae.kind, m.posted_at, m.body \
         FROM action_evidence ae \
         JOIN messages m ON m.id = ae.message_id \
         WHERE ae.action_id = ? \
         ORDER BY ae.is_primary DESC, m.posted_at",
    )
    .bind(action_id)
    .fetch_all(pool)
    .await
    .context("get_action evidence lookup failed")?;
    let evidence: Vec<serde_json::Value> = ev_rows
        .into_iter()
        .map(|(external_id, kind, posted_at, body)| {
            json!({
                "external_id": external_id,
                "kind": kind,
                "posted_at": iso(posted_at),
                "snippet": snippet(&body, 200),
            })
        })
        .collect();

    let event_rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT event_kind, actor, occurred_at FROM action_events \
         WHERE action_id = ? ORDER BY occurred_at DESC LIMIT 10",
    )
    .bind(action_id)
    .fetch_all(pool)
    .await
    .context("get_action events lookup failed")?;
    let events: Vec<serde_json::Value> = event_rows
        .into_iter()
        .map(|(kind, actor, at)| json!({ "kind": kind, "actor": actor, "at": iso(at) }))
        .collect();

    Ok(json!({
        "action_id": format!("A-{action_id}"),
        "title": title,
        "details": details,
        "confidence": confidence,
        "status": status,
        "rationale": rationale,
        "due_at": due_at.map(iso),
        "resolved_at": resolved_at.map(iso),
        "evidence": evidence,
        "events": events,
    })
    .to_string())
}

/// Truncate `text` to `max` chars (collapsing newlines) with an ellipsis when
/// cut. Public so the CLI `dump_prompt` command and the window projection share
/// one snippet definition rather than drifting.
pub fn snippet(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.replace('\n', " ");
    }
    let truncated: String = text.chars().take(max).collect();
    format!("{}…", truncated.replace('\n', " "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::test_util::{SeedMessage, seed_messages, seed_minimal};
    use tempfile::TempDir;

    async fn db_with_messages(bodies: &[(&str, &str)]) -> (TempDir, SqlitePool, ToolScope) {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let ctx = seed_minimal(&pool).await.unwrap();
        let msgs: Vec<SeedMessage> = bodies
            .iter()
            .map(|(id, body)| SeedMessage {
                external_id: id,
                author_email: "ana@example.com",
                author_name: "Ana",
                subject: "S",
                body,
                recipients: &[],
            })
            .collect();
        seed_messages(&pool, ctx.source_id, ctx.channel_id, &msgs)
            .await
            .unwrap();
        (tmp, pool, ToolScope::Source(ctx.source_id))
    }

    #[tokio::test]
    async fn fetch_messages_returns_bodies_and_flags_missing() {
        let (_tmp, pool, scope) = db_with_messages(&[("msg-a", "the full body of A")]).await;
        let mut budget = FetchBudget::unlimited();
        let args = json!({ "external_ids": ["msg-a", "msg-missing"] }).to_string();
        let out = fetch_messages(&pool, scope, &mut budget, &args)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["messages"][0]["external_id"], "msg-a");
        assert!(
            v["messages"][0]["body"]
                .as_str()
                .unwrap()
                .contains("full body of A")
        );
        assert_eq!(v["not_found"][0], "msg-missing");
    }

    #[tokio::test]
    async fn fetch_messages_respects_the_fetch_budget() {
        let big = "x".repeat(100);
        let (_tmp, pool, scope) =
            db_with_messages(&[("msg-a", big.as_str()), ("msg-b", big.as_str())]).await;
        // Cap fits one 100-char body but not two.
        let mut budget = FetchBudget::new(150);
        let args = json!({ "external_ids": ["msg-a", "msg-b"] }).to_string();
        let out = fetch_messages(&pool, scope, &mut budget, &args)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["messages"].as_array().unwrap().len(),
            1,
            "only the first body should fit the budget: {out}"
        );
        assert_eq!(v["messages"][0]["external_id"], "msg-a");
        assert_eq!(v["over_budget"][0], "msg-b");
        assert!(
            v["notice"].is_string(),
            "a budget-exhausted notice should be present: {out}"
        );
    }

    #[tokio::test]
    async fn get_action_returns_the_action_with_evidence_and_events() {
        let (_tmp, pool, scope) =
            db_with_messages(&[("msg-a", "Ana asked you to ship the report")]).await;
        // Record an action citing msg-a, then look it up.
        let rec = json!({
            "title": "Ship the report",
            "details": "Ana asked.",
            "confidence": "high",
            "rationale": "direct ask",
            "evidence_external_ids": ["msg-a"],
        })
        .to_string();
        let recorded = record_action(&pool, scope, &rec).await.unwrap();
        let action_ref = serde_json::from_str::<serde_json::Value>(&recorded).unwrap()["action_id"]
            .as_str()
            .unwrap()
            .to_string();

        let out = get_action(&pool, &json!({ "action_id": action_ref }).to_string())
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["action_id"], action_ref);
        assert_eq!(v["title"], "Ship the report");
        assert_eq!(v["status"], "auto_claimed");
        assert_eq!(v["evidence"][0]["external_id"], "msg-a");
        assert!(
            v["evidence"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("ship the report"),
            "evidence snippet should carry the body: {out}"
        );
        let kinds: Vec<&str> = v["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"created"), "events: {kinds:?}");
    }

    #[tokio::test]
    async fn get_action_reports_missing() {
        let (_tmp, pool, _scope) = db_with_messages(&[("msg-a", "x")]).await;
        assert!(
            get_action(&pool, &json!({ "action_id": "A-999" }).to_string())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn list_actions_returns_open_actions_with_refs() {
        let (_tmp, pool, scope) =
            db_with_messages(&[("msg-a", "Ana asked you to ship the report")]).await;
        let rec = json!({
            "title": "Ship the report",
            "details": "Ana asked.",
            "confidence": "high",
            "rationale": "direct ask",
            "evidence_external_ids": ["msg-a"],
        })
        .to_string();
        record_action(&pool, scope, &rec).await.unwrap();

        // Empty args must not panic (the model often sends "" for no-arg tools).
        for args in ["", "{}"] {
            let out = list_actions(&pool, args).await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["actions"][0]["action_id"], "A-1", "args={args:?}: {out}");
            assert_eq!(v["actions"][0]["title"], "Ship the report");
            assert_eq!(v["actions"][0]["status"], "auto_claimed");
        }
    }

    #[tokio::test]
    async fn search_messages_global_scope_spans_sources() {
        // Source 1 (from seed_minimal) has "alpha"; add a second source with "beta".
        let (_tmp, pool, source1_scope) =
            db_with_messages(&[("m1", "alpha from source one")]).await;
        let (s2,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'two', 'two/ref', 0) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let (c2,): (i64,) = sqlx::query_as(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, 'INBOX2', 'INBOX2', 'mailbox') RETURNING id",
        )
        .bind(s2)
        .fetch_one(&pool)
        .await
        .unwrap();
        seed_messages(
            &pool,
            s2,
            c2,
            &[SeedMessage {
                external_id: "m2",
                author_email: "bob@example.com",
                author_name: "Bob",
                subject: "S",
                body: "beta from source two",
                recipients: &[],
            }],
        )
        .await
        .unwrap();

        let q = json!({ "query": "beta" }).to_string();

        // Source-scoped to source 1: must not see source 2's message.
        let out1 = search_messages(&pool, source1_scope, &q).await.unwrap();
        let hits1: serde_json::Value = serde_json::from_str(&out1).unwrap();
        assert_eq!(
            hits1.as_array().unwrap().len(),
            0,
            "source-scoped search must not leak across sources: {out1}"
        );

        // Global: finds the source-2 message.
        let out2 = search_messages(&pool, ToolScope::Global, &q).await.unwrap();
        let hits2: serde_json::Value = serde_json::from_str(&out2).unwrap();
        assert_eq!(
            hits2[0]["external_id"], "m2",
            "global search should span sources: {out2}"
        );
    }

    #[tokio::test]
    async fn search_messages_tolerates_operator_syntax() {
        let (_tmp, pool, scope) = db_with_messages(&[("m1", "an email about the meeting")]).await;

        // The exact query the model sent on "last 2 days" that used to raise a
        // hard `no such column: from` FTS5 error — must now degrade gracefully.
        let messy =
            json!({ "query": "email from:2026-05-28..2026-05-30 OR subject OR FROM" }).to_string();
        let out = search_messages(&pool, scope, &messy).await;
        assert!(out.is_ok(), "operator-laden query must not error: {out:?}");

        // A plain keyword still finds the message.
        let plain = json!({ "query": "email" }).to_string();
        let hits: serde_json::Value =
            serde_json::from_str(&search_messages(&pool, scope, &plain).await.unwrap()).unwrap();
        assert_eq!(
            hits[0]["external_id"], "m1",
            "keyword search still works: {hits}"
        );

        // A query with no word characters degrades to empty, not an error.
        let punct = json!({ "query": ":-)" }).to_string();
        let empty: serde_json::Value =
            serde_json::from_str(&search_messages(&pool, scope, &punct).await.unwrap()).unwrap();
        assert_eq!(
            empty.as_array().unwrap().len(),
            0,
            "punctuation-only query → empty: {empty}"
        );
    }

    #[tokio::test]
    async fn list_messages_pages_newest_first_with_a_cursor() {
        let (_tmp, pool, scope) = db_with_messages(&[
            ("m0", "b0"),
            ("m1", "b1"),
            ("m2", "b2"),
            ("m3", "b3"),
            ("m4", "b4"),
        ])
        .await;

        // Page through two at a time via the `before` cursor; the union must be
        // every message exactly once, newest-first, with no gaps or overlaps.
        let mut seen: Vec<String> = Vec::new();
        let mut before: Option<String> = None;
        for page in 0..10 {
            let mut args = serde_json::Map::new();
            args.insert("limit".to_string(), json!(2));
            if let Some(b) = &before {
                args.insert("before".to_string(), json!(b));
            }
            let out = list_messages(&pool, scope, &serde_json::Value::Object(args).to_string())
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            for m in v["messages"].as_array().unwrap() {
                seen.push(m["external_id"].as_str().unwrap().to_string());
            }
            if v["has_more"].as_bool().unwrap() {
                before = Some(v["next_before"].as_str().unwrap().to_string());
            } else {
                assert!(
                    v.get("next_before").is_none(),
                    "the last page must not carry a cursor: {out}"
                );
                break;
            }
            assert!(page < 9, "paging failed to terminate");
        }
        assert_eq!(
            seen,
            vec!["m4", "m3", "m2", "m1", "m0"],
            "cursor paging should walk newest→oldest with no gaps or dups"
        );
    }

    #[tokio::test]
    async fn list_messages_filters_by_date_range_and_spans_sources() {
        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let ctx = seed_minimal(&pool).await.unwrap();

        // A second source/channel, to prove Global spans sources.
        let (s2,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'two', 'two/ref', 0) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let (c2,): (i64,) = sqlx::query_as(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, 'INBOX2', 'INBOX2', 'mailbox') RETURNING id",
        )
        .bind(s2)
        .fetch_one(&pool)
        .await
        .unwrap();

        const T0: i64 = 1_700_000_000;
        const DAY: i64 = 86_400;
        for (ch, ext, posted, body) in [
            (ctx.channel_id, "old", T0, "before the window"),
            (ctx.channel_id, "mid1", T0 + DAY, "in window, source one"),
            (c2, "mid2", T0 + DAY + 100, "in window, source two"),
            (ctx.channel_id, "new", T0 + 2 * DAY, "at/after the window"),
        ] {
            sqlx::query(
                "INSERT INTO messages \
                 (channel_id, external_id, posted_at, subject, body, body_format, ingested_at) \
                 VALUES (?, ?, ?, 'S', ?, 'text', 0)",
            )
            .bind(ch)
            .bind(ext)
            .bind(posted)
            .bind(body)
            .execute(&pool)
            .await
            .unwrap();
        }

        let args = json!({ "since": iso(T0 + DAY), "until": iso(T0 + 2 * DAY) }).to_string();

        // Global: both in-window messages, newest-first, across both sources;
        // "old" (before since) and "new" (>= until, exclusive) are excluded.
        let out = list_messages(&pool, ToolScope::Global, &args)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let ids: Vec<&str> = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["external_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["mid2", "mid1"],
            "date window + newest-first across sources: {out}"
        );
        assert_eq!(
            v["has_more"], false,
            "only two messages in the window: {out}"
        );

        // Source-scoped to source 1: only its in-window message.
        let out1 = list_messages(&pool, ToolScope::Source(ctx.source_id), &args)
            .await
            .unwrap();
        let v1: serde_json::Value = serde_json::from_str(&out1).unwrap();
        let ids1: Vec<&str> = v1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["external_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids1,
            vec!["mid1"],
            "scope must restrict to its source: {out1}"
        );
    }
}
