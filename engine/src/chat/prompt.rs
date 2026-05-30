//! System prompt for the chat agent. Kept separate from the extraction prompt
//! (`extract::prompt`) on purpose: this one frames an interactive assistant
//! that answers the user and acts on their behalf, not a batch triager.

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::SqlitePool;

use crate::extract::tools::snippet;

/// Everything the rendered prompt needs, gathered from the DB by
/// [`build_system_prompt`]. Split out so [`render`] stays pure and testable.
pub struct RenderInputs {
    pub now_iso: String,
    pub user_display_name: String,
    pub custom_prompt: Option<String>,
    /// A pre-rendered "this conversation is about X" block for a seeded chat.
    pub seed: Option<String>,
}

/// Assemble the system prompt from already-gathered parts (no I/O).
pub fn render(inputs: &RenderInputs) -> String {
    let mut out = String::new();
    out.push_str(
        "You are mnemis's assistant — a helper inside the user's personal action-tracking app. \
         You help them understand and manage their messages and action items.\n\n\
         You have tools to search and read the user's ingested messages (across every source) \
         and to look up, create, update, and resolve their action items. Ground every claim in \
         what the tools actually return, and cite message external_ids when you explain why \
         something is (or isn't) an action.\n\n\
         When the user explicitly asks you to track, complete, or change something, act on it — \
         use high confidence so the change applies immediately. When you are merely suggesting \
         something, use medium or low confidence so the user can confirm it. Never invent actions \
         or evidence; if you can't find support for a claim, say so plainly.\n\n",
    );

    out.push_str("# You are helping\n");
    out.push_str(&format!("Display name: {}\n", inputs.user_display_name));
    if let Some(cp) = inputs.custom_prompt.as_deref()
        && !cp.trim().is_empty()
    {
        out.push_str("\nWhat they told you about themselves and their priorities:\n");
        out.push_str(cp.trim());
        out.push('\n');
    }

    out.push_str("\n# Current time\n");
    out.push_str(&inputs.now_iso);
    out.push_str("\nResolve relative dates (\"Friday\", \"tomorrow\") against this.\n");

    if let Some(seed) = inputs.seed.as_deref() {
        out.push_str("\n# This conversation\n");
        out.push_str(seed);
        out.push('\n');
    }

    out.push_str("\nAnswer concisely and conversationally.\n");
    out
}

/// Build the chat system prompt for `chat_id`, loading the user profile and —
/// when the chat was seeded from an entity ("Talk about this") — a short
/// summary of that entity so the agent starts with context.
pub async fn build_system_prompt(pool: &SqlitePool, chat_id: i64) -> Result<String> {
    let profile = crate::settings::get_user_profile(pool)
        .await
        .context("loading user profile for chat prompt")?;

    let seed = load_seed(pool, chat_id).await?;

    Ok(render(&RenderInputs {
        now_iso: Utc::now().to_rfc3339(),
        user_display_name: profile.display_name,
        custom_prompt: profile.custom_prompt,
        seed,
    }))
}

/// A short, user-facing label for a seeded chat — shown as a banner in the UI
/// so the user can see what the conversation is grounded in. `None` for a blank
/// chat (or a seed that no longer resolves). Distinct from [`load_seed`], which
/// builds the verbose, model-directed block injected into the prompt.
pub async fn seed_label(pool: &SqlitePool, chat_id: i64) -> Result<Option<String>> {
    let row: Option<(Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT seeded_from_kind, seeded_from_id FROM chats WHERE id = ?")
            .bind(chat_id)
            .fetch_optional(pool)
            .await
            .context("loading chat seed label")?;
    let Some((Some(kind), Some(id))) = row else {
        return Ok(None);
    };

    match kind.as_str() {
        "action" => {
            let r: Option<(String,)> = sqlx::query_as("SELECT title FROM actions WHERE id = ?")
                .bind(id)
                .fetch_optional(pool)
                .await?;
            Ok(r.map(|(title,)| format!("action A-{id} \u{00b7} {title}")))
        }
        "message" => {
            let r: Option<(Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT m.subject, p.display_name FROM messages m \
                 LEFT JOIN people p ON p.id = m.author_id WHERE m.id = ?",
            )
            .bind(id)
            .fetch_optional(pool)
            .await?;
            Ok(r.map(|(subject, author)| {
                let who = author.unwrap_or_else(|| "someone".to_string());
                let subj = subject
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "(no subject)".to_string());
                format!("message from {who} \u{00b7} \u{201c}{subj}\u{201d}")
            }))
        }
        _ => Ok(None),
    }
}

/// Render the "this conversation is about X" block for a seeded chat, or `None`
/// for a blank chat / a seed that no longer resolves.
async fn load_seed(pool: &SqlitePool, chat_id: i64) -> Result<Option<String>> {
    let row: Option<(Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT seeded_from_kind, seeded_from_id FROM chats WHERE id = ?")
            .bind(chat_id)
            .fetch_optional(pool)
            .await
            .context("loading chat seed")?;
    let Some((Some(kind), Some(id))) = row else {
        return Ok(None);
    };

    match kind.as_str() {
        "action" => seed_for_action(pool, id).await,
        "message" => seed_for_message(pool, id).await,
        _ => Ok(None),
    }
}

async fn seed_for_action(pool: &SqlitePool, action_id: i64) -> Result<Option<String>> {
    #[allow(clippy::type_complexity)]
    let row: Option<(String, Option<String>, String, String, Option<String>)> = sqlx::query_as(
        "SELECT title, details, confidence, status, rationale FROM actions WHERE id = ?",
    )
    .bind(action_id)
    .fetch_optional(pool)
    .await
    .context("loading seed action")?;
    let Some((title, details, confidence, status, rationale)) = row else {
        return Ok(None);
    };

    let evidence: Vec<(String,)> = sqlx::query_as(
        "SELECT m.external_id FROM action_evidence ae \
         JOIN messages m ON m.id = ae.message_id \
         WHERE ae.action_id = ? ORDER BY ae.is_primary DESC",
    )
    .bind(action_id)
    .fetch_all(pool)
    .await
    .context("loading seed action evidence")?;
    let ev_ids: Vec<String> = evidence.into_iter().map(|(e,)| e).collect();

    let mut s = format!(
        "This conversation is about action A-{action_id}: \"{title}\" \
         (status: {status}, confidence: {confidence})."
    );
    if let Some(d) = details.filter(|d| !d.trim().is_empty()) {
        s.push_str(&format!(" Details: {d}"));
    }
    if let Some(r) = rationale.filter(|r| !r.trim().is_empty()) {
        s.push_str(&format!(" Why it was recorded: {r}"));
    }
    if !ev_ids.is_empty() {
        s.push_str(&format!(
            " Evidence messages: {}. Call get_action(\"A-{action_id}\") for the full record and \
             fetch_messages to read the evidence.",
            ev_ids.join(", ")
        ));
    }
    Ok(Some(s))
}

async fn seed_for_message(pool: &SqlitePool, message_id: i64) -> Result<Option<String>> {
    let row: Option<(String, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT m.external_id, m.subject, p.display_name, m.body \
         FROM messages m LEFT JOIN people p ON p.id = m.author_id \
         WHERE m.id = ?",
    )
    .bind(message_id)
    .fetch_optional(pool)
    .await
    .context("loading seed message")?;
    let Some((external_id, subject, author, body)) = row else {
        return Ok(None);
    };

    let mut s = "This conversation is about a message".to_string();
    if let Some(a) = author {
        s.push_str(&format!(" from {a}"));
    }
    if let Some(subj) = subject.filter(|s| !s.trim().is_empty()) {
        s.push_str(&format!(", subject \"{subj}\""));
    }
    s.push_str(&format!(
        " (external_id {external_id}): {}. Call fetch_messages([\"{external_id}\"]) for the full body.",
        snippet(&body, 200)
    ));
    Ok(Some(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_role_user_and_seed() {
        let p = render(&RenderInputs {
            now_iso: "2026-05-30T12:00:00+00:00".to_string(),
            user_display_name: "Gustavo".to_string(),
            custom_prompt: Some("Ana is my direct report.".to_string()),
            seed: Some("This conversation is about action A-42.".to_string()),
        });
        assert!(p.contains("mnemis's assistant"));
        assert!(p.contains("high confidence so the change applies immediately"));
        assert!(p.contains("Display name: Gustavo"));
        assert!(p.contains("Ana is my direct report."));
        assert!(p.contains("2026-05-30T12:00:00+00:00"));
        assert!(p.contains("# This conversation"));
        assert!(p.contains("A-42"));
    }

    #[test]
    fn render_omits_empty_custom_prompt_and_seed() {
        let p = render(&RenderInputs {
            now_iso: "2026-05-30T12:00:00+00:00".to_string(),
            user_display_name: "Gustavo".to_string(),
            custom_prompt: Some("   ".to_string()),
            seed: None,
        });
        assert!(!p.contains("What they told you"));
        assert!(!p.contains("# This conversation"));
    }

    #[tokio::test]
    async fn seed_label_describes_a_seeded_action_and_skips_blank() {
        use crate::chat::store::create_chat;
        use crate::db;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let pool = db::open(&tmp.path().join("t.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        crate::test_util::seed_minimal(&pool).await.unwrap();

        let now = chrono::Utc::now().timestamp();
        let (aid,): (i64,) = sqlx::query_as(
            "INSERT INTO actions (title, details, confidence, rationale, status, extracted_at) \
             VALUES ('Renew the cert', 'x', 'high', 'r', 'auto_claimed', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(&pool)
        .await
        .unwrap();

        let chat = create_chat(&pool, Some("action"), Some(aid)).await.unwrap();
        let label = seed_label(&pool, chat).await.unwrap().unwrap();
        assert!(label.contains(&format!("action A-{aid}")), "got: {label}");
        assert!(label.contains("Renew the cert"), "got: {label}");

        // A blank chat has no seed label.
        let blank = create_chat(&pool, None, None).await.unwrap();
        assert!(seed_label(&pool, blank).await.unwrap().is_none());
    }
}
