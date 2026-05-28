use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use mnemis_engine::config::Config;
use mnemis_engine::db;
use mnemis_engine::embed::{Embedder, OmlxEmbedder, drain_once};
use mnemis_engine::extract::{extract_for_channel, prompt};
use mnemis_engine::llm::LlmClient;
use mnemis_engine::maintenance;
use mnemis_engine::orchestrator::{self, build_imap_source};
use mnemis_engine::secrets;
use mnemis_engine::source::Source;
use mnemis_engine::source::SourceId;
use std::sync::Arc;

pub async fn init(cfg: &Config, display_name: Option<String>) -> Result<()> {
    let path = cfg.db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let pool = db::open(&path).await?;
    db::migrate(&pool).await?;

    if let Some(name) = display_name {
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET display_name = excluded.display_name, \
             updated_at = excluded.updated_at",
        )
        .bind(&name)
        .bind(now)
        .execute(&pool)
        .await?;

        // Also seed a self-contact so the extractor can pull identifiers later.
        sqlx::query(
            "INSERT INTO contacts (display_name, relationship, created_at, updated_at) \
             VALUES (?, 'self', ?, ?) \
             ON CONFLICT DO NOTHING",
        )
        .bind(&name)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .ok();
    }

    println!("Database initialized at {}", path.display());
    Ok(())
}

pub async fn add_source_imap(
    cfg: &Config,
    name: &str,
    server: &str,
    port: u16,
    username: &str,
) -> Result<()> {
    let pool = db::open(&cfg.db_path()).await?;

    let password = rpassword::prompt_password(format!("IMAP password for {username}@{server}: "))
        .context("reading password from stdin")?;

    let keychain_ref = format!("imap/{username}@{server}");
    secrets::store(&keychain_ref, &password).await?;

    let now = Utc::now().timestamp();
    let (source_id,): (i64,) = sqlx::query_as(
        "INSERT INTO sources (kind, name, config_ref, created_at) \
         VALUES ('imap', ?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(&keychain_ref)
    .bind(now)
    .fetch_one(&pool)
    .await?;

    // Also record the IMAP server + port + username as JSON in settings keyed by source.
    let conn_json = serde_json::json!({
        "server": server,
        "port": port,
        "username": username,
    })
    .to_string();
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?)")
        .bind(format!("source/{source_id}/imap"))
        .bind(&conn_json)
        .execute(&pool)
        .await?;

    // Also seed user's email as a self-contact identifier (if a self-contact exists).
    sqlx::query(
        "INSERT INTO contact_identifiers (contact_id, kind, value) \
         SELECT id, 'email', ? FROM contacts WHERE relationship = 'self' LIMIT 1 \
         ON CONFLICT(kind, value) DO NOTHING",
    )
    .bind(username)
    .execute(&pool)
    .await
    .ok();

    // Discover channels.
    let source = build_imap_source(&pool, SourceId(source_id)).await?;
    let channels = source.list_channels().await.context("listing mailboxes")?;
    let mut imported = 0;
    for ch in &channels {
        sqlx::query(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(source_id)
        .bind(&ch.external_id)
        .bind(&ch.name)
        .bind(&ch.kind)
        .execute(&pool)
        .await?;
        imported += 1;
    }

    println!("Source '{name}' added (id={source_id}); discovered {imported} mailbox(es).");
    Ok(())
}

pub async fn sync(cfg: &Config) -> Result<()> {
    let pool = db::open(&cfg.db_path()).await?;
    let llm = build_llm(cfg);
    let embedder: Arc<dyn Embedder> = Arc::new(build_embedder(cfg));

    let count_sources: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sources")
        .fetch_one(&pool)
        .await?;
    if count_sources.0 == 0 {
        println!("No sources configured. Add one with `mnemis add-source imap ...`.");
        return Ok(());
    }

    let traces = cfg.traces_dir();
    let outcome =
        orchestrator::sync_now(&pool, &llm, embedder, &cfg.llm.chat_model, Some(&traces)).await?;

    println!(
        "Synced {} source(s) ({} failed), {} channel(s) polled, {} new message(s), \
         {} embedding(s) drained, {} action(s) created.",
        outcome.sources_synced,
        outcome.sources_failed,
        outcome.channels_polled,
        outcome.messages_ingested,
        outcome.embeddings_drained,
        outcome.actions_created,
    );
    for e in &outcome.errors {
        eprintln!("  ! {e}");
    }
    Ok(())
}

pub async fn list_actions(cfg: &Config, status_filter: Option<&str>, json: bool) -> Result<()> {
    let pool = db::open(&cfg.db_path()).await?;
    let statuses: Vec<String> = match status_filter {
        Some(s) => s.split(',').map(|s| s.trim().to_string()).collect(),
        None => vec!["pending".into(), "auto_claimed".into(), "claimed".into()],
    };
    let placeholders = vec!["?"; statuses.len()].join(",");
    let sql = sqlx::AssertSqlSafe(format!(
        "SELECT id, title, details, confidence, status, due_at, extracted_at \
         FROM actions WHERE status IN ({placeholders}) \
         ORDER BY extracted_at DESC"
    ));
    #[allow(clippy::type_complexity)]
    let mut q = sqlx::query_as::<
        _,
        (
            i64,
            String,
            Option<String>,
            String,
            String,
            Option<i64>,
            i64,
        ),
    >(sql);
    for s in &statuses {
        q = q.bind(s);
    }
    let rows = q.fetch_all(&pool).await?;

    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|(id, title, details, conf, status, due, extracted)| {
                serde_json::json!({
                    "id": id,
                    "title": title,
                    "details": details,
                    "confidence": conf,
                    "status": status,
                    "due_at": due.and_then(|t| DateTime::<Utc>::from_timestamp(t, 0).map(|d| d.to_rfc3339())),
                    "extracted_at": DateTime::<Utc>::from_timestamp(*extracted, 0).map(|d| d.to_rfc3339()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if rows.is_empty() {
        println!("(no actions)");
    } else {
        for (id, title, details, conf, status, due, _extracted) in &rows {
            let due_str = due
                .and_then(|t| DateTime::<Utc>::from_timestamp(t, 0))
                .map(|d| format!(" due {}", d.format("%Y-%m-%d")))
                .unwrap_or_default();
            println!("[A-{id}] {} [{status}/{conf}]{due_str}", title);
            if let Some(d) = details
                && !d.is_empty()
            {
                println!("       {d}");
            }
        }
    }
    Ok(())
}

pub async fn extract(cfg: &Config, channel_id: i64) -> Result<()> {
    let pool = db::open(&cfg.db_path()).await?;
    let llm = build_llm(cfg);
    let traces = cfg.traces_dir();
    let outcome =
        extract_for_channel(&pool, &llm, channel_id, &cfg.llm.chat_model, Some(&traces)).await?;
    println!(
        "result={} actions_created={} up_to_message_id={:?}",
        outcome.result, outcome.actions_created, outcome.up_to_message_id
    );
    if let Some(s) = outcome.summary {
        println!("summary: {s}");
    }
    println!("trace dir: {}", traces.display());
    Ok(())
}

pub async fn embed_once(cfg: &Config) -> Result<()> {
    let pool = db::open(&cfg.db_path()).await?;
    let embedder = build_embedder(cfg);
    let processed = drain_once(&pool, &embedder).await?;
    println!("Embedded {processed} target(s).");
    Ok(())
}

fn build_llm(cfg: &Config) -> LlmClient {
    let llm = LlmClient::new(cfg.llm.base_url.clone(), cfg.llm.chat_model.clone());
    match &cfg.llm.bearer_token {
        Some(t) => llm.with_bearer_token(t.clone()),
        None => llm,
    }
}

fn build_embedder(cfg: &Config) -> OmlxEmbedder {
    let mut e = OmlxEmbedder::new(cfg.llm.base_url.clone(), cfg.llm.embedding_model.clone());
    if let Some(t) = &cfg.llm.bearer_token {
        e = e.with_bearer_token(t.clone());
    }
    e
}

pub async fn dump_prompt(cfg: &Config, channel_id: i64) -> Result<()> {
    // Build the prompt by calling extract::prompt::build directly with data loaded
    // the same way extract_for_channel would. Cheap to duplicate the load logic
    // here for now; if we add more dump-like commands we can factor it out.
    let pool = db::open(&cfg.db_path()).await?;

    let (source_id, source_kind, channel_name): (i64, String, String) = sqlx::query_as(
        "SELECT c.source_id, s.kind, c.name FROM channels c \
         JOIN sources s ON s.id = c.source_id WHERE c.id = ?",
    )
    .bind(channel_id)
    .fetch_one(&pool)
    .await
    .context("loading channel")?;

    let watermark: Option<i64> = sqlx::query_as::<_, (Option<i64>,)>(
        "SELECT up_to_message_id FROM extraction_runs \
         WHERE channel_id = ? AND result IN ('ok', 'no_activity') \
         ORDER BY ran_at DESC LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(&pool)
    .await?
    .and_then(|(o,)| o);

    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, i64, Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT m.external_id, m.posted_at, m.subject, m.body, p.display_name \
         FROM messages m LEFT JOIN people p ON p.id = m.author_id \
         WHERE m.channel_id = ? AND m.id > ? ORDER BY m.id ASC LIMIT 100",
    )
    .bind(channel_id)
    .bind(watermark.unwrap_or(0))
    .fetch_all(&pool)
    .await?;

    let window: Vec<prompt::WindowMessage> = rows
        .into_iter()
        .map(
            |(external_id, posted_at, subject, body, author)| prompt::WindowMessage {
                external_id,
                posted_at: DateTime::<Utc>::from_timestamp(posted_at, 0).unwrap_or_else(Utc::now),
                author: author.unwrap_or_else(|| "?".to_string()),
                subject,
                body,
            },
        )
        .collect();

    let profile_row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT display_name, custom_prompt FROM user_profile WHERE id = 1")
            .fetch_optional(&pool)
            .await?;
    let (display, custom) = profile_row.unwrap_or(("(unknown)".to_string(), None));

    let identifier_kinds: &[&str] = match source_kind.as_str() {
        "imap" => &["email"],
        "mattermost" => &["mattermost_handle", "email"],
        _ => &[],
    };
    let mut identifiers = Vec::new();
    for k in identifier_kinds {
        let r: Vec<(String,)> = sqlx::query_as(
            "SELECT ci.value FROM contact_identifiers ci JOIN contacts c ON c.id = ci.contact_id \
             WHERE c.relationship = 'self' AND ci.kind = ?",
        )
        .bind(*k)
        .fetch_all(&pool)
        .await?;
        identifiers.extend(r.into_iter().map(|(v,)| v));
    }

    let existing: Vec<(i64, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT DISTINCT a.id, a.title, a.details, a.due_at FROM actions a \
         JOIN action_evidence ae ON ae.action_id = a.id \
         JOIN messages m ON m.id = ae.message_id \
         WHERE m.channel_id = ? AND a.status IN ('pending', 'auto_claimed', 'claimed') \
         ORDER BY a.extracted_at DESC LIMIT 50",
    )
    .bind(channel_id)
    .fetch_all(&pool)
    .await?;
    let existing: Vec<prompt::ExistingAction> = existing
        .into_iter()
        .map(|(id, title, details, due_at)| prompt::ExistingAction {
            id,
            title,
            details,
            due_at: due_at.and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0)),
        })
        .collect();

    // Match what extract_for_channel feeds the model — the dump is only
    // useful as a regression aid if the two prompts stay in sync.
    let feedback = mnemis_engine::extract::load_feedback_for(&pool, source_id, channel_id).await?;
    let inputs = prompt::PromptInputs {
        source_kind: &source_kind,
        channel_name: &channel_name,
        user_display_name: &display,
        user_identifiers: &identifiers,
        custom_prompt: custom.as_deref(),
        current_time: Utc::now(),
        existing_actions: &existing,
        feedback: &feedback,
        window: &window,
    };
    println!("{}", prompt::build(&inputs));
    Ok(())
}

pub async fn reset_data(cfg: &Config, yes: bool) -> Result<()> {
    let path = cfg.db_path();
    let pool = db::open(&path).await?;
    db::migrate(&pool).await?;

    println!("DB: {}", path.display());
    let before = maintenance::count_user_data(&pool).await?;
    println!("== BEFORE ==");
    for (t, n) in &before {
        println!("  {t:<22} {n}");
    }

    if !yes {
        println!();
        println!("(dry run) re-run with --yes to actually clear.");
        return Ok(());
    }

    println!();
    println!("Clearing...");
    let counts = maintenance::reset_data(&pool).await?;
    println!("== AFTER ==");
    for (t, n) in &counts.after {
        println!("  {t:<22} {n}");
    }
    println!();
    println!("done.");
    Ok(())
}
