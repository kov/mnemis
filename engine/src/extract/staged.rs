//! Extraction in small, single-purpose turns, for models with a small context
//! that lose the thread in the agent loop: per message, classify it (marketing
//! and spam are dropped), summarise it (no tools), decide from both whether
//! it needs the user to act (a 0 or 1), and only then record the action from
//! the summary with a narrow tool set.
//!
//! Whether the sender looks like an impostor is decided in code, from
//! signals the model can't judge: a domain the user has never had mail from,
//! and a sender name that has nothing to do with that domain. A new domain
//! records the to-do with a warning; with the name on top, the message is a
//! warning only. The name alone means nothing: mailing services, bots and
//! people at their employer's domain make it common in genuine mail.

use std::time::Duration;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::prompt::{ExistingAction, render_recipients};
use super::{DryCall, ExtractionScope, UserProfile, WindowRow, dispatch_or_capture, tools};
use crate::agent;
use crate::llm::{ContentItem, InputItem, LlmTransport, OutputItem, ResponsesResponse, Role};
use crate::source::links::registrable;

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
    spam: unsolicited junk";

/// Kinds that are never actionable, whatever the email asks.
const IGNORED_KINDS: [&str; 2] = ["marketing", "spam"];

/// The kinds `KIND_SYSTEM` offers.
const KINDS: [&str; 5] = ["personal", "account", "notification", "marketing", "spam"];

/// Link domains named in a warning.
const WARNING_LINKS: usize = 3;

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

/// What a staged run made of one message.
pub(super) struct Outcome {
    /// A line on what was decided, for the caller to report.
    pub summary: String,
    /// Set when the sender is suspicious enough that the message is shown as
    /// a warning instead of a to-do.
    pub warning: Option<String>,
}

pub(super) async fn run(
    pool: &SqlitePool,
    llm: &dyn LlmTransport,
    scope: ExtractionScope,
    ctx: &Context<'_>,
    message: &WindowRow,
    mut dry: Option<&mut Vec<DryCall>>,
) -> Result<Outcome> {
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
    let quiet = |what: &str| Outcome {
        summary: format!("{}: {kind}, {what} — {summary}", message.external_id),
        warning: None,
    };
    if IGNORED_KINDS.contains(&kind) {
        return Ok(quiet("not actionable"));
    }
    let suspicion = suspicion(pool, message).await?;
    if suspicion.signals.len() >= 2 {
        return Ok(Outcome {
            summary: format!("{}: {kind}, warning only — {summary}", message.external_id),
            warning: Some(suspicion.warning()),
        });
    }
    let digest = format!("{headers}Kind: {kind}\nSummary: {summary}\n");

    let verdict = text_of(&send(llm, &relevance_system(ctx), digest.clone(), &[], &stream).await?);
    if !verdict.trim_start().starts_with('1') {
        return Ok(quiet("not actionable"));
    }
    let caution = (!suspicion.signals.is_empty()).then(|| suspicion.warning());

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
            let arguments = match (&caution, name.as_str()) {
                (Some(caution), "record_action") => with_caution(&arguments, caution),
                _ => arguments,
            };
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
    let flag = if caution.is_some() {
        ", with warning"
    } else {
        ""
    };
    Ok(quiet(&format!("{actions} recorded{flag}")))
}

/// Why a sender looks like an impostor; empty when it doesn't.
struct Suspicion {
    signals: Vec<String>,
    /// Where the message's links lead, other than the sender's own domain.
    links: Vec<String>,
}

impl Suspicion {
    fn warning(&self) -> String {
        let mut out = format!("Suspicious sender: {}", self.signals.join("; "));
        if !self.links.is_empty() {
            out.push_str(&format!("; its links go to {}", self.links.join(", ")));
        }
        out
    }
}

async fn suspicion(pool: &SqlitePool, message: &WindowRow) -> Result<Suspicion> {
    let mut found = Suspicion {
        signals: Vec::new(),
        links: Vec::new(),
    };
    let Some(host) = message
        .author_address
        .as_deref()
        .and_then(|a| a.rsplit_once('@'))
        .map(|(_, d)| d.to_lowercase())
    else {
        return Ok(found);
    };
    let domain = registrable(&host);
    let (earlier,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM messages m JOIN people p ON p.id = m.author_id \
         WHERE (p.external_id LIKE '%@' || ?1 OR p.external_id LIKE '%.' || ?1) \
         AND m.posted_at < ?2",
    )
    .bind(&domain)
    .bind(message.posted_at)
    .fetch_one(pool)
    .await
    .context("loading sender history")?;
    if earlier > 0 {
        return Ok(found);
    }
    found
        .signals
        .push(format!("the first email you've had from {domain}"));
    if let Some(name) = message.author_display.as_deref()
        && name_unrelated_to_host(name, &host)
    {
        found.signals.push(format!(
            "the name \"{name}\" has nothing to do with {domain}"
        ));
    }
    for link in &message.links {
        let to = registrable(&link.host);
        if to != domain && !found.links.contains(&to) && found.links.len() < WARNING_LINKS {
            found.links.push(to);
        }
    }
    Ok(found)
}

/// Whether a sender's name reads like someone other than its address's
/// host: none of its words (of four letters or more) appears in the host,
/// as "Bradesco Net Empresas" from `suporte5.ux6qrc.com` does, nor does the
/// registered domain appear in the name ("SpaceXAI" from `x.ai`).
fn name_unrelated_to_host(name: &str, host: &str) -> bool {
    let fold = |s: &str| -> String {
        s.to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
            .collect()
    };
    if name.contains('@') {
        return false;
    }
    let domain: String = fold(&registrable(host)).split_whitespace().collect();
    let host: String = fold(host).split_whitespace().collect();
    let name = fold(name);
    let words: Vec<&str> = name.split_whitespace().filter(|w| w.len() >= 4).collect();
    let joined: String = name.split_whitespace().collect();
    !words.is_empty()
        && !words.iter().any(|w| host.contains(w))
        && !host.contains(&joined)
        && !joined.contains(&domain)
}

/// A record_action call with the warning ahead of its details.
fn with_caution(arguments: &str, caution: &str) -> String {
    let Ok(mut args) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.to_string();
    };
    let details = args["details"].as_str().unwrap_or("").trim().to_string();
    args["details"] = if details.is_empty() {
        format!("⚠️ {caution}.")
    } else {
        format!("⚠️ {caution}.\n\n{details}")
    }
    .into();
    args.to_string()
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
         - things already done\n",
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
    use super::{name_unrelated_to_host, parse_kind, with_caution};

    #[test]
    fn tells_names_that_belong_to_their_host() {
        assert!(name_unrelated_to_host(
            "Bradesco Net Empresas|Portal Digital PJ",
            "suporte5.ux6qrc.com"
        ));
        assert!(name_unrelated_to_host("Exodus", "balaguer.cat"));
        assert!(!name_unrelated_to_host(
            "Amazon Web Services",
            "amazonaws.com"
        ));
        assert!(!name_unrelated_to_host("QuintoAndar", "quintoandar.com.br"));
        assert!(!name_unrelated_to_host(
            "Clube FII",
            "contato.clubefii.com.br"
        ));
        assert!(!name_unrelated_to_host("SpaceXAI", "x.ai"));
        assert!(!name_unrelated_to_host("a@b.com", "c.com"));
        assert!(!name_unrelated_to_host("X", "y.com"), "no word to compare");
    }

    #[test]
    fn puts_the_warning_ahead_of_the_details() {
        let args = with_caution(
            r#"{"title":"Pay","details":"Due Friday"}"#,
            "Suspicious sender: x",
        );
        let args: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(args["details"], "⚠️ Suspicious sender: x.\n\nDue Friday");
        assert_eq!(args["title"], "Pay");
    }

    #[test]
    fn reads_the_kind_out_of_a_chatty_reply() {
        assert_eq!(parse_kind("Marketing."), "marketing");
        assert_eq!(parse_kind("**spam**"), "spam");
        assert_eq!(parse_kind("I think this is spam"), "spam");
        assert_eq!(parse_kind("no idea"), "unknown");
    }
}
