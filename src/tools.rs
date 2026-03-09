use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::imap_client::ImapClient;
use crate::llm::ToolDef;
use crate::memory::MemoryStore;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolName {
    ListMailboxes,
    ListMessages,
    ReadMessage,
    MarkAsRead,
    WriteMemory,
    ReadMemory,
    SearchMemory,
    WriteReport,
}

fn tool(name: &str, description: &str, parameters: serde_json::Value) -> ToolDef {
    ToolDef::function(name.to_string(), description.to_string(), parameters)
}

/// Build the list of tool definitions for the LLM.
pub fn tool_definitions() -> Vec<ToolDef> {
    vec![
        tool(
            "list_mailboxes",
            "List all IMAP mailboxes.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        ),
        tool(
            "list_messages",
            "List messages in a mailbox (subject, from, date, uid, flags).",
            json!({
                "type": "object",
                "properties": {
                    "mailbox": { "type": "string", "description": "Mailbox name (e.g. INBOX)" },
                    "limit": { "type": "integer", "description": "Max number of messages to return (most recent first). Omit for all." }
                },
                "required": ["mailbox"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "read_message",
            "Read a full message by UID (headers + text body).",
            json!({
                "type": "object",
                "properties": {
                    "mailbox": { "type": "string", "description": "Mailbox name" },
                    "uid": { "type": "integer", "description": "Message UID" }
                },
                "required": ["mailbox", "uid"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "mark_as_read",
            "Set the \\Seen flag on a message. Only use when explicitly instructed.",
            json!({
                "type": "object",
                "properties": {
                    "mailbox": { "type": "string", "description": "Mailbox name" },
                    "uid": { "type": "integer", "description": "Message UID" }
                },
                "required": ["mailbox", "uid"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "write_memory",
            "Write or overwrite a persistent memory note by key.",
            json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Note key (alphanumeric, hyphens, underscores)" },
                    "content": { "type": "string", "description": "Note content (markdown)" }
                },
                "required": ["key", "content"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "read_memory",
            "Read a memory note by key. If key is omitted, list all available keys.",
            json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Note key. Omit to list all keys." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "search_memory",
            "Search across all memory notes for a pattern (case-insensitive substring match). Returns matching lines with keys.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern" }
                },
                "required": ["pattern"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "write_report",
            "Output the attention report or final response to the user.",
            json!({
                "type": "object",
                "properties": {
                    "report": { "type": "string", "description": "The report text" }
                },
                "required": ["report"],
                "additionalProperties": false,
            }),
        ),
    ]
}

// Argument structs for each tool

#[derive(Deserialize)]
struct ListMessagesArgs {
    mailbox: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct MailboxUidArgs {
    mailbox: String,
    uid: u32,
}

#[derive(Deserialize)]
struct WriteMemoryArgs {
    key: String,
    content: String,
}

#[derive(Deserialize)]
struct ReadMemoryArgs {
    key: Option<String>,
}

#[derive(Deserialize)]
struct SearchMemoryArgs {
    pattern: String,
}

#[derive(Deserialize)]
struct WriteReportArgs {
    report: String,
}

/// Result of dispatching a tool call.
pub struct ToolResult {
    pub output: String,
    /// If this was a write_report call, contains the report text.
    pub report: Option<String>,
}

/// Dispatch a tool call by name, executing it against the IMAP client and memory store.
pub async fn dispatch(
    name: &str,
    arguments: &str,
    imap: &mut ImapClient,
    memory: &MemoryStore,
) -> ToolResult {
    let tool_name: ToolName =
        match serde_json::from_value(serde_json::Value::String(name.to_string())) {
            Ok(n) => n,
            Err(_) => {
                return ToolResult {
                    output: serde_json::to_string(
                        &json!({"error": format!("unknown tool: {name}")}),
                    )
                    .unwrap_or_default(),
                    report: None,
                };
            }
        };

    match dispatch_inner(tool_name, arguments, imap, memory).await {
        Ok(result) => result,
        Err(err) => ToolResult {
            output: format!("Error: {err:#}"),
            report: None,
        },
    }
}

async fn dispatch_inner(
    name: ToolName,
    arguments: &str,
    imap: &mut ImapClient,
    memory: &MemoryStore,
) -> Result<ToolResult> {
    let ok = |output: String| ToolResult {
        output,
        report: None,
    };

    match name {
        ToolName::ListMailboxes => {
            let mailboxes = imap.list_mailboxes().await?;
            Ok(ok(serde_json::to_string(&mailboxes)?))
        }
        ToolName::ListMessages => {
            let args: ListMessagesArgs = serde_json::from_str(arguments)?;
            let messages = imap.list_messages(&args.mailbox, args.limit).await?;
            Ok(ok(serde_json::to_string(&messages)?))
        }
        ToolName::ReadMessage => {
            let args: MailboxUidArgs = serde_json::from_str(arguments)?;
            let message = imap.read_message(&args.mailbox, args.uid).await?;
            Ok(ok(serde_json::to_string(&message)?))
        }
        ToolName::MarkAsRead => {
            let args: MailboxUidArgs = serde_json::from_str(arguments)?;
            imap.mark_as_read(&args.mailbox, args.uid).await?;
            Ok(ok(serde_json::to_string(&json!({"status": "ok"}))?))
        }
        ToolName::WriteMemory => {
            let args: WriteMemoryArgs = serde_json::from_str(arguments)?;
            memory.write(&args.key, &args.content).await?;
            Ok(ok(serde_json::to_string(
                &json!({"status": "ok", "key": args.key}),
            )?))
        }
        ToolName::ReadMemory => {
            let args: ReadMemoryArgs = serde_json::from_str(arguments)?;
            match args.key {
                Some(key) => match memory.read(&key).await? {
                    Some(content) => Ok(ok(serde_json::to_string(
                        &json!({"key": key, "content": content}),
                    )?)),
                    None => Ok(ok(serde_json::to_string(
                        &json!({"error": format!("no note with key: {key}")}),
                    )?)),
                },
                None => {
                    let keys = memory.list_keys().await?;
                    Ok(ok(serde_json::to_string(&json!({"keys": keys}))?))
                }
            }
        }
        ToolName::SearchMemory => {
            let args: SearchMemoryArgs = serde_json::from_str(arguments)?;
            let hits = memory.search(&args.pattern).await?;
            let results: Vec<_> = hits
                .iter()
                .map(|h| {
                    json!({
                        "key": h.key,
                        "line_number": h.line_number,
                        "line": h.line,
                    })
                })
                .collect();
            Ok(ok(serde_json::to_string(&results)?))
        }
        ToolName::WriteReport => {
            let args: WriteReportArgs = serde_json::from_str(arguments)?;
            Ok(ToolResult {
                output: serde_json::to_string(&json!({"status": "ok"}))?,
                report: Some(args.report),
            })
        }
    }
}
