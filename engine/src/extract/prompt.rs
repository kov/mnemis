use chrono::{DateTime, Utc};

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
        out.push_str("# Existing pending actions for this channel (do NOT duplicate)\n");
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
           record one action; evidence_external_ids must reference at least one message\n\n\
         # Process\n\
         1. Read the window below.\n\
         2. For each candidate: judge against the criteria. Use the tools if you need prior context.\n\
         3. Call record_action for each genuine action. Skip if it matches an existing pending action.\n\
         4. Stop when finished. If no actions, just stop.\n\n",
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
        out.push_str(&m.body);
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
