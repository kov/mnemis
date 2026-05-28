//! Read/write helpers backing the Settings UI: user profile, source list,
//! per-source mute, source delete. LLM config lives in `config::*` since it
//! sits in `config.toml` rather than the database.

use anyhow::{Context, Result};
use chrono::Utc;
use mnemis_types::{ProfileIdentifier, SourceHealth, SourceRowDto, UserProfileDto};
use sqlx::SqlitePool;

use crate::secrets;

/// Load the singleton user profile. Returns defaults (empty name, no
/// identifiers) when nothing has been saved yet — the first-run flow uses
/// this to render an empty form rather than erroring.
pub async fn get_user_profile(pool: &SqlitePool) -> Result<UserProfileDto> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT display_name, custom_prompt FROM user_profile WHERE id = 1")
            .fetch_optional(pool)
            .await
            .context("loading user_profile")?;
    let (display_name, custom_prompt) = row.unwrap_or_default();

    let identifiers: Vec<(String, String)> = sqlx::query_as(
        "SELECT ci.kind, ci.value FROM contact_identifiers ci \
         JOIN contacts c ON c.id = ci.contact_id \
         WHERE c.relationship = 'self' ORDER BY ci.kind, ci.value",
    )
    .fetch_all(pool)
    .await
    .context("loading self-identifiers")?;

    Ok(UserProfileDto {
        display_name,
        custom_prompt,
        identifiers: identifiers
            .into_iter()
            .map(|(kind, value)| ProfileIdentifier { kind, value })
            .collect(),
    })
}

/// Replace the user profile (upserts the singleton row) and reconciles the
/// self-contact's identifiers against the desired set. Identifiers are
/// matched by `(kind, value)`; rows not in the desired set are deleted, new
/// ones are inserted. The `display_name` is mirrored onto the self-contact
/// so prompt rendering reflects the latest name.
pub async fn save_user_profile(pool: &SqlitePool, profile: &UserProfileDto) -> Result<()> {
    let now = Utc::now().timestamp();
    let mut tx = pool.begin().await?;

    sqlx::query(
        "INSERT INTO user_profile (id, display_name, custom_prompt, updated_at) \
         VALUES (1, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
            display_name = excluded.display_name, \
            custom_prompt = excluded.custom_prompt, \
            updated_at = excluded.updated_at",
    )
    .bind(&profile.display_name)
    .bind(profile.custom_prompt.as_deref())
    .bind(now)
    .execute(&mut *tx)
    .await
    .context("upserting user_profile")?;

    // Make sure a self-contact exists so the identifier rows have somewhere
    // to attach (and so the prompt's "who you work for" lookup finds it).
    let self_id: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM contacts WHERE relationship = 'self' LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?;
    let self_id = match self_id {
        Some((id,)) => {
            sqlx::query("UPDATE contacts SET display_name = ?, updated_at = ? WHERE id = ?")
                .bind(&profile.display_name)
                .bind(now)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            id
        }
        None => {
            let (id,): (i64,) = sqlx::query_as(
                "INSERT INTO contacts (display_name, relationship, created_at, updated_at) \
                 VALUES (?, 'self', ?, ?) RETURNING id",
            )
            .bind(&profile.display_name)
            .bind(now)
            .bind(now)
            .fetch_one(&mut *tx)
            .await?;
            id
        }
    };

    // Reconcile identifiers: drop ones no longer present, insert new ones.
    // Keying on (kind, value) since that's also the UNIQUE constraint.
    let existing: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, value FROM contact_identifiers WHERE contact_id = ?")
            .bind(self_id)
            .fetch_all(&mut *tx)
            .await?;
    let desired: std::collections::HashSet<(String, String)> = profile
        .identifiers
        .iter()
        .map(|i| (i.kind.clone(), i.value.clone()))
        .collect();
    for (kind, value) in &existing {
        if !desired.contains(&(kind.clone(), value.clone())) {
            sqlx::query(
                "DELETE FROM contact_identifiers \
                 WHERE contact_id = ? AND kind = ? AND value = ?",
            )
            .bind(self_id)
            .bind(kind)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
    }
    let existing_set: std::collections::HashSet<(String, String)> = existing.into_iter().collect();
    for ident in &profile.identifiers {
        if !existing_set.contains(&(ident.kind.clone(), ident.value.clone())) {
            sqlx::query(
                "INSERT INTO contact_identifiers (contact_id, kind, value) \
                 VALUES (?, ?, ?) ON CONFLICT(kind, value) DO NOTHING",
            )
            .bind(self_id)
            .bind(&ident.kind)
            .bind(&ident.value)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}

/// True when nothing has been set up yet — no user_profile row. The first-run
/// banner reads this to know whether to show itself.
pub async fn is_first_run(pool: &SqlitePool) -> Result<bool> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_profile")
        .fetch_one(pool)
        .await
        .context("counting user_profile rows")?;
    Ok(count == 0)
}

/// All configured sources, in display order, with mute + health columns.
#[allow(clippy::type_complexity)]
pub async fn list_sources(pool: &SqlitePool) -> Result<Vec<SourceRowDto>> {
    let rows: Vec<(
        i64,
        String,
        String,
        i64,
        String,
        Option<i64>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT s.id, s.name, s.kind, \
                    COALESCE((SELECT MAX(c.muted) FROM channels c WHERE c.source_id = s.id), 0), \
                    s.status, s.last_synced_at, s.last_error \
             FROM sources s ORDER BY s.id",
    )
    .fetch_all(pool)
    .await
    .context("listing sources for settings")?;
    Ok(rows
        .into_iter()
        .map(
            |(id, name, kind, muted, status, last_synced_at, last_error)| SourceRowDto {
                id,
                name,
                kind,
                muted: muted != 0,
                health: SourceHealth::parse(&status).unwrap_or(SourceHealth::Warning),
                last_synced_at,
                last_error,
            },
        )
        .collect())
}

/// Toggle mute for all channels on a source. Per-channel mute already exists
/// in the schema (`channels.muted`); the settings UI exposes it at the
/// source granularity for simplicity. A muted channel is skipped by the
/// polling loop, so the next sync silently drops it.
pub async fn set_source_muted(pool: &SqlitePool, source_id: i64, muted: bool) -> Result<()> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM sources WHERE id = ?")
        .bind(source_id)
        .fetch_optional(pool)
        .await?;
    if row.is_none() {
        anyhow::bail!("source {source_id} not found");
    }
    sqlx::query("UPDATE channels SET muted = ? WHERE source_id = ?")
        .bind(if muted { 1 } else { 0 })
        .bind(source_id)
        .execute(pool)
        .await
        .context("updating channel mute")?;
    Ok(())
}

/// Remove a source and everything tied to it (channels, messages,
/// extraction runs, embed queue entries). Actions referencing the source's
/// messages stay — the audit log shouldn't lose history just because the
/// source was removed; `action_evidence` rows will dangle and the UI
/// gracefully handles "(unknown source)" for those.
pub async fn delete_source(pool: &SqlitePool, source_id: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    // Order matters: child rows first.
    sqlx::query(
        "DELETE FROM extraction_runs WHERE channel_id IN (SELECT id FROM channels WHERE source_id = ?)",
    )
    .bind(source_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM embed_queue WHERE target_kind = 'message' \
         AND target_id IN (SELECT m.id FROM messages m \
                           JOIN channels c ON c.id = m.channel_id \
                           WHERE c.source_id = ?)",
    )
    .bind(source_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM messages WHERE channel_id IN (SELECT id FROM channels WHERE source_id = ?)",
    )
    .bind(source_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM channels WHERE source_id = ?")
        .bind(source_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM people WHERE source_id = ?")
        .bind(source_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("source/{source_id}/%"))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sources WHERE id = ?")
        .bind(source_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Register a new IMAP source: stash the password in the OS keychain,
/// insert the `sources` row pointing at that keychain ref, and persist the
/// IMAP server/port/username in `settings` under `source/{id}/imap`.
/// **Does not** discover mailboxes — that needs a network round-trip and is
/// left to the next `sync_now`. Returns the new source id.
pub async fn add_imap_source(
    pool: &SqlitePool,
    name: &str,
    server: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<i64> {
    let keychain_ref = format!("imap/{username}@{server}");
    secrets::store(&keychain_ref, password)
        .await
        .context("storing IMAP password in keychain")?;

    let now = Utc::now().timestamp();
    let (source_id,): (i64,) = sqlx::query_as(
        "INSERT INTO sources (kind, name, config_ref, created_at) \
         VALUES ('imap', ?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(&keychain_ref)
    .bind(now)
    .fetch_one(pool)
    .await
    .context("inserting sources row")?;

    let conn_json = serde_json::json!({
        "server": server,
        "port": port,
        "username": username,
    })
    .to_string();
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?)")
        .bind(format!("source/{source_id}/imap"))
        .bind(&conn_json)
        .execute(pool)
        .await
        .context("storing IMAP connection settings")?;

    // Seed the username as a self-contact email identifier if a self-contact
    // exists. Best-effort — failures here shouldn't block source creation.
    let _ = sqlx::query(
        "INSERT INTO contact_identifiers (contact_id, kind, value) \
         SELECT id, 'email', ? FROM contacts WHERE relationship = 'self' LIMIT 1 \
         ON CONFLICT(kind, value) DO NOTHING",
    )
    .bind(username)
    .execute(pool)
    .await;

    Ok(source_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use tempfile::TempDir;

    async fn open() -> Result<(TempDir, SqlitePool)> {
        let tmp = TempDir::new()?;
        let pool = db::open(&tmp.path().join("t.db")).await?;
        db::migrate(&pool).await?;
        Ok((tmp, pool))
    }

    #[tokio::test]
    async fn save_then_get_round_trips_profile_and_identifiers() -> Result<()> {
        let (_tmp, pool) = open().await?;
        let p = UserProfileDto {
            display_name: "Gustavo".into(),
            custom_prompt: Some("Ana is my direct report.".into()),
            identifiers: vec![
                ProfileIdentifier {
                    kind: "email".into(),
                    value: "g@x.com".into(),
                },
                ProfileIdentifier {
                    kind: "mattermost_handle".into(),
                    value: "gustavo".into(),
                },
            ],
        };
        save_user_profile(&pool, &p).await?;

        let got = get_user_profile(&pool).await?;
        assert_eq!(got.display_name, "Gustavo");
        assert_eq!(
            got.custom_prompt.as_deref(),
            Some("Ana is my direct report.")
        );
        assert_eq!(got.identifiers.len(), 2);
        assert!(
            got.identifiers
                .iter()
                .any(|i| i.kind == "email" && i.value == "g@x.com")
        );
        Ok(())
    }

    #[tokio::test]
    async fn saving_again_reconciles_identifiers() -> Result<()> {
        // Pin the reconciliation rule: a save with a smaller identifier set
        // drops the ones no longer present; a save with new ones adds them.
        // Without this, edits would accumulate stale rows forever.
        let (_tmp, pool) = open().await?;
        save_user_profile(
            &pool,
            &UserProfileDto {
                display_name: "G".into(),
                custom_prompt: None,
                identifiers: vec![
                    ProfileIdentifier {
                        kind: "email".into(),
                        value: "a@x".into(),
                    },
                    ProfileIdentifier {
                        kind: "email".into(),
                        value: "b@x".into(),
                    },
                ],
            },
        )
        .await?;
        save_user_profile(
            &pool,
            &UserProfileDto {
                display_name: "G".into(),
                custom_prompt: None,
                identifiers: vec![
                    ProfileIdentifier {
                        kind: "email".into(),
                        value: "b@x".into(),
                    },
                    ProfileIdentifier {
                        kind: "email".into(),
                        value: "c@x".into(),
                    },
                ],
            },
        )
        .await?;
        let got = get_user_profile(&pool).await?;
        let values: Vec<&str> = got.identifiers.iter().map(|i| i.value.as_str()).collect();
        assert!(
            !values.contains(&"a@x"),
            "stale identifier should have been dropped"
        );
        assert!(values.contains(&"b@x"));
        assert!(values.contains(&"c@x"));
        Ok(())
    }

    #[tokio::test]
    async fn is_first_run_flips_after_first_save() -> Result<()> {
        let (_tmp, pool) = open().await?;
        assert!(is_first_run(&pool).await?);
        save_user_profile(
            &pool,
            &UserProfileDto {
                display_name: "G".into(),
                custom_prompt: None,
                identifiers: vec![],
            },
        )
        .await?;
        assert!(!is_first_run(&pool).await?);
        Ok(())
    }

    #[tokio::test]
    async fn set_source_muted_and_delete_source() -> Result<()> {
        let (_tmp, pool) = open().await?;
        let now = Utc::now().timestamp();
        let (sid,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'work', 'kc/work', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             VALUES (?, 'INBOX', 'INBOX', 'mailbox')",
        )
        .bind(sid)
        .execute(&pool)
        .await?;

        set_source_muted(&pool, sid, true).await?;
        let rows = list_sources(&pool).await?;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].muted);

        set_source_muted(&pool, sid, false).await?;
        let rows = list_sources(&pool).await?;
        assert!(!rows[0].muted);

        delete_source(&pool, sid).await?;
        let rows = list_sources(&pool).await?;
        assert!(rows.is_empty());
        Ok(())
    }
}
