use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use mnemis_engine::db;
use mnemis_engine::embed::{Embedder, OmlxEmbedder, drain_once};
use mnemis_engine::extract::{extract_for_channel, prompt};
use mnemis_engine::ingest::ingest_batch;
use mnemis_engine::llm::LlmClient;
use mnemis_engine::source::imap::{ImapConfig, ImapSource};
use mnemis_engine::source::{Cursor, Source, SourceId};
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::config::Config;
use crate::secrets;

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
    let llm = LlmClient::new(cfg.llm.base_url.clone(), cfg.llm.chat_model.clone());
    let llm = match &cfg.llm.bearer_token {
        Some(t) => llm.with_bearer_token(t.clone()),
        None => llm,
    };
    let mut embedder = OmlxEmbedder::new(cfg.llm.base_url.clone(), cfg.llm.embedding_model.clone());
    if let Some(t) = &cfg.llm.bearer_token {
        embedder = embedder.with_bearer_token(t.clone());
    }
    let embedder: Arc<dyn Embedder> = Arc::new(embedder);

    let sources: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM sources WHERE status != 'disabled' ORDER BY id")
            .fetch_all(&pool)
            .await?;

    if sources.is_empty() {
        println!("No sources configured. Add one with `mnemis add-source imap ...`.");
        return Ok(());
    }

    for (source_id, source_name) in sources {
        println!("==> Source [{source_id}] {source_name}");
        let source = match build_imap_source(&pool, SourceId(source_id)).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  source build failed: {e:#}");
                mark_source_failure(&pool, source_id, &format!("{e:#}"))
                    .await
                    .ok();
                continue;
            }
        };

        let channels: Vec<(i64, String, Option<String>)> = sqlx::query_as(
            "SELECT id, external_id, cursor FROM channels WHERE source_id = ? AND muted = 0",
        )
        .bind(source_id)
        .fetch_all(&pool)
        .await?;

        for (channel_id, external_id, cursor) in channels {
            print!("  channel {external_id}: polling… ");
            let cursor = cursor.map(Cursor);
            let batch = match source.poll(&external_id, cursor.as_ref()).await {
                Ok(b) => b,
                Err(e) => {
                    println!("ERROR: {e:#}");
                    continue;
                }
            };
            let inserted = ingest_batch(&pool, SourceId(source_id), channel_id, &batch).await?;
            println!("{inserted} new messages.");

            if inserted > 0 {
                print!("    embedding new messages… ");
                let drained = drain_once(&pool, embedder.as_ref()).await?;
                println!("{drained} embedded.");

                print!("    extracting… ");
                let outcome =
                    extract_for_channel(&pool, &llm, channel_id, &cfg.llm.chat_model).await?;
                println!(
                    "{} ({} actions{})",
                    outcome.result,
                    outcome.actions_created,
                    outcome
                        .summary
                        .as_deref()
                        .map(|s| format!(", \"{}\"", truncate(s, 80)))
                        .unwrap_or_default()
                );
            }
        }

        mark_source_ok(&pool, source_id).await.ok();
    }

    // Final drain pass for anything (memory_notes, actions) enqueued during the run.
    let drained = drain_once(&pool, embedder.as_ref()).await?;
    if drained > 0 {
        println!("Final embed pass: {drained} more embedded.");
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

    let _ = source_id; // suppress unused warning until we use it elsewhere
    let inputs = prompt::PromptInputs {
        source_kind: &source_kind,
        channel_name: &channel_name,
        user_display_name: &display,
        user_identifiers: &identifiers,
        custom_prompt: custom.as_deref(),
        current_time: Utc::now(),
        existing_actions: &existing,
        window: &window,
    };
    println!("{}", prompt::build(&inputs));
    Ok(())
}

// --- helpers --------------------------------------------------------------

async fn build_imap_source(pool: &SqlitePool, source_id: SourceId) -> Result<ImapSource> {
    let (kind, config_ref): (String, String) =
        sqlx::query_as("SELECT kind, config_ref FROM sources WHERE id = ?")
            .bind(source_id.0)
            .fetch_one(pool)
            .await?;
    if kind != "imap" {
        anyhow::bail!("source {} is not IMAP (kind={kind})", source_id.0);
    }

    let conn_json: (String,) = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
        .bind(format!("source/{}/imap", source_id.0))
        .fetch_one(pool)
        .await
        .context("missing IMAP connection settings")?;
    let parsed: serde_json::Value = serde_json::from_str(&conn_json.0)?;
    let server = parsed["server"]
        .as_str()
        .context("missing server")?
        .to_string();
    let port = parsed["port"].as_u64().context("missing port")? as u16;
    let username = parsed["username"]
        .as_str()
        .context("missing username")?
        .to_string();

    let password = secrets::fetch(&config_ref)
        .await
        .with_context(|| format!("fetching keychain entry {config_ref}"))?;

    Ok(ImapSource::new(
        source_id,
        ImapConfig {
            server,
            port,
            username,
            password,
        },
    ))
}

async fn mark_source_ok(pool: &SqlitePool, source_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE sources SET status = 'ok', last_synced_at = ?, last_error = NULL, \
         consecutive_failures = 0 WHERE id = ?",
    )
    .bind(Utc::now().timestamp())
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_source_failure(pool: &SqlitePool, source_id: i64, error: &str) -> Result<()> {
    sqlx::query(
        "UPDATE sources SET status = CASE WHEN consecutive_failures + 1 >= 6 THEN 'failed' \
         ELSE 'warning' END, last_error = ?, consecutive_failures = consecutive_failures + 1 \
         WHERE id = ?",
    )
    .bind(error)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}
