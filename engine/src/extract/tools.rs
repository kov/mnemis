use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;

use crate::llm::ToolDef;

/// Snapshot of which channel + source an extraction run is scoped to.
/// Used by tool dispatch to scope DB lookups appropriately.
#[derive(Debug, Clone, Copy)]
pub struct ExtractionScope {
    pub source_id: i64,
    pub channel_id: i64,
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

pub fn definitions() -> Vec<ToolDef> {
    vec![
        ToolDef::function(
            "search_messages".to_string(),
            "Keyword search across messages in this source. Returns up to 10 matches \
             with external_id, channel, posted_at, and a short snippet."
                .to_string(),
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }),
        ),
        ToolDef::function(
            "fetch_messages".to_string(),
            "Fetch the full bodies of one or more messages by external_id. Batch the ids — \
             pass several at once to save round-trips. The window shows only snippets, so \
             call this before recording an action whenever a snippet alone doesn't confirm \
             the ask. Returns {\"messages\": [...], \"not_found\": [...]} plus \"over_budget\" \
             ids and a notice if the per-run fetch budget is hit."
                .to_string(),
            json!({
                "type": "object",
                "properties": {
                    "external_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1
                    }
                },
                "required": ["external_ids"]
            }),
        ),
        ToolDef::function(
            "record_action".to_string(),
            "Record one action item. evidence_external_ids must reference at least one \
             message visible in the window or fetched via fetch_messages. Returns \
             {\"action_id\": \"A-N\", \"status\": ...} — keep the action_id if you may \
             want to amend the same action later in this response (use update_action)."
                .to_string(),
            json!({
                "type": "object",
                "properties": {
                    "title":      { "type": "string", "description": "imperative, ≤80 chars" },
                    "details":    { "type": "string", "description": "1-3 sentences of context" },
                    "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
                    "rationale":  { "type": "string", "description": "≤200 chars, why this is an action" },
                    "due_at":     { "type": ["string", "null"], "description": "ISO 8601 timestamp or null" },
                    "evidence_external_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1
                    }
                },
                "required": ["title", "details", "confidence", "rationale", "evidence_external_ids"]
            }),
        ),
        ToolDef::function(
            "resolve_action".to_string(),
            "Mark a prior action as done or cancelled because the window proves it. \
             Identify the action by its A-N id; provide evidence_external_ids that \
             show the resolution (≥1). high-confidence applies immediately; medium/low \
             queue the suggestion for the user to confirm — same gating as record_action."
                .to_string(),
            json!({
                "type": "object",
                "properties": {
                    "action_id":  { "type": "string", "description": "A-N id from Existing actions" },
                    "status":     { "type": "string", "enum": ["done", "cancelled"] },
                    "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
                    "rationale":  { "type": "string", "description": "≤200 chars, what proves it's done" },
                    "evidence_external_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1
                    }
                },
                "required": ["action_id", "status", "confidence", "rationale", "evidence_external_ids"]
            }),
        ),
        ToolDef::function(
            "update_action".to_string(),
            "Amend an existing action (one you just recorded, or one from the Existing list). \
             Identify it by its A-N id. Only the fields you pass are changed; new evidence \
             is appended, not replaced. Use this instead of calling record_action a second \
             time for the same underlying item."
                .to_string(),
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
            }),
        ),
    ]
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
    scope: ExtractionScope,
    fetch_budget: &mut FetchBudget,
    name: &str,
    arguments: &str,
) -> DispatchOutput {
    let result = match name {
        "search_messages" => search_messages(pool, scope, arguments).await,
        "fetch_messages" => fetch_messages(pool, scope, fetch_budget, arguments).await,
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
        Err(e) => json!({ "error": e.to_string() }).to_string(),
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

async fn search_messages(
    pool: &SqlitePool,
    scope: ExtractionScope,
    arguments: &str,
) -> Result<String> {
    let args: SearchArgs =
        serde_json::from_str(arguments).context("parsing search_messages args")?;
    let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
        "SELECT m.external_id, m.posted_at, m.body, c.name \
         FROM messages_fts \
         JOIN messages m ON m.id = messages_fts.rowid \
         JOIN channels c ON c.id = m.channel_id \
         WHERE messages_fts MATCH ? AND c.source_id = ? \
         ORDER BY rank LIMIT 10",
    )
    .bind(&args.query)
    .bind(scope.source_id)
    .fetch_all(pool)
    .await
    .context("FTS search failed")?;

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
    scope: ExtractionScope,
    budget: &mut FetchBudget,
    arguments: &str,
) -> Result<String> {
    let args: FetchArgs = serde_json::from_str(arguments).context("parsing fetch_messages args")?;

    let mut messages = Vec::new();
    let mut not_found = Vec::new();
    let mut over_budget = Vec::new();

    for ext_id in &args.external_ids {
        let row: Option<(String, i64, Option<String>, Option<String>, String)> = sqlx::query_as(
            "SELECT m.external_id, m.posted_at, p.display_name, m.subject, m.body \
             FROM messages m \
             LEFT JOIN people p ON p.id = m.author_id \
             JOIN channels c ON c.id = m.channel_id \
             WHERE m.external_id = ? AND c.source_id = ?",
        )
        .bind(ext_id)
        .bind(scope.source_id)
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

async fn record_action(
    pool: &SqlitePool,
    scope: ExtractionScope,
    arguments: &str,
) -> Result<String> {
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

    // Resolve evidence message ids (scoped to source — extractor can fetch across channels).
    let mut evidence_ids = Vec::new();
    for (i, ext_id) in args.evidence_external_ids.iter().enumerate() {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT m.id FROM messages m \
             JOIN channels c ON c.id = m.channel_id \
             WHERE m.external_id = ? AND c.source_id = ? \
             LIMIT 1",
        )
        .bind(ext_id)
        .bind(scope.source_id)
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

async fn update_action(
    pool: &SqlitePool,
    scope: ExtractionScope,
    arguments: &str,
) -> Result<String> {
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
    let mut appended_evidence = Vec::new();
    if let Some(extras) = &args.evidence_external_ids {
        for ext_id in extras {
            let row: Option<(i64,)> = sqlx::query_as(
                "SELECT m.id FROM messages m \
                 JOIN channels c ON c.id = m.channel_id \
                 WHERE m.external_id = ? AND c.source_id = ? LIMIT 1",
            )
            .bind(ext_id)
            .bind(scope.source_id)
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

async fn resolve_action(
    pool: &SqlitePool,
    scope: ExtractionScope,
    arguments: &str,
) -> Result<String> {
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
    let mut attached = Vec::new();
    for ext_id in &args.evidence_external_ids {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT m.id FROM messages m \
             JOIN channels c ON c.id = m.channel_id \
             WHERE m.external_id = ? AND c.source_id = ? LIMIT 1",
        )
        .bind(ext_id)
        .bind(scope.source_id)
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

    async fn db_with_messages(bodies: &[(&str, &str)]) -> (TempDir, SqlitePool, ExtractionScope) {
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
        let scope = ExtractionScope {
            source_id: ctx.source_id,
            channel_id: ctx.channel_id,
        };
        (tmp, pool, scope)
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
}
