use chrono::{DateTime, Utc};
use mnemis_types::FeedbackKind;

use crate::source::Recipient;

/// Inputs for prompt assembly. Pure-data so we can unit-test rendering.
pub struct PromptInputs<'a> {
    pub source_kind: &'a str,
    pub channel_name: &'a str,
    pub user_display_name: &'a str,
    pub user_identifiers: &'a [String],
    pub custom_prompt: Option<&'a str>,
    pub current_time: DateTime<Utc>,
    pub existing_actions: &'a [ExistingAction],
    /// Recent rows from `dismissal_feedback`, already scoped + capped by the
    /// caller. Each becomes a labelled negative example so the model learns
    /// to avoid analogous items going forward.
    pub feedback: &'a [FeedbackExample],
    pub window: &'a [WindowMessage],
}

pub struct ExistingAction {
    pub id: i64,
    pub title: String,
    pub details: Option<String>,
    pub due_at: Option<DateTime<Utc>>,
}

pub struct FeedbackExample {
    pub kind: FeedbackKind,
    pub example_text: String,
    /// User comment; may be empty (the dialog's Skip path writes no row at
    /// all, but a Submit with an empty textarea still inserts a row with
    /// `reason=""`. Treat empty as "no learning signal beyond the example").
    pub reason: String,
}

pub struct WindowMessage {
    pub external_id: String,
    pub posted_at: DateTime<Utc>,
    pub author: String,
    /// to/cc addressees, so the model can tell whether the user is a direct
    /// recipient (strong signal) or merely cc'd. Empty for chat sources.
    pub recipients: Vec<Recipient>,
    pub subject: Option<String>,
    /// Short preview of the body — the full body is NOT in the window.
    /// The model calls `fetch_messages` to read the full text of any message
    /// that looks like it might carry an ask (metadata-first extraction).
    pub snippet: String,
}

fn render_feedback_row(out: &mut String, f: &FeedbackExample) {
    // Squash multi-line example_text to a single bullet so the section stays
    // scannable for the model; the reason (when present) goes on its own line.
    let single_line = f.example_text.replace('\n', " ").trim().to_string();
    out.push_str(&format!("- {single_line}\n"));
    let reason = f.reason.trim();
    if !reason.is_empty() {
        out.push_str(&format!("  reason: {reason}\n"));
    }
}

pub fn build(inputs: &PromptInputs) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "You are mnemis's action extractor. Below is *metadata* for the recent \
         messages in this {} channel — sender, recipients, subject, and a short \
         snippet of each, but NOT the full body. Triage the metadata, call \
         fetch_messages to read the full text of anything that might carry an ask, \
         and record each genuine action item via record_action. Dismiss obvious \
         non-actionables (newsletters, FYIs, idle chatter) from the metadata alone — \
         don't fetch what you can already tell is noise.\n\n",
        inputs.source_kind
    ));

    out.push_str("# Who you work for\n");
    out.push_str(&format!("Display name: {}\n", inputs.user_display_name));
    if !inputs.user_identifiers.is_empty() {
        out.push_str(&format!(
            "Identifiers on this source: {}\n",
            inputs.user_identifiers.join(", ")
        ));
    }
    out.push('\n');

    out.push_str("# Current time\n");
    out.push_str(&inputs.current_time.to_rfc3339());
    out.push_str("\n\n");

    if let Some(prompt) = inputs.custom_prompt
        && !prompt.trim().is_empty()
    {
        out.push_str("# Context (user-provided)\n");
        out.push_str(prompt.trim());
        out.push_str("\n\n");
    }

    out.push_str(
        "# What counts as an action FOR THE USER\n\
         - An explicit ask directed at you (\"can you review #42?\")\n\
         - A commitment you made (\"I'll send it by Friday\") — your own outgoing messages count\n\
         - A deadline that affects you\n\n\
         # What does NOT count\n\
         - Information sharing without an ask\n\
         - General discussion or brainstorming\n\
         - Asks directed at someone else, even if you saw the conversation\n\
         - Things you've already done (mentioned as completed in the window)\n\n\
         # Confidence\n\
         - high: explicit ask or commitment with clear scope, directed at you\n\
         - medium: implied ask, ambiguous recipient, or vague scope\n\
         - low: speculative — usually skip\n\n",
    );

    if !inputs.existing_actions.is_empty() {
        out.push_str(
            "# Existing actions for this channel\n\
             These already exist. Each line is `[A-N] title` — if one of them needs more \
             info or a correction based on what's in the window, amend it with \
             update_action(action_id=\"A-N\", ...) instead of recording a new one. \
             If the window proves one is already done or no longer relevant, call \
             resolve_action(action_id=\"A-N\", status=...) so it stops cluttering the inbox.\n",
        );
        for a in inputs.existing_actions {
            let due = a
                .due_at
                .map(|d| format!(" — due {}", d.format("%Y-%m-%d")))
                .unwrap_or_default();
            let details = a
                .details
                .as_deref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default();
            out.push_str(&format!("[A-{}] {}{}{}\n", a.id, a.title, due, details));
        }
        out.push('\n');
    }

    if !inputs.feedback.is_empty() {
        out.push_str(
            "# Negative examples from past feedback\n\
             The user previously told you these were wrong. Use them to calibrate \
             what NOT to surface or auto-claim from this channel. If a comment is \
             present it's the user's reason; if not, treat the example itself as \
             the only signal.\n",
        );
        let dismissed: Vec<&FeedbackExample> = inputs
            .feedback
            .iter()
            .filter(|f| matches!(f.kind, FeedbackKind::Dismissed))
            .collect();
        let wrong_auto: Vec<&FeedbackExample> = inputs
            .feedback
            .iter()
            .filter(|f| matches!(f.kind, FeedbackKind::WrongAutoClaim))
            .collect();
        if !dismissed.is_empty() {
            out.push_str("## Dismissed — \"this isn't an action item for me\"\n");
            for f in &dismissed {
                render_feedback_row(&mut out, f);
            }
        }
        if !wrong_auto.is_empty() {
            out.push_str("## Wrongly auto-claimed — \"don't auto-claim items like this\"\n");
            for f in &wrong_auto {
                render_feedback_row(&mut out, f);
            }
        }
        out.push('\n');
    }

    out.push_str(
        "# Tools\n\
         - fetch_messages(external_ids[]): read the full bodies of one or more messages by \
           external_id. Batch the ids — pass several at once to save round-trips. The window \
           shows only snippets, so call this before recording an action whenever the snippet \
           alone doesn't confirm the ask. There's a per-run fetch budget, so fetch what looks \
           promising, not everything.\n\
         - search_messages(query): keyword/semantic search across recent messages for prior context\n\
         - record_action(title, details, confidence, rationale, due_at?, evidence_external_ids[]): \
           record one concrete action the user must take. Returns {\"action_id\": \"A-N\", ...} — \
           hold onto the A-N id if you might need to revise the same action later in this response. \
           Never call this to report the *absence* of actions: if nothing is actionable, record \
           nothing and say so in your final message. Set due_at to an ISO 8601 timestamp whenever \
           the message gives a concrete date or deadline (\"by Friday\", \"vence 16/06/2026\", \
           \"expires soon — 06/12\"); resolve relative dates against the current time above. Leave \
           it null only when there is genuinely no date.\n\
         - update_action(action_id, ...): amend an action you already recorded (or one from \
           the Existing list above). Pass only the fields you want to change; extra evidence \
           is appended, not replaced. Use this whenever you'd otherwise be tempted to record \
           the same underlying item a second time.\n\
         - resolve_action(action_id, status, confidence, rationale, evidence_external_ids): \
           mark a prior action as done or cancelled because the window proves it. \
           High-confidence applies immediately; medium/low queues a suggestion for the user. \
           Use this when you'd otherwise just record nothing about an item that's clearly resolved.\n\n\
         # Process\n\
         1. Scan the metadata window below. Dismiss obvious non-actionables from sender, \
            subject, and snippet alone.\n\
         2. For anything that might carry an ask but whose snippet doesn't settle it, \
            fetch_messages the full body (batch the ids in one call). Use search_messages for \
            prior context if you need it.\n\
         3. Judge each candidate against the criteria. Create exactly one action per actionable \
            item; if the same item recurs (or you learn more), amend it via update_action rather \
            than recording it twice.\n\
         4. Stop when finished. Your final message MUST reflect what you actually did — if \
            you recorded actions, summarize them briefly; if nothing was actionable, record \
            NOTHING and just say so here. A \"no actions found\" note belongs in this message, \
            never via record_action.\n\n",
    );

    out.push_str(&format!(
        "# Window — channel \"{}\" ({} new messages, metadata only)\n\n",
        inputs.channel_name,
        inputs.window.len()
    ));

    for m in inputs.window {
        out.push_str(&format!(
            "[msg id={} @{} from \"{}\"]\n",
            m.external_id,
            m.posted_at.to_rfc3339(),
            m.author
        ));
        render_recipients(&mut out, &m.recipients, inputs.user_identifiers);
        if let Some(s) = &m.subject
            && !s.is_empty()
        {
            out.push_str(&format!("Subject: {s}\n"));
        }
        out.push_str(&format!("Snippet: {}\n", m.snippet));
        out.push_str("---\n");
    }

    out
}

/// Render `To:`/`Cc:` lines for a message, tagging any addressee whose address
/// matches one of the user's identifiers with `(you)` — the direct-recipient
/// signal the model uses to tell "addressed to me" from "merely cc'd". Emits
/// nothing for a kind with no recipients (e.g. chat sources have none).
fn render_recipients(out: &mut String, recipients: &[Recipient], user_identifiers: &[String]) {
    let is_self = |r: &Recipient| {
        r.address
            .as_deref()
            .is_some_and(|a| user_identifiers.iter().any(|id| id.eq_ignore_ascii_case(a)))
    };
    for (kind, label) in [("to", "To"), ("cc", "Cc")] {
        let people: Vec<String> = recipients
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| {
                let who = match (r.name.as_deref(), r.address.as_deref()) {
                    (Some(n), Some(a)) => format!("{n} <{a}>"),
                    (Some(n), None) => n.to_string(),
                    (None, Some(a)) => a.to_string(),
                    (None, None) => "?".to_string(),
                };
                if is_self(r) {
                    format!("{who} (you)")
                } else {
                    who
                }
            })
            .collect();
        if !people.is_empty() {
            out.push_str(&format!("{label}: {}\n", people.join(", ")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(ts: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(ts, 0).unwrap()
    }

    fn msg(external_id: &str, subject: Option<&str>, snippet: &str) -> WindowMessage {
        WindowMessage {
            external_id: external_id.to_string(),
            posted_at: dt(1_699_999_000),
            author: "Ana <ana@example.com>".to_string(),
            recipients: Vec::new(),
            subject: subject.map(str::to_string),
            snippet: snippet.to_string(),
        }
    }

    #[test]
    fn renders_minimum_prompt() {
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &["gustavo@example.com".to_string()],
            custom_prompt: None,
            current_time: dt(1_700_000_000),
            existing_actions: &[],
            feedback: &[],
            window: &[msg(
                "msg-1",
                Some("Hello"),
                "Can you take a pass by Friday?",
            )],
        };
        let rendered = build(&inputs);
        assert!(rendered.contains("Display name: Gustavo"));
        assert!(rendered.contains("gustavo@example.com"));
        assert!(rendered.contains("[msg id=msg-1"));
        assert!(rendered.contains("Subject: Hello"));
        assert!(rendered.contains("by Friday"));
        assert!(rendered.contains("# Window"));
    }

    #[test]
    fn prompt_forbids_recording_non_actions_and_asks_for_due_dates() {
        // Two extraction-quality guards live in the prompt: (1) the model must
        // never record_action a "no actions found" entry — absence is reported
        // in the final message; (2) when a message states a concrete date, the
        // action must carry a due_at (it powers calendar sync). Pin both so a
        // later prompt edit can't silently drop them.
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &[],
            custom_prompt: None,
            current_time: dt(1_700_000_000),
            existing_actions: &[],
            feedback: &[],
            window: &[msg("msg-1", Some("Hi"), "Anything to do?")],
        };
        let rendered = build(&inputs);
        assert!(
            rendered.contains("Never call this to report the *absence* of actions"),
            "prompt must forbid recording a non-action as an action"
        );
        assert!(
            rendered.contains("never via record_action"),
            "process step must route no-action turns to the final message, not record_action"
        );
        assert!(
            rendered.contains("Set due_at to an ISO 8601 timestamp whenever"),
            "prompt must tell the model to capture concrete dates into due_at"
        );
    }

    #[test]
    fn window_shows_snippet_and_frames_fetch_messages_as_the_body_path() {
        // Pin metadata-first: the window carries only a snippet (never the full
        // body), and the prompt must tell the model to fetch_messages for the
        // real text. This is the bound that keeps a backlog of long emails from
        // blowing the context window.
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &[],
            custom_prompt: None,
            current_time: dt(1_700_000_000),
            existing_actions: &[],
            feedback: &[],
            window: &[msg(
                "msg-1",
                Some("Daily report"),
                "First 200 chars of the body…",
            )],
        };
        let rendered = build(&inputs);
        assert!(
            rendered.contains("Snippet: First 200 chars of the body…"),
            "the window should render the snippet line"
        );
        // The full body is never in the window; fetch_messages is the path.
        assert!(
            rendered.contains("fetch_messages"),
            "prompt must point the model at fetch_messages for full bodies"
        );
        assert!(
            rendered.contains("metadata only"),
            "the window header should flag that only metadata is shown"
        );
    }

    #[test]
    fn renders_to_cc_and_marks_the_user_as_a_direct_recipient() {
        // The direct-recipient-vs-cc signal: an addressee matching one of the
        // user's identifiers is tagged "(you)" so the model can weight an
        // ask addressed *to* the user over one they were merely cc'd on.
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &["gustavo@example.com".to_string()],
            custom_prompt: None,
            current_time: dt(1_700_000_000),
            existing_actions: &[],
            feedback: &[],
            window: &[WindowMessage {
                external_id: "msg-1".to_string(),
                posted_at: dt(1_699_999_000),
                author: "Ana <ana@example.com>".to_string(),
                recipients: vec![
                    Recipient {
                        kind: "to".to_string(),
                        name: Some("Gustavo".to_string()),
                        address: Some("gustavo@example.com".to_string()),
                    },
                    Recipient {
                        kind: "cc".to_string(),
                        name: Some("Bob".to_string()),
                        address: Some("bob@example.com".to_string()),
                    },
                ],
                subject: Some("Review".to_string()),
                snippet: "please take a look".to_string(),
            }],
        };
        let rendered = build(&inputs);
        assert!(
            rendered.contains("To: Gustavo <gustavo@example.com> (you)"),
            "direct recipient matching a user identifier should be tagged (you). prompt:\n{rendered}"
        );
        assert!(
            rendered.contains("Cc: Bob <bob@example.com>"),
            "cc line should render without a (you) tag for non-self addressees"
        );
    }

    #[test]
    fn prompt_mentions_resolve_action_in_both_existing_actions_and_tools_sections() {
        // Pin: the agent must learn it can close out stale items, and the
        // hint must appear in both the existing-actions block (where the IDs
        // live) and the tools block (where the schema lives). Easy to lose
        // either reference during prompt edits.
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &[],
            custom_prompt: None,
            current_time: dt(1_700_000_000),
            existing_actions: &[ExistingAction {
                id: 7,
                title: "Send the contract".to_string(),
                details: None,
                due_at: None,
            }],
            feedback: &[],
            window: &[],
        };
        let rendered = build(&inputs);
        let existing_section_idx = rendered
            .find("# Existing actions for this channel")
            .expect("existing-actions section missing");
        let tools_idx = rendered.find("# Tools").expect("tools section missing");
        let after_existing = &rendered[existing_section_idx..tools_idx];
        let after_tools = &rendered[tools_idx..];
        assert!(
            after_existing.contains("resolve_action"),
            "existing-actions block must hint at resolve_action"
        );
        assert!(
            after_tools.contains("resolve_action("),
            "tools block must define resolve_action"
        );
    }

    #[test]
    fn includes_existing_actions_with_ids() {
        let inputs = PromptInputs {
            source_kind: "mattermost",
            channel_name: "#eng",
            user_display_name: "Gustavo",
            user_identifiers: &[],
            custom_prompt: Some("Ana is my direct report."),
            current_time: dt(1_700_000_000),
            existing_actions: &[ExistingAction {
                id: 42,
                title: "Review the Q3 draft".to_string(),
                details: Some("Ana asked Monday".to_string()),
                due_at: Some(dt(1_700_500_000)),
            }],
            feedback: &[],
            window: &[],
        };
        let rendered = build(&inputs);
        assert!(rendered.contains("[A-42] Review the Q3 draft"));
        assert!(rendered.contains("Ana is my direct report"));
    }

    #[test]
    fn omits_custom_prompt_section_when_empty() {
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &[],
            custom_prompt: Some("   "),
            current_time: dt(1_700_000_000),
            existing_actions: &[],
            feedback: &[],
            window: &[],
        };
        let rendered = build(&inputs);
        assert!(!rendered.contains("# Context (user-provided)"));
    }

    #[test]
    fn renders_feedback_grouped_by_kind_with_reasons_when_present() {
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &[],
            custom_prompt: None,
            current_time: dt(1_700_000_000),
            existing_actions: &[],
            feedback: &[
                FeedbackExample {
                    kind: FeedbackKind::Dismissed,
                    example_text: "Billing reminder: pay invoice 42".to_string(),
                    reason: "billing emails aren't actionable for me".to_string(),
                },
                FeedbackExample {
                    kind: FeedbackKind::WrongAutoClaim,
                    // Empty reason: user took the action out of band; the
                    // example itself is the only learning signal.
                    example_text: "Confirm meeting with Sam".to_string(),
                    reason: String::new(),
                },
            ],
            window: &[],
        };
        let rendered = build(&inputs);
        assert!(
            rendered.contains("# Negative examples"),
            "feedback section header missing. prompt:\n{rendered}"
        );
        assert!(rendered.contains("## Dismissed"));
        assert!(rendered.contains("Billing reminder: pay invoice 42"));
        assert!(rendered.contains("reason: billing emails aren't actionable"));
        assert!(rendered.contains("## Wrongly auto-claimed"));
        assert!(rendered.contains("Confirm meeting with Sam"));
        // Empty reasons render without the "reason:" line — would just
        // confuse the model otherwise.
        let after_sam = rendered
            .split("Confirm meeting with Sam")
            .nth(1)
            .unwrap_or("");
        assert!(
            !after_sam.starts_with("\n  reason:"),
            "empty reason should not be rendered. context:\n{after_sam}"
        );
    }

    #[test]
    fn omits_feedback_section_when_empty() {
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &[],
            custom_prompt: None,
            current_time: dt(1_700_000_000),
            existing_actions: &[],
            feedback: &[],
            window: &[],
        };
        let rendered = build(&inputs);
        assert!(!rendered.contains("Negative examples"));
    }
}
