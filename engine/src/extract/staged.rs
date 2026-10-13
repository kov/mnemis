//! Extraction in small, single-purpose turns, for models with a small context
//! that lose the thread in the agent loop: per message, classify it (marketing
//! and spam are dropped), summarise it (no tools), decide from both whether
//! it needs the user to act (a 0 or 1), and only then record the action from
//! the summary with a narrow tool set. A message that needs nothing new is
//! checked against the open to-dos instead: when it shows one done (a payment
//! received, a vote cast), resolving it is suggested for the user to confirm.
//!
//! Whether the sender looks like an impostor is decided in code, from
//! signals the model can't judge (see `mail::suspicion`): a forged date, a
//! sender its domain didn't authorize, misleading links, a domain the user
//! has never had mail from, and a sender name that has nothing to do with
//! that domain, counted only with the new domain (alone it means nothing:
//! mailing services, bots and people at their employer's domain make it
//! common in genuine mail). One signal records the to-do with a warning; two
//! make the message a warning only.

use std::time::Duration;

use anyhow::Result;
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
    /// To-dos recorded.
    pub actions: usize,
    /// The model's summary of the message.
    pub gist: String,
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
        actions: 0,
        gist: summary.clone(),
    };
    if IGNORED_KINDS.contains(&kind) {
        return Ok(quiet("not actionable"));
    }
    let suspicion = suspicion(pool, message).await?;
    if suspicion.signals.len() >= 2 {
        return Ok(Outcome {
            summary: format!("{}: {kind}, warning only — {summary}", message.external_id),
            warning: Some(suspicion.warning()),
            actions: 0,
            gist: summary,
        });
    }
    let digest = format!("{headers}Kind: {kind}\nSummary: {summary}\n");

    let verdict = text_of(&send(llm, &relevance_system(ctx), digest.clone(), &[], &stream).await?);
    if !verdict.trim_start().starts_with('1') {
        let Some(open) = resolves(llm, ctx, message, kind, &digest, &stream).await? else {
            return Ok(quiet("not actionable"));
        };
        let args = serde_json::json!({
            "action_id": format!("A-{}", open.id),
            "status": "done",
            // Only suggested: the user confirms what a small model concludes.
            "confidence": "medium",
            "rationale": summary.chars().take(200).collect::<String>(),
            "evidence_external_ids": [message.external_id],
        });
        dispatch_or_capture(
            pool,
            scope,
            &mut tools::FetchBudget::new(0),
            dry.as_deref_mut(),
            "resolve_action",
            &args.to_string(),
        )
        .await;
        return Ok(quiet(&format!("resolves A-{} {}", open.id, open.title)));
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
    Ok(Outcome {
        actions,
        ..quiet(&format!("{actions} recorded{flag}"))
    })
}

/// The open to-do the message shows done or no longer needed, if any. Only
/// account mail and notifications confirm things, and only to-dos that share
/// a distinctive word with the message (a name, a ticker, an order number)
/// are offered: given the whole list, the small model picks one for almost
/// any email.
async fn resolves<'a>(
    llm: &dyn LlmTransport,
    ctx: &'a Context<'_>,
    message: &WindowRow,
    kind: &str,
    digest: &str,
    stream: &agent::StreamCtx,
) -> Result<Option<&'a ExistingAction>> {
    if kind != "account" && kind != "notification" {
        return Ok(None);
    }
    let text = format!(
        "{} {}",
        message.subject.as_deref().unwrap_or(""),
        message.body
    )
    .to_lowercase();
    let candidates: Vec<&ExistingAction> = ctx
        .existing
        .iter()
        .filter(|a| {
            // A to-do that names a number (an order, a contract) is only
            // about mail carrying that number.
            let (numbers, names): (Vec<String>, Vec<String>) =
                distinctive_words(&a.title).partition(|w| w.chars().any(|c| c.is_ascii_digit()));
            if numbers.is_empty() {
                names.iter().any(|w| text.contains(w))
            } else {
                numbers.iter().any(|w| text.contains(w))
            }
        })
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    let mut system = format!(
        "These are the open to-dos of {}. Does the email below show that one of them is \
         done or no longer needed: paid, voted, signed, delivered, picked up, confirmed, or \
         cancelled? Only an email about that very to-do counts, not one on a similar \
         subject.\n\n",
        who(ctx)
    );
    for (n, action) in candidates.iter().enumerate() {
        system.push_str(&format!("{}. {}\n", n + 1, action.title));
    }
    system.push_str("\nReply with the to-do's number, or 0 when none is done.");
    let reply = text_of(&send(llm, &system, digest.to_string(), &[], stream).await?);
    let n: usize = reply
        .split(|c: char| !c.is_ascii_digit())
        .find(|w| !w.is_empty())
        .and_then(|w| w.parse().ok())
        .unwrap_or(0);
    let Some(chosen) = n.checked_sub(1).and_then(|i| candidates.get(i).copied()) else {
        return Ok(None);
    };
    // Picking from a list, the model reaches for the nearest subject; asked
    // about the one to-do alone, it tells "same company" from "this is done".
    let check = format!(
        "The user has this to-do: \"{}\". Does the email below show that this very to-do \
         is done or no longer needed? An email from the same company about something else \
         does not count. Reply with the single digit 1 or 0.",
        chosen.title
    );
    let verdict = text_of(&send(llm, &check, digest.to_string(), &[], stream).await?);
    Ok(verdict.trim_start().starts_with('1').then_some(chosen))
}

/// The words of a to-do title that name something, lowercased: those with
/// a digit ("KNSC11", "#702-7145333") or capitalised past the first word
/// ("Amazon", "Tesouro"), three characters or more.
fn distinctive_words(title: &str) -> impl Iterator<Item = String> + '_ {
    title
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| w.chars().count() >= 3)
        .enumerate()
        .filter(|(i, w)| {
            w.chars().any(|c| c.is_ascii_digit())
                || (*i > 0 && w.chars().next().is_some_and(char::is_uppercase))
        })
        .map(|(_, w)| w.to_lowercase())
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
    use crate::mail::suspicion::{first_from_domain, misleading_links, name_unrelated_to_host};
    let mut found = Suspicion {
        signals: crate::mail::misdated(message.posted_at, message.received_at)
            .map(|why| format!("its date is forged ({why})"))
            .into_iter()
            .collect(),
        links: Vec::new(),
    };
    found
        .signals
        .extend(message.auth.as_ref().and_then(|a| a.failure()));
    let Some(address) = message.author_address.as_deref() else {
        return Ok(found);
    };
    let Some(host) = address.rsplit_once('@').map(|(_, d)| d.to_lowercase()) else {
        return Ok(found);
    };
    let domain = registrable(&host);
    if let Some((shown, to)) = misleading_links(&message.links, Some(&domain)).first() {
        found
            .signals
            .push(format!("a link showing {shown} leads to {to}"));
    }
    if !first_from_domain(pool, address, message.posted_at).await? {
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
    if let Some(rule) =
        crate::settings::language_rule(ctx.profile.language.as_deref(), "the title and details")
    {
        out.push_str(&rule);
        out.push('\n');
    }
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
    use super::{distinctive_words, parse_kind, with_caution};
    use crate::mail::suspicion::name_unrelated_to_host;

    #[tokio::test]
    async fn a_forged_date_is_a_signal_whatever_the_sender() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let pool = crate::db::open(&tmp.path().join("t.db")).await?;
        crate::db::migrate(&pool).await?;
        let arrived = 1_790_000_000;
        let message = super::WindowRow {
            id: 1,
            external_id: "x".into(),
            posted_at: arrived + 120 * 86_400,
            subject: None,
            body: String::new(),
            author_display: Some("Ana".into()),
            author_address: Some("ana@example.com".into()),
            recipients: Vec::new(),
            links: Vec::new(),
            received_at: Some(arrived),
            auth: None,
            warnings: Vec::new(),
        };
        let found = super::suspicion(&pool, &message).await?;
        assert_eq!(
            found.signals,
            ["its date is forged (dated 4 months after it arrived)"]
        );
        Ok(())
    }

    #[test]
    fn picks_the_words_that_name_something() {
        let words: Vec<String> =
            distinctive_words("Garantir saldo na Itaú CV para investimento Tesouro Direto")
                .collect();
        assert_eq!(words, ["itaú", "tesouro", "direto"]);
        let words: Vec<String> =
            distinctive_words("Update Amazon payment method for order #702-7145333").collect();
        assert_eq!(words, ["amazon", "702-7145333"]);
        let words: Vec<String> =
            distinctive_words("Exercise subscription right for KNSC11").collect();
        assert_eq!(words, ["knsc11"]);
    }

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
