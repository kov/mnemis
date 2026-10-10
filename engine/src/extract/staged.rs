//! Extraction in small, single-purpose turns, for models with a small context
//! that lose the thread in the agent loop: per message, summarise it (no
//! tools), decide whether it needs the user to act (a 0 or 1), and only then
//! record the action from the summary with a narrow tool set.

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::prompt::{ExistingAction, render_recipients};
use super::{DryCall, ExtractionScope, UserProfile, WindowRow, dispatch_or_capture, tools};
use crate::agent;
use crate::llm::{ContentItem, InputItem, LlmTransport, OutputItem, ResponsesResponse, Role};

const SUMMARY_SYSTEM: &str = "You summarize one email for an assistant that keeps track of \
    what its user has to do. In at most three sentences, say who sent it, what it asks or tells \
    the reader, and any date or deadline it gives. Write in English, whatever the email's \
    language. Report only what the email says.";

/// Tool turns allowed per message once it is judged actionable.
const ACTION_TURNS: usize = 3;

/// Inputs shared by every message of a batch.
pub(super) struct Context<'a> {
    pub profile: &'a UserProfile,
    pub memory_rules: Option<&'a str>,
    pub existing: &'a [ExistingAction],
    pub now: DateTime<Utc>,
    /// Longest body, in chars, handed to the summary turn.
    pub body_chars: usize,
    pub idle_timeout: Duration,
}

pub(super) async fn run(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    scope: ExtractionScope,
    ctx: &Context<'_>,
    message: &WindowRow,
    mut dry: Option<&mut Vec<DryCall>>,
) -> Result<String> {
    let headers = headers(message, &ctx.profile.identifiers);
    let stream = agent::StreamCtx::headless(ctx.idle_timeout);

    let body = clean(&message.body, ctx.body_chars);
    let summary = text_of(
        &send(
            llm,
            SUMMARY_SYSTEM,
            format!("{headers}\n{body}"),
            &[],
            &stream,
        )
        .await?,
    );
    let digest = format!("{headers}Summary: {summary}\n");

    let verdict = text_of(&send(llm, &relevance_system(ctx), digest.clone(), &[], &stream).await?);
    if !verdict.trim_start().starts_with('1') {
        return Ok(format!(
            "{}: not actionable — {summary}",
            message.external_id
        ));
    }

    let defs: Vec<_> = tools::definitions()
        .into_iter()
        .filter(|d| d.name == "record_action" || d.name == "update_action")
        .collect();
    let system = action_system(ctx, &message.external_id);
    let mut history = vec![InputItem::Message {
        role: Role::User,
        content: digest,
    }];
    let mut actions = 0;
    let mut budget = tools::FetchBudget::new(0);
    for step in 0..ACTION_TURNS {
        let response = agent::send_with_stall_retry(
            llm,
            &system,
            &history,
            &defs,
            step,
            Some(&stream),
            |_| {},
        )
        .await?;
        let mut calls = Vec::new();
        for item in response.output {
            match item {
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    history.push(InputItem::FunctionCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    calls.push((call_id, name, arguments));
                }
                OutputItem::Thinking { block } => history.push(InputItem::Thinking { block }),
                _ => {}
            }
        }
        if calls.is_empty() {
            break;
        }
        for (call_id, name, arguments) in calls {
            let out = dispatch_or_capture(
                pool,
                scope,
                &mut budget,
                dry.as_deref_mut(),
                &name,
                &arguments,
            )
            .await;
            if out.recorded_action {
                actions += 1;
            }
            history.push(InputItem::FunctionCallOutput {
                call_id,
                output: out.output,
            });
        }
        if actions > 0 {
            break;
        }
    }
    Ok(format!(
        "{}: {actions} recorded — {summary}",
        message.external_id
    ))
}

async fn send(
    llm: &dyn LlmTransport,
    system: &str,
    content: String,
    tools: &[crate::llm::ToolDef],
    stream: &agent::StreamCtx,
) -> Result<ResponsesResponse> {
    let history = [InputItem::Message {
        role: Role::User,
        content,
    }];
    agent::send_with_stall_retry(llm, system, &history, tools, 0, Some(stream), |_| {}).await
}

fn text_of(response: &ResponsesResponse) -> String {
    response
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Message { content } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        ContentItem::OutputText { text } => Some(text.as_str()),
                        ContentItem::Unknown => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string()
}

fn headers(message: &WindowRow, identifiers: &[String]) -> String {
    let mut out = format!(
        "From: {}\n",
        message.author_display.as_deref().unwrap_or("?")
    );
    render_recipients(&mut out, &message.recipients, identifiers);
    if let Some(date) = DateTime::<Utc>::from_timestamp(message.posted_at, 0) {
        out.push_str(&format!("Date: {}\n", date.to_rfc3339()));
    }
    if let Some(subject) = message.subject.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("Subject: {subject}\n"));
    }
    out
}

/// Collapse runs of whitespace (mail bodies carry plenty) and cap the length.
fn clean(body: &str, max_chars: usize) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &collapsed[..cut]),
        None => collapsed,
    }
}

fn who(ctx: &Context<'_>) -> String {
    let name = ctx.profile.display_name.as_deref().unwrap_or("the user");
    if ctx.profile.identifiers.is_empty() {
        name.to_string()
    } else {
        format!("{name} ({})", ctx.profile.identifiers.join(", "))
    }
}

fn relevance_system(ctx: &Context<'_>) -> String {
    let mut out = format!(
        "You decide whether an email needs {} to do something. It is now {}.\n\n\
         Answer 1 when the email asks the user to do something, records something the user \
         promised to do, or sets a deadline that affects the user. Answer 0 for newsletters, \
         promotions, information without an ask, asks aimed at someone else, and things \
         already done.\n",
        who(ctx),
        ctx.now.to_rfc3339()
    );
    if let Some(prompt) = ctx
        .profile
        .custom_prompt
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    {
        out.push_str(&format!("\nAbout the user: {}\n", prompt.trim()));
    }
    if let Some(rules) = ctx.memory_rules {
        out.push_str(&format!(
            "\nRules the user taught you; they override the above:\n{}\n",
            rules.trim()
        ));
    }
    out.push_str("\nReply with the single digit 1 or 0 and nothing else.");
    out
}

fn action_system(ctx: &Context<'_>, external_id: &str) -> String {
    let mut out = format!(
        "You record to-dos for {}. It is now {}. The email below needs the user to do \
         something. Call record_action once for it, with evidence_external_ids \
         [\"{external_id}\"], a short imperative title, details, a confidence (high for an \
         explicit ask or commitment, medium when implied or vague) and due_at as an ISO 8601 \
         timestamp when the email gives a date (null otherwise).\n",
        who(ctx),
        ctx.now.to_rfc3339()
    );
    if !ctx.existing.is_empty() {
        out.push_str(
            "\nIf it is the same to-do as one of these, call update_action with its id instead:\n",
        );
        for a in ctx.existing {
            out.push_str(&format!("[A-{}] {}\n", a.id, a.title));
        }
    }
    out
}
