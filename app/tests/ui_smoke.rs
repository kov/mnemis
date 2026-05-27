//! End-to-end smoke test that spawns the real `mnemis-app` binary via
//! `tauri-driver`, points it at a seeded temp database, and asserts on
//! what the user would see in the window.
//!
//! Gated behind the `ui-probe` feature so the heavy WebDriver deps aren't
//! pulled into default builds. Run with:
//!
//!   cargo build -p mnemis-app                            # binary the test drives
//!   cargo test  -p mnemis-app --features ui-probe \
//!       --test ui_smoke -- --nocapture
//!
//! Add `MNEMIS_TEST_HEADLESS=1` to run against a private weston compositor
//! (no display required). On a developer desktop this is optional; on CI
//! it's mandatory.
//!
//! Assertion style follows the project testing tenets: outcomes the user
//! would see (title visible, badge present), never DOM structure that's an
//! implementation detail.

#![cfg(feature = "ui-probe")]

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use fantoccini::Locator;
use mnemis_app::test_support::{Harness, HarnessOpts, sibling_app_binary};
use serde_json::Value;
use sqlx::SqlitePool;
use tempfile::TempDir;

const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "current_thread")]
async fn loads_actions_into_visible_list() -> Result<()> {
    // 1. Seed a self-contained DB.
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke.db");
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        seed(&pool).await?;

        // Pre-flight: confirm the same query the app will run sees our data.
        // If this fails the bug is in seed; if this passes but the app shows
        // empty, the bug is env-forwarding.
        let rows = mnemis_engine::queries::list_actions(
            &pool,
            mnemis_engine::queries::ActionFilter::default(),
        )
        .await?;
        assert_eq!(
            rows.len(),
            2,
            "pre-flight: expected 2 actions in seeded DB, got {}",
            rows.len()
        );
        eprintln!(
            "[ui_smoke] pre-flight ok: {} actions visible via engine",
            rows.len()
        );

        pool.close().await;
    }
    eprintln!("[ui_smoke] db path forwarded to app: {}", db_path.display());

    let app_bin = sibling_app_binary()
        .context("mnemis-app binary missing; run `cargo build -p mnemis-app` before the test")?;

    // 2. Bring up the harness, exporting the DB-path env BEFORE the driver
    //    is forked so it inherits to all descendants.
    let env = HashMap::from([("MNEMIS_DB_PATH".to_string(), db_path.display().to_string())]);
    let harness = Harness::start(HarnessOpts::default(), env).await?;

    // 3. Open a session pointing at the seeded DB.
    let client = harness.open_session(&app_bin).await?;

    // 4. Wait for the app shell, give Suspense a beat.
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.app"))
        .await
        .context("waiting for div.app")?;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 5. Dump what the user sees.
    let body_text = client
        .find(Locator::Css("body"))
        .await?
        .text()
        .await
        .unwrap_or_default()
        .to_lowercase();
    let body_html = client
        .find(Locator::Css("body"))
        .await?
        .html(true)
        .await
        .unwrap_or_default();

    // 6. Assertions (outcomes the user can see; loose contains-checks).
    assert!(
        body_text.contains("review q3 roadmap"),
        "expected the high-confidence action title in the visible text. \
         got text: {body_text}\n----html----\n{body_html}"
    );
    assert!(
        body_html.contains("badge-high"),
        "expected a high-confidence badge in the DOM. html: {body_html}"
    );
    assert!(
        !body_text.contains("background reading"),
        "the low-confidence action should be hidden behind the revealer. \
         got text: {body_text}"
    );
    assert!(
        body_text.contains("low-confidence"),
        "expected the low-confidence revealer to be visible. got text: {body_text}"
    );

    // 7. Navigate to the inbox via the nav link. We click programmatically
    //    rather than using fantoccini's click (which goes through the W3C
    //    perform-actions endpoint and was rejecting clicks on the SPA link
    //    with no useful diagnostic).
    let nav_result = client
        .execute(
            r#"
            const link = document.querySelector('a[href="/inbox"]');
            if (!link) { return 'missing-link'; }
            link.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(
        nav_result.as_str(),
        Some("ok"),
        "inbox nav script could not find the link: {nav_result:?}"
    );
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.message"))
        .await
        .context("waiting for inbox to render")?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let inbox_text = client
        .find(Locator::Css("body"))
        .await?
        .text()
        .await
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        inbox_text.contains("q3 roadmap draft"),
        "expected the high-action message subject in the inbox. text: {inbox_text}"
    );
    assert!(
        inbox_text.contains("background reading"),
        "expected the low-action message subject in the inbox. text: {inbox_text}"
    );
    assert!(
        inbox_text.contains("fyi no action"),
        "expected the action-less message subject in the inbox. text: {inbox_text}"
    );
    // Exactly two messages are linked to actions in the seed; the third
    // should not carry an "action" badge. Count the badge spans directly
    // — text-based counting trips on "no action needed" in the subject.
    let badge_count_raw = client
        .execute(
            r#"
            return String(Array.from(document.querySelectorAll('div.message span.badge'))
                .filter(s => s.textContent.trim() === 'action').length);
            "#,
            vec![],
        )
        .await?;
    let badge_count: usize = badge_count_raw.as_str().unwrap_or("0").parse().unwrap_or(0);
    assert_eq!(
        badge_count, 2,
        "expected exactly 2 'action' badges in the inbox (one per action-linked message), \
         got {badge_count}. text: {inbox_text}"
    );

    // 8. No console errors / window errors.
    let log_raw = client
        .execute("return JSON.stringify(window.__mnemis_log || []);", vec![])
        .await?;
    if let Some(s) = log_raw.as_str()
        && let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(s)
    {
        let bad: Vec<_> = arr
            .iter()
            .filter(|e| {
                matches!(
                    e["kind"].as_str(),
                    Some("error") | Some("window.error") | Some("unhandled")
                )
            })
            .collect();
        assert!(
            bad.is_empty(),
            "console / window errors captured: {bad:?}\nfull log: {arr:?}"
        );
    }

    client.close().await.ok();
    Ok(())
}

/// Seed exactly two actions linked to messages: one high-confidence (visible
/// up front) and one low-confidence (hidden behind the revealer). Matches
/// the `record_action` insert shape so the same `queries::list_actions` SQL
/// returns them.
async fn seed(pool: &SqlitePool) -> Result<()> {
    let now = Utc::now().timestamp();

    // Singleton user_profile + self-contact.
    sqlx::query(
        "INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, 'Smoke Tester', ?)",
    )
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO contacts (display_name, relationship, created_at, updated_at) \
         VALUES ('Smoke Tester', 'self', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO contact_identifiers (contact_id, kind, value) \
         SELECT id, 'email', 'tester@example.com' FROM contacts WHERE relationship = 'self'",
    )
    .execute(pool)
    .await?;

    // One source, one channel.
    let (source_id,): (i64,) = sqlx::query_as(
        "INSERT INTO sources (kind, name, config_ref, created_at) \
         VALUES ('imap', 'work', 'kc/work', ?) RETURNING id",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;
    let (channel_id,): (i64,) = sqlx::query_as(
        "INSERT INTO channels (source_id, external_id, name, kind) \
         VALUES (?, 'INBOX', 'INBOX', 'mailbox') RETURNING id",
    )
    .bind(source_id)
    .fetch_one(pool)
    .await?;

    // Author + two messages.
    let (author_id,): (i64,) = sqlx::query_as(
        "INSERT INTO people (source_id, external_id, display_name) \
         VALUES (?, 'ana@example.com', 'Ana') RETURNING id",
    )
    .bind(source_id)
    .fetch_one(pool)
    .await?;

    let (high_msg_id,): (i64,) = sqlx::query_as(
        "INSERT INTO messages \
            (channel_id, external_id, author_id, posted_at, subject, body, body_format, ingested_at, flags) \
         VALUES (?, 'msg-high', ?, ?, 'Q3 roadmap draft', 'Can you take a pass by EOD?', 'text', ?, 0) \
         RETURNING id",
    )
    .bind(channel_id)
    .bind(author_id)
    .bind(now - 600)
    .bind(now - 600)
    .fetch_one(pool)
    .await?;
    let (low_msg_id,): (i64,) = sqlx::query_as(
        "INSERT INTO messages \
            (channel_id, external_id, author_id, posted_at, subject, body, body_format, ingested_at, flags) \
         VALUES (?, 'msg-low', ?, ?, 'Background reading', 'Sharing this for context.', 'text', ?, 0) \
         RETURNING id",
    )
    .bind(channel_id)
    .bind(author_id)
    .bind(now - 300)
    .bind(now - 300)
    .fetch_one(pool)
    .await?;

    // Third message with no linked action; lets the inbox test confirm the
    // 'action' badge is per-message rather than always-on.
    sqlx::query(
        "INSERT INTO messages \
            (channel_id, external_id, author_id, posted_at, subject, body, body_format, ingested_at, flags) \
         VALUES (?, 'msg-fyi', ?, ?, 'FYI no action needed', 'Just sharing.', 'text', ?, 0)",
    )
    .bind(channel_id)
    .bind(author_id)
    .bind(now - 60)
    .bind(now - 60)
    .execute(pool)
    .await?;

    // Two actions: one high (auto-claimed, shown immediately), one low
    // (pending, hidden behind the revealer).
    let (high_action_id,): (i64,) = sqlx::query_as(
        "INSERT INTO actions \
            (title, details, confidence, rationale, status, extracted_at) \
         VALUES ('Review Q3 roadmap', 'Ana asked for a pass on the draft.', 'high', \
                 'Explicit ask', 'auto_claimed', ?) RETURNING id",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;
    let (low_action_id,): (i64,) = sqlx::query_as(
        "INSERT INTO actions \
            (title, details, confidence, rationale, status, extracted_at) \
         VALUES ('Background reading on perf wins', 'Optional context.', 'low', \
                 'Soft suggestion', 'pending', ?) RETURNING id",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;

    for (aid, mid) in [(high_action_id, high_msg_id), (low_action_id, low_msg_id)] {
        sqlx::query(
            "INSERT INTO action_evidence (action_id, message_id, kind, is_primary) \
             VALUES (?, ?, 'source', 1)",
        )
        .bind(aid)
        .bind(mid)
        .execute(pool)
        .await?;
    }

    Ok(())
}
