//! Extraction in small, single-purpose turns, for models with a small context
//! that lose the thread in the agent loop: per message, classify it (marketing,
//! phishing and the like), summarise it (no tools), decide from both whether
//! it needs the user to act (a 0 or 1), and only then record the action from
//! the summary with a narrow tool set.

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

const KIND_SYSTEM: &str = "You sort email for an assistant that keeps track of what its \
    user has to do. Judge the email by its sender address, links and wording, and reply with \
    one word:\n\
    personal: written by a person to the user\n\
    account: about the user's own account, order, bill, investment or service, such as a \
    failed payment, a security alert, a statement or a shareholder vote\n\
    notification: other automated mail, such as receipts, shipping updates, one-time codes, \
    newsletters and social network notifications\n\
    marketing: promotions, offers, upsells and invitations to buy, sign up or enroll\n\
    phishing: pretends to be a company or person to get credentials, money or clicks; tell \
    signs are a sender domain that doesn't belong to the brand named, threats of closing an \
    account, and urgent requests to log in or pay through a link\n\
    spam: other unsolicited junk";

/// The kinds `KIND_SYSTEM` offers.
const KINDS: [&str; 6] = [
    "personal",
    "account",
    "notification",
    "marketing",
    "phishing",
    "spam",
];

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
    let kind = parse_kind(&text_of(
        &send(
            llm,
            &kind_system(ctx),
            format!("{headers}\n{body}"),
            &[],
            &stream,
        )
        .await?,
    ));
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
    let digest = format!("{headers}Kind: {kind}\nSummary: {summary}\n");

    let verdict = text_of(&send(llm, &relevance_system(ctx), digest.clone(), &[], &stream).await?);
    if !verdict.trim_start().starts_with('1') {
        return Ok(format!(
            "{}: {kind}, not actionable — {summary}",
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
        "{}: {kind}, {actions} recorded — {summary}",
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

/// The first known kind in the reply, or "unknown".
fn parse_kind(reply: &str) -> &'static str {
    let reply = reply.to_lowercase();
    reply
        .split(|c: char| !c.is_alphabetic())
        .find_map(|word| KINDS.into_iter().find(|kind| *kind == word))
        .unwrap_or("unknown")
}

fn headers(message: &WindowRow, identifiers: &[String]) -> String {
    let name = message.author_display.as_deref().unwrap_or("?");
    let mut out = match message.author_address.as_deref() {
        Some(address) => format!("From: {name} <{address}>\n"),
        None => format!("From: {name}\n"),
    };
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

fn kind_system(ctx: &Context<'_>) -> String {
    let mut out = KIND_SYSTEM.to_string();
    if let Some(prompt) = ctx
        .profile
        .custom_prompt
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    {
        out.push_str(&format!(
            "\n\nAbout the user, to tell their real accounts and services from imitations: {}",
            prompt.trim()
        ));
    }
    out.push_str("\n\nReply with the one word and nothing else.");
    out
}

fn relevance_system(ctx: &Context<'_>) -> String {
    let mut out = format!(
        "You decide whether an email needs {} to do something. It is now {}.\n\n\
         Answer 1 only when the user must do something specific: a person asks them to, they \
         promised to, or something of theirs needs attention by a date (a failed payment, a \
         bill due, a vote or decision with a deadline, a security problem on their account).\n\n\
         Answer 0 for everything else, including:\n\
         - marketing and offers, even when they invite the reader to buy, sign up, enroll, \
         upgrade or try something\n\
         - newsletters, digests and social network notifications (invitations, likes, job \
         alerts)\n\
         - receipts, confirmations, shipping updates and one-time codes\n\
         - information without an ask, and asks aimed at someone else\n\
         - things already done\n\n\
         The Kind line comes from a classifier that has read the whole email: marketing, \
         spam and phishing are 0, since acting on them is never what the user needs.\n",
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

#[cfg(test)]
mod tests {
    use super::parse_kind;

    #[test]
    fn reads_the_kind_out_of_a_chatty_reply() {
        assert_eq!(parse_kind("Marketing."), "marketing");
        assert_eq!(parse_kind("**phishing**"), "phishing");
        assert_eq!(parse_kind("I think this is spam"), "spam");
        assert_eq!(parse_kind("no idea"), "unknown");
    }
}
