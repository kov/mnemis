use chrono::{DateTime, Utc};

/// Max chars of a message body rendered into the window. A single huge email
/// (financial daily reports, mailing-list digests) can otherwise blow the
/// model's context window. The model still has `fetch_message` to pull the
/// full body when it actually needs it.
const BODY_PROMPT_CAP_CHARS: usize = 4000;

/// Inputs for prompt assembly. Pure-data so we can unit-test rendering.
pub struct PromptInputs<'a> {
    pub source_kind: &'a str,
    pub channel_name: &'a str,
    pub user_display_name: &'a str,
    pub user_identifiers: &'a [String],
    pub custom_prompt: Option<&'a str>,
    pub current_time: DateTime<Utc>,
    pub existing_actions: &'a [ExistingAction],
    pub window: &'a [WindowMessage],
}

pub struct ExistingAction {
    pub id: i64,
    pub title: String,
    pub details: Option<String>,
    pub due_at: Option<DateTime<Utc>>,
}

pub struct WindowMessage {
    pub external_id: String,
    pub posted_at: DateTime<Utc>,
    pub author: String,
    pub subject: Option<String>,
    pub body: String,
}

pub fn build(inputs: &PromptInputs) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "You are mnemis's action extractor. Your job is to identify action items \
         from the recent activity in this {} channel and record each via the \
         record_action tool.\n\n",
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
             update_action(action_id=\"A-N\", ...) instead of recording a new one.\n",
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

    out.push_str(
        "# Tools\n\
         - search_messages(query): keyword/semantic search across recent messages for context\n\
         - fetch_message(external_id): full body of one message\n\
         - record_action(title, details, confidence, rationale, due_at?, evidence_external_ids[]): \
           record one action. Returns {\"action_id\": \"A-N\", ...} — hold onto the A-N id if \
           you might need to revise the same action later in this response.\n\
         - update_action(action_id, ...): amend an action you already recorded (or one from \
           the Existing list above). Pass only the fields you want to change; extra evidence \
           is appended, not replaced. Use this whenever you'd otherwise be tempted to record \
           the same underlying item a second time.\n\n\
         # Process\n\
         1. Read the window below.\n\
         2. For each actionable thing: judge against the criteria. Use the tools if you need prior context.\n\
         3. Create exactly one action per actionable item. If you later spot the same item \
            being mentioned again (or with more detail), amend the existing action via \
            update_action rather than recording another one.\n\
         4. Stop when finished. Your final message MUST reflect what you actually did — if \
            you recorded actions, summarize them briefly; if you recorded none, say so. Do \
            not say \"no actions found\" after calling record_action.\n\n",
    );

    out.push_str(&format!(
        "# Window — channel \"{}\" ({} new messages)\n\n",
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
        if let Some(s) = &m.subject
            && !s.is_empty()
        {
            out.push_str(&format!("Subject: {s}\n"));
        }
        if m.body.len() > BODY_PROMPT_CAP_CHARS {
            // Cap on byte length, but slice on a char boundary so we don't
            // panic on multi-byte UTF-8 (matters for non-ASCII bodies).
            let mut end = BODY_PROMPT_CAP_CHARS;
            while !m.body.is_char_boundary(end) {
                end -= 1;
            }
            out.push_str(&m.body[..end]);
            out.push_str(&format!(
                "\n[... {} chars truncated; fetch_message(\"{}\") for full body]",
                m.body.len() - end,
                m.external_id,
            ));
        } else {
            out.push_str(&m.body);
        }
        out.push_str("\n---\n");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(ts: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(ts, 0).unwrap()
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
            window: &[WindowMessage {
                external_id: "msg-1".to_string(),
                posted_at: dt(1_699_999_000),
                author: "Ana <ana@example.com>".to_string(),
                subject: Some("Hello".to_string()),
                body: "Can you take a pass by Friday?".to_string(),
            }],
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
    fn long_message_bodies_are_capped_with_a_marker_pointing_to_fetch_message() {
        // Pin: a single huge body must not flood the prompt. The model still
        // has fetch_message available if it needs the full content. This is
        // the bound that keeps a backlog of long emails from blowing the
        // context window (observed: 100 messages × ~3K tokens each).
        let huge = "X".repeat(50_000);
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &[],
            custom_prompt: None,
            current_time: dt(1_700_000_000),
            existing_actions: &[],
            window: &[WindowMessage {
                external_id: "msg-huge".to_string(),
                posted_at: dt(1_699_999_000),
                author: "Ana".to_string(),
                subject: Some("Daily report".to_string()),
                body: huge,
            }],
        };
        let rendered = build(&inputs);
        let body_x_count = rendered.matches('X').count();
        assert!(
            body_x_count < 10_000,
            "body should be truncated; saw {body_x_count} X chars"
        );
        assert!(
            rendered.contains("truncated"),
            "truncation marker missing from prompt"
        );
        assert!(
            rendered.contains("fetch_message(\"msg-huge\")"),
            "marker should tell the model how to get the full body"
        );
    }

    #[test]
    fn short_message_bodies_are_left_untouched() {
        let inputs = PromptInputs {
            source_kind: "imap",
            channel_name: "INBOX",
            user_display_name: "Gustavo",
            user_identifiers: &[],
            custom_prompt: None,
            current_time: dt(1_700_000_000),
            existing_actions: &[],
            window: &[WindowMessage {
                external_id: "msg-tiny".to_string(),
                posted_at: dt(1_699_999_000),
                author: "Ana".to_string(),
                subject: None,
                body: "Short ask".to_string(),
            }],
        };
        let rendered = build(&inputs);
        assert!(rendered.contains("Short ask"));
        assert!(!rendered.contains("truncated"));
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
            window: &[],
        };
        let rendered = build(&inputs);
        assert!(!rendered.contains("# Context (user-provided)"));
    }
}
