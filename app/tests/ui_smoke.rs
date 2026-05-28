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
    //    is forked so it inherits to all descendants. MNEMIS_DISABLE_LLM
    //    keeps sync_now deterministic: it short-circuits with "No LLM
    //    configured" instead of trying to contact the dev machine's omlx
    //    + real IMAP.
    let env = HashMap::from([
        ("MNEMIS_DB_PATH".to_string(), db_path.display().to_string()),
        ("MNEMIS_DISABLE_LLM".to_string(), "1".to_string()),
    ]);
    let harness = Harness::start(HarnessOpts::default(), env).await?;

    // 3. Open a session pointing at the seeded DB.
    let client = harness.open_session(&app_bin).await?;

    // 4. Wait for the app shell + status panel, give Suspense a beat.
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.status-panel"))
        .await
        .context("waiting for div.status-panel")?;
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.app"))
        .await
        .context("waiting for div.app")?;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 4a. Status panel shows the seeded source + embed-queue depth (0 in seed).
    let status_text = client
        .find(Locator::Css("div.status-panel"))
        .await?
        .text()
        .await
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        status_text.contains("work"),
        "status panel should list the 'work' source. got: {status_text}"
    );
    assert!(
        status_text.contains("embed queue: 0"),
        "status panel should show an empty embed queue. got: {status_text}"
    );

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

    // 6a. Click "Done" on the visible high-confidence action; assert the
    //     list refreshes and the action is no longer rendered.
    let click_done = client
        .execute(
            r#"
            const btn = Array.from(document.querySelectorAll('.action-actions button'))
                .find(b => b.textContent.trim() === 'Done');
            if (!btn) { return 'missing-done'; }
            btn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(
        click_done.as_str(),
        Some("ok"),
        "Done button not found in DOM: {click_done:?}"
    );
    // Wait for the list to refresh (the action drops out of the default
    // filter once status=done).
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let t = client
            .find(Locator::Css("body"))
            .await?
            .text()
            .await
            .unwrap_or_default()
            .to_lowercase();
        if !t.contains("review q3 roadmap") {
            break;
        }
    }
    let after_done = client
        .find(Locator::Css("body"))
        .await?
        .text()
        .await
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        !after_done.contains("review q3 roadmap"),
        "Done action should have disappeared from the default list. text: {after_done}"
    );

    // Verify the backend recorded a user-driven 'resolved' event. We open
    // a separate read pool against the same file because the app holds
    // the write side.
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        let (event_kind, actor): (String, String) = sqlx::query_as(
            "SELECT event_kind, actor FROM action_events \
             WHERE actor = 'user' ORDER BY occurred_at DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .context("reading action_events after Done click")?;
        assert_eq!(event_kind, "resolved");
        assert_eq!(actor, "user");
        pool.close().await;
    }

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

    // 8. Click the Sync now button and assert the expected error toast
    //    (no LLM configured — see MNEMIS_DISABLE_LLM in step 2).
    let click_result = client
        .execute(
            r#"
            const btn = document.querySelector('button.sync-button');
            if (!btn) { return 'missing-button'; }
            btn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(
        click_result.as_str(),
        Some("ok"),
        "sync button selector did not match: {click_result:?}"
    );
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.status-toast"))
        .await
        .context("waiting for sync toast")?;
    let toast_text = client
        .find(Locator::Css("div.status-toast"))
        .await?
        .text()
        .await
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        toast_text.contains("sync failed") && toast_text.contains("no llm"),
        "expected 'sync failed: no llm' style toast; got: {toast_text}"
    );

    // 9. No console errors / window errors.
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

/// Two adjacent flows share one seeded DB: dismissing a pending action with
/// a comment writes a `dismissal_feedback` row, while undoing an auto-claim
/// via the Skip path writes none (the user often took the action out of band
/// and there's nothing the model could have learned). Pin both end-to-end so
/// the modal's Submit/Skip wiring stays load-bearing.
#[tokio::test(flavor = "current_thread")]
async fn dismiss_records_feedback_and_undo_skip_records_none() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-feedback.db");
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        seed(&pool).await?;
        pool.close().await;
    }

    let app_bin = sibling_app_binary()
        .context("mnemis-app binary missing; run `cargo build -p mnemis-app` before the test")?;
    let env = HashMap::from([
        ("MNEMIS_DB_PATH".to_string(), db_path.display().to_string()),
        ("MNEMIS_DISABLE_LLM".to_string(), "1".to_string()),
    ]);
    let harness = Harness::start(HarnessOpts::default(), env).await?;
    let client = harness.open_session(&app_bin).await?;

    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.action"))
        .await
        .context("waiting for actions to render")?;
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 1. Click Undo on the auto-claimed action ("Review Q3 roadmap"), then
    //    Skip the feedback modal. The action should drop out of the default
    //    list (it goes back to pending — still listed) but the modal must
    //    appear and close cleanly.
    let undo = client
        .execute(
            r#"
            const card = Array.from(document.querySelectorAll('div.action'))
                .find(c => c.textContent.includes('Review Q3 roadmap'));
            if (!card) { return 'missing-card'; }
            const btn = Array.from(card.querySelectorAll('button')).find(b => b.textContent.trim() === 'Undo');
            if (!btn) { return 'missing-undo'; }
            btn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(undo.as_str(), Some("ok"), "Undo click failed: {undo:?}");

    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css(
            "div.feedback-modal[data-feedback-kind='wrong_auto_claim']",
        ))
        .await
        .context("waiting for the wrong-auto-claim feedback modal")?;

    let skip = client
        .execute(
            r#"
            const modal = document.querySelector("div.feedback-modal[data-feedback-kind='wrong_auto_claim']");
            if (!modal) { return 'missing-modal'; }
            const skipBtn = Array.from(modal.querySelectorAll('button')).find(b => b.textContent.trim() === 'Skip');
            if (!skipBtn) { return 'missing-skip'; }
            skipBtn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(skip.as_str(), Some("ok"), "Skip click failed: {skip:?}");

    // Wait for the modal to disappear.
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if client
            .find(Locator::Css("div.feedback-modal"))
            .await
            .is_err()
        {
            break;
        }
    }
    assert!(
        client
            .find(Locator::Css("div.feedback-modal"))
            .await
            .is_err(),
        "feedback modal should have closed after Skip"
    );

    // 2. Reveal the low-confidence list (the pending "Background reading"
    //    action lives there) and Dismiss it with a comment.
    let reveal = client
        .execute(
            r#"
            const rev = document.querySelector('div.revealer');
            if (!rev) { return 'missing-revealer'; }
            rev.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(reveal.as_str(), Some("ok"));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let dismiss = client
        .execute(
            r#"
            const card = Array.from(document.querySelectorAll('div.action'))
                .find(c => c.textContent.toLowerCase().includes('background reading'));
            if (!card) { return 'missing-card'; }
            const btn = Array.from(card.querySelectorAll('button')).find(b => b.textContent.trim() === 'Dismiss');
            if (!btn) { return 'missing-dismiss'; }
            btn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(
        dismiss.as_str(),
        Some("ok"),
        "Dismiss click failed: {dismiss:?}"
    );

    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css(
            "div.feedback-modal[data-feedback-kind='dismissed']",
        ))
        .await
        .context("waiting for the dismissed feedback modal")?;

    let submit = client
        .execute(
            r#"
            const modal = document.querySelector("div.feedback-modal[data-feedback-kind='dismissed']");
            if (!modal) { return 'missing-modal'; }
            const ta = modal.querySelector('textarea.feedback-input');
            if (!ta) { return 'missing-textarea'; }
            ta.value = 'this came from an automated list, not actionable';
            ta.dispatchEvent(new Event('input', { bubbles: true }));
            const submitBtn = Array.from(modal.querySelectorAll('button')).find(b => b.textContent.trim() === 'Submit feedback');
            if (!submitBtn) { return 'missing-submit'; }
            submitBtn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(
        submit.as_str(),
        Some("ok"),
        "Submit click failed: {submit:?}"
    );

    // Wait for the modal to disappear AND the feedback row to land.
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if client
            .find(Locator::Css("div.feedback-modal"))
            .await
            .is_err()
        {
            break;
        }
    }

    // 3. Verify the DB state: exactly one dismissal_feedback row, the right
    //    kind and reason. The Skip path must NOT have written a row.
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT kind, reason, scope_kind FROM dismissal_feedback")
                .fetch_all(&pool)
                .await?;
        assert_eq!(
            rows.len(),
            1,
            "Skip should have written nothing; Submit should have written one row. got: {rows:?}"
        );
        let (kind, reason, scope_kind) = &rows[0];
        assert_eq!(kind, "dismissed");
        assert!(
            reason.contains("automated list"),
            "reason should carry the typed comment, got: {reason:?}"
        );
        assert_eq!(scope_kind, "channel");

        // And the undo path emitted an 'unclaimed' event, not 'unresolved'.
        let kinds: Vec<(String,)> =
            sqlx::query_as("SELECT event_kind FROM action_events WHERE actor = 'user' ORDER BY id")
                .fetch_all(&pool)
                .await?;
        let kinds: Vec<String> = kinds.into_iter().map(|(k,)| k).collect();
        assert!(
            kinds.contains(&"unclaimed".to_string()),
            "expected an 'unclaimed' event from the Undo path. events: {kinds:?}"
        );
        assert!(
            kinds.contains(&"dismissed".to_string()),
            "expected a 'dismissed' event from the Dismiss path. events: {kinds:?}"
        );
        pool.close().await;
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

/// Sync now produces per-channel/per-source errors; the toast must show the
/// **classified** one-liner (via mnemis_types::summarize_sync_error), not
/// the raw anyhow chain. Triggers it by seeding an IMAP source whose
/// build_imap_source fails (no settings row), then clicking Sync.
///
/// Uses a temp config.toml + MNEMIS_CONFIG_PATH so the LlmStack initializes
/// without depending on the dev machine's real config. The LLM is never
/// reached because sync fails at IMAP setup, so the URL can be bogus.
#[tokio::test(flavor = "current_thread")]
async fn toast_classifies_per_source_error() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-class.db");
    let config_path = tmp.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[llm]\nbase_url = \"http://0.0.0.0:1/v1\"\nchat_model = \"none\"\n\
             embedding_model = \"none\"\n\n[paths]\ndb = \"{}\"\n",
            db_path.display(),
        ),
    )?;

    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        // Single source, no settings row → build_imap_source fails with
        // "missing IMAP connection settings".
        sqlx::query(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'work', 'fake/missing', ?)",
        )
        .bind(Utc::now().timestamp())
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             SELECT id, 'INBOX', 'INBOX', 'mailbox' FROM sources WHERE name = 'work'",
        )
        .execute(&pool)
        .await?;
        // user_profile is required by the status query.
        sqlx::query(
            "INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, 'Tester', ?)",
        )
        .bind(Utc::now().timestamp())
        .execute(&pool)
        .await?;
        pool.close().await;
    }

    let app_bin = sibling_app_binary()
        .context("mnemis-app binary missing; run `cargo build -p mnemis-app` before the test")?;

    let env = HashMap::from([
        ("MNEMIS_DB_PATH".to_string(), db_path.display().to_string()),
        (
            "MNEMIS_CONFIG_PATH".to_string(),
            config_path.display().to_string(),
        ),
        // Deliberately NOT setting MNEMIS_DISABLE_LLM — we want LlmStack to
        // initialize so sync_now reaches the orchestrator and emits the
        // per-source error in SyncOutcome.errors. (Harness applies env to
        // the child only, so other tests' values don't leak in.)
    ]);
    let harness = Harness::start(HarnessOpts::default(), env).await?;
    let client = harness.open_session(&app_bin).await?;

    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("button.sync-button"))
        .await
        .context("waiting for sync button")?;
    tokio::time::sleep(Duration::from_millis(800)).await;

    let click_result = client
        .execute(
            "const btn = document.querySelector('button.sync-button'); \
             if (!btn) { return 'missing'; } btn.click(); return 'ok';",
            vec![],
        )
        .await?;
    assert_eq!(click_result.as_str(), Some("ok"));

    // Wait for the success toast (Ok branch — sync_now returned Ok with
    // errors populated, since IMAP build failure is per-source, not
    // top-level).
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("ul.status-errors"))
        .await
        .context("waiting for status-errors list inside the sync toast")?;

    let toast_text = client
        .find(Locator::Css("ul.status-errors"))
        .await?
        .text()
        .await
        .unwrap_or_default();

    assert!(
        toast_text.contains("Missing IMAP"),
        "toast should show the classified summary, not the raw chain. got: {toast_text}"
    );
    assert!(
        !toast_text.contains("no rows returned"),
        "raw sqlx error should NOT leak into the user-facing summary. got: {toast_text}"
    );

    client.close().await.ok();
    Ok(())
}
