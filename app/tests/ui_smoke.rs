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

/// Settings → Sources expand-row exposes a per-channel tree with checkboxes
/// and bulk Enable/Disable buttons. Seed one source with a nested mailbox
/// (`INBOX` + `INBOX/Lembrar` + `Archive`) so the tree code is exercised,
/// then pin three things end-to-end:
///   * one-by-one checkbox toggle persists and leaves siblings alone,
///   * "Disable all" mutes every channel atomically,
///   * "Enable all" unmutes every channel atomically.
#[tokio::test(flavor = "current_thread")]
async fn settings_sources_per_channel_mute_round_trips() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-channels.db");
    let (chatty_id, nested_id, quiet_id) = {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, 'Tester', ?)",
        )
        .bind(now)
        .execute(&pool)
        .await?;
        let (sid,): (i64,) = sqlx::query_as(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'work', 'kc/work', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(&pool)
        .await?;
        let mut inserted = Vec::new();
        for ext in ["INBOX", "INBOX/Lembrar", "Archive"] {
            let (id,): (i64,) = sqlx::query_as(
                "INSERT INTO channels (source_id, external_id, name, kind) \
                 VALUES (?, ?, ?, 'mailbox') RETURNING id",
            )
            .bind(sid)
            .bind(ext)
            .bind(ext)
            .fetch_one(&pool)
            .await?;
            inserted.push(id);
        }
        let chatty = inserted[0];
        let nested = inserted[1];
        let quiet = inserted[2];
        for ext in ["m1", "m2", "m3"] {
            sqlx::query(
                "INSERT INTO messages (channel_id, external_id, posted_at, body, body_format, ingested_at) \
                 VALUES (?, ?, ?, 'b', 'text', ?)",
            )
            .bind(chatty)
            .bind(ext)
            .bind(now)
            .bind(now)
            .execute(&pool)
            .await?;
        }
        pool.close().await;
        (chatty, nested, quiet)
    };

    let app_bin = sibling_app_binary()
        .context("mnemis-app binary missing; run `cargo build -p mnemis-app` before the test")?;
    let env = HashMap::from([
        ("MNEMIS_DB_PATH".to_string(), db_path.display().to_string()),
        ("MNEMIS_DISABLE_LLM".to_string(), "1".to_string()),
    ]);
    let harness = Harness::start(HarnessOpts::default(), env).await?;
    let client = harness.open_session(&app_bin).await?;

    // Navigate Actions → Settings → Sources via the nav links.
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("nav.nav"))
        .await?;
    client
        .execute(
            r#"document.querySelector('nav.nav a[href="/settings"]').click(); return 'ok';"#,
            vec![],
        )
        .await?;
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("ul.settings-home"))
        .await?;
    client
        .execute(
            r#"document.querySelector('ul.settings-home a[href="/settings/sources"]').click(); return 'ok';"#,
            vec![],
        )
        .await?;
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("table.sources-table tr.source-row"))
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Expand the source row.
    let expand = client
        .execute(
            r#"
            const row = document.querySelector('tr.source-row[data-source-id]');
            const btn = row.querySelector('button.source-expand');
            if (!btn) { return 'missing-expand'; }
            btn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(expand.as_str(), Some("ok"));
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("ul.channels-tree li.channel-row"))
        .await
        .context("waiting for channels tree to render")?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // All three channels visible — the nested "Lembrar" gets its own row
    // under INBOX, so the leaf-count matches the seed.
    let count = client
        .execute(
            r#"return String(document.querySelectorAll('li.channel-row').length);"#,
            vec![],
        )
        .await?;
    assert_eq!(count.as_str(), Some("3"), "expected 3 channel rows");

    // Tree shape: the "Lembrar" row should be inside a nested <ul> that
    // hangs off the INBOX row's parent <li>. Asserting on the DOM nesting
    // here is the cheapest way to make sure the tree-builder isn't quietly
    // flattening or dropping branches.
    let nested_ok = client
        .execute(
            r#"
            const inbox = Array.from(document.querySelectorAll('li.channel-row'))
                .find(el => el.querySelector('.channel-name')?.textContent.trim() === 'INBOX');
            if (!inbox) { return 'no-inbox'; }
            const nested = inbox.parentElement.querySelector('ul.channel-children li.channel-row .channel-name');
            return nested ? nested.textContent.trim() : 'no-nested';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(
        nested_ok.as_str(),
        Some("Lembrar"),
        "expected Lembrar nested under INBOX in the tree, got {nested_ok:?}"
    );

    // 1) Toggle the INBOX checkbox off (mute that one mailbox).
    let click = client
        .execute(
            r#"
            const li = Array.from(document.querySelectorAll('li.channel-row'))
                .find(el => el.querySelector('.channel-name')?.textContent.trim() === 'INBOX');
            if (!li) { return 'missing-li'; }
            const cb = li.querySelector('input.channel-checkbox');
            if (!cb) { return 'missing-checkbox'; }
            cb.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(
        click.as_str(),
        Some("ok"),
        "checkbox click failed: {click:?}"
    );

    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let state = client
            .execute(
                r#"
                const li = Array.from(document.querySelectorAll('li.channel-row'))
                    .find(el => el.querySelector('.channel-name')?.textContent.trim() === 'INBOX');
                return li ? li.getAttribute('data-channel-muted') : 'gone';
                "#,
                vec![],
            )
            .await?;
        if state.as_str() == Some("true") {
            break;
        }
    }

    // DB: only the INBOX mailbox is muted; the sibling and the nested
    // child are untouched (no cascading on purpose).
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        let muted_of = async |id: i64| -> Result<i64> {
            let (m,): (i64,) = sqlx::query_as("SELECT muted FROM channels WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
            Ok(m)
        };
        assert_eq!(muted_of(chatty_id).await?, 1, "INBOX should be muted");
        assert_eq!(
            muted_of(nested_id).await?,
            0,
            "INBOX/Lembrar must stay unmuted — toggles don't cascade"
        );
        assert_eq!(muted_of(quiet_id).await?, 0, "Archive must stay unmuted");
        pool.close().await;
    }

    // 2) "Disable all" mutes every channel in one shot.
    client
        .execute(
            r#"document.querySelector('button.channels-disable-all').click(); return 'ok';"#,
            vec![],
        )
        .await?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let all_muted = client
            .execute(
                r#"
                const rows = Array.from(document.querySelectorAll('li.channel-row[data-channel-muted]'));
                return String(rows.every(li => li.getAttribute('data-channel-muted') === 'true'));
                "#,
                vec![],
            )
            .await?;
        if all_muted.as_str() == Some("true") {
            break;
        }
    }
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        let (unmuted_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM channels WHERE muted = 0")
                .fetch_one(&pool)
                .await?;
        assert_eq!(unmuted_count, 0, "Disable all should mute every channel");
        pool.close().await;
    }

    // 3) "Enable all" unmutes every channel in one shot.
    client
        .execute(
            r#"document.querySelector('button.channels-enable-all').click(); return 'ok';"#,
            vec![],
        )
        .await?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let none_muted = client
            .execute(
                r#"
                const rows = Array.from(document.querySelectorAll('li.channel-row[data-channel-muted]'));
                return String(rows.every(li => li.getAttribute('data-channel-muted') === 'false'));
                "#,
                vec![],
            )
            .await?;
        if none_muted.as_str() == Some("true") {
            break;
        }
    }
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        let (muted_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM channels WHERE muted = 1")
                .fetch_one(&pool)
                .await?;
        assert_eq!(muted_count, 0, "Enable all should unmute every channel");
        pool.close().await;
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

/// Suggestions panel: seed two pending actions, each with a queued
/// `suggested_resolution` event. Click Confirm on one (it goes to done +
/// disappears from the active list) and Reject on the other (action stays
/// pending; suggestion disappears via a `suggestion_dismissed` event).
#[tokio::test(flavor = "current_thread")]
async fn suggestions_panel_confirm_applies_and_reject_dismisses() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-suggestions.db");
    let (confirm_id, reject_id) = {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        seed(&pool).await?;
        // Reuse the two seeded actions as suggestion targets. Mark both
        // pending (the high one is auto_claimed by default) so the
        // suggestions query picks them up alongside.
        sqlx::query("UPDATE actions SET status = 'pending'")
            .execute(&pool)
            .await?;
        let actions: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, title FROM actions ORDER BY id")
                .fetch_all(&pool)
                .await?;
        assert_eq!(actions.len(), 2);
        let (confirm_id, _) = actions[0].clone();
        let (reject_id, _) = actions[1].clone();
        let now = Utc::now().timestamp();
        for (id, status_proposal) in [(confirm_id, "done"), (reject_id, "cancelled")] {
            let data = serde_json::json!({
                "status": status_proposal,
                "confidence": "medium",
                "rationale": "Looks resolved in the window",
            })
            .to_string();
            sqlx::query(
                "INSERT INTO action_events \
                 (action_id, event_kind, actor, data_json, occurred_at) \
                 VALUES (?, 'suggested_resolution', 'agent_queued', ?, ?)",
            )
            .bind(id)
            .bind(&data)
            .bind(now)
            .execute(&pool)
            .await?;
        }
        pool.close().await;
        (confirm_id, reject_id)
    };

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
        .for_element(Locator::Css("div.suggestions-panel"))
        .await
        .context("waiting for suggestions panel to render")?;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Sanity: both suggestion rows should be present, identified by their
    // data-action-id attributes.
    let counts = client
        .execute(
            r#"
            return JSON.stringify({
                total: document.querySelectorAll('div.suggestion').length,
                confirm: document.querySelectorAll(`div.suggestion[data-action-id]`).length,
            });
            "#,
            vec![],
        )
        .await?;
    assert!(
        counts.as_str().unwrap_or("").contains("\"total\":2"),
        "expected 2 suggestion rows, got: {counts:?}"
    );

    // Confirm the first one — action should go to 'done' and the suggestion
    // row should disappear.
    let confirm = client
        .execute(
            &format!(
                r#"
                const row = document.querySelector(`div.suggestion[data-action-id="{confirm_id}"]`);
                if (!row) {{ return 'missing-row'; }}
                const btn = Array.from(row.querySelectorAll('button')).find(b => b.textContent.trim() === 'Confirm');
                if (!btn) {{ return 'missing-confirm'; }}
                btn.click();
                return 'ok';
                "#
            ),
            vec![],
        )
        .await?;
    assert_eq!(
        confirm.as_str(),
        Some("ok"),
        "Confirm click failed: {confirm:?}"
    );

    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let still = client
            .execute(
                &format!(
                    r#"
                    return document.querySelector(`div.suggestion[data-action-id="{confirm_id}"]`) ? 'present' : 'gone';
                    "#
                ),
                vec![],
            )
            .await?;
        if still.as_str() == Some("gone") {
            break;
        }
    }

    // Reject the second one — action stays pending, but the suggestion row
    // should disappear and a 'suggestion_dismissed' event should land.
    let reject = client
        .execute(
            &format!(
                r#"
                const row = document.querySelector(`div.suggestion[data-action-id="{reject_id}"]`);
                if (!row) {{ return 'missing-row'; }}
                const btn = Array.from(row.querySelectorAll('button')).find(b => b.textContent.trim() === 'Reject');
                if (!btn) {{ return 'missing-reject'; }}
                btn.click();
                return 'ok';
                "#
            ),
            vec![],
        )
        .await?;
    assert_eq!(
        reject.as_str(),
        Some("ok"),
        "Reject click failed: {reject:?}"
    );

    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let still = client
            .execute(
                &format!(
                    r#"
                    return document.querySelector(`div.suggestion[data-action-id="{reject_id}"]`) ? 'present' : 'gone';
                    "#
                ),
                vec![],
            )
            .await?;
        if still.as_str() == Some("gone") {
            break;
        }
    }

    // Verify DB state.
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        let confirm_state: (String, Option<i64>) =
            sqlx::query_as("SELECT status, resolved_at FROM actions WHERE id = ?")
                .bind(confirm_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(confirm_state.0, "done", "confirmed action should be done");
        assert!(confirm_state.1.is_some(), "resolved_at should be set");

        let reject_state: (String,) = sqlx::query_as("SELECT status FROM actions WHERE id = ?")
            .bind(reject_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(reject_state.0, "pending", "rejected action stays pending");
        let (dismissed_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM action_events \
             WHERE action_id = ? AND event_kind = 'suggestion_dismissed'",
        )
        .bind(reject_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            dismissed_count, 1,
            "expected one suggestion_dismissed event"
        );
        pool.close().await;
    }

    client.close().await.ok();
    Ok(())
}

/// First-run banner: fresh DB (no user_profile, no sources) shows the welcome
/// banner with a link to the profile settings. After the profile is saved
/// (here directly via SQL — the form interaction is covered separately) the
/// banner disappears on a refresh / sync_tick bump.
#[tokio::test(flavor = "current_thread")]
async fn first_run_banner_appears_on_empty_db() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-firstrun.db");
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        // Deliberately empty — no user_profile row, no sources.
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
        .for_element(Locator::Css("div.first-run-banner"))
        .await
        .context("waiting for first-run banner")?;

    let banner = client
        .find(Locator::Css("div.first-run-banner"))
        .await?
        .text()
        .await
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        banner.contains("welcome") && banner.contains("profile"),
        "banner text: {banner}"
    );

    // The link inside the banner must route to /settings/profile so the user
    // can act on the prompt with a single click.
    let href = client
        .execute(
            r#"
            const a = document.querySelector('div.first-run-banner a');
            return a ? a.getAttribute('href') : null;
            "#,
            vec![],
        )
        .await?;
    assert_eq!(
        href.as_str(),
        Some("/settings/profile"),
        "banner link should point at the profile page, got: {href:?}"
    );

    client.close().await.ok();
    Ok(())
}

/// Profile editor round-trip: load empty form, fill it, save, assert the
/// `user_profile` + `contact_identifiers` rows landed, and confirm the
/// first-run banner is gone.
#[tokio::test(flavor = "current_thread")]
async fn profile_editor_saves_and_clears_first_run_banner() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-profile.db");
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
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

    // Navigate to the profile page via the banner link.
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.first-run-banner a"))
        .await?;
    let nav = client
        .execute(
            r#"
            const a = document.querySelector('div.first-run-banner a');
            if (!a) { return 'missing'; }
            a.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(nav.as_str(), Some("ok"));
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.settings-form[data-form='profile']"))
        .await
        .context("waiting for profile form")?;

    // Fill the form, add one identifier, save.
    let fill = client
        .execute(
            r#"
            const form = document.querySelector("div.settings-form[data-form='profile']");
            const name = form.querySelector('input.settings-input');
            name.value = 'Gustavo';
            name.dispatchEvent(new Event('input', { bubbles: true }));
            const prompt = form.querySelector('textarea.settings-textarea');
            prompt.value = 'Ana is my direct report.';
            prompt.dispatchEvent(new Event('input', { bubbles: true }));

            // Add identifier: select kind=email, value=g@x.com, click Add.
            const addRow = form.querySelector('div.identifier-add');
            const value = addRow.querySelector('input.settings-input');
            value.value = 'g@x.com';
            value.dispatchEvent(new Event('input', { bubbles: true }));
            const addBtn = Array.from(addRow.querySelectorAll('button')).find(b => b.textContent.trim() === 'Add');
            addBtn.click();

            // Save.
            const saveBtn = Array.from(form.querySelectorAll('button')).find(b => b.textContent.trim() === 'Save profile');
            saveBtn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(fill.as_str(), Some("ok"), "fill+save script: {fill:?}");

    // Wait for the toast.
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.settings-toast"))
        .await
        .context("waiting for save toast")?;

    // Banner should be gone now (sync_tick was bumped on save).
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if client
            .find(Locator::Css("div.first-run-banner"))
            .await
            .is_err()
        {
            break;
        }
    }
    assert!(
        client
            .find(Locator::Css("div.first-run-banner"))
            .await
            .is_err(),
        "first-run banner should have disappeared after the profile save"
    );

    // DB state.
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        let (name, custom): (String, Option<String>) =
            sqlx::query_as("SELECT display_name, custom_prompt FROM user_profile WHERE id = 1")
                .fetch_one(&pool)
                .await?;
        assert_eq!(name, "Gustavo");
        assert!(custom.as_deref().unwrap_or("").contains("direct report"));

        let (ident_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM contact_identifiers ci \
             JOIN contacts c ON c.id = ci.contact_id \
             WHERE c.relationship = 'self' AND ci.kind = 'email' AND ci.value = 'g@x.com'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(ident_count, 1, "expected the email identifier to be saved");
        pool.close().await;
    }

    client.close().await.ok();
    Ok(())
}

/// Sources page surfaces seeded sources, the Add IMAP modal opens cleanly,
/// and the mute toggle round-trips through the DB. The add path itself
/// isn't exercised end-to-end because writing to the keychain requires a
/// live secret-service over D-Bus, which the headless test harness doesn't
/// provide. Engine-side `add_imap_source` is unit-tested elsewhere.
#[tokio::test(flavor = "current_thread")]
async fn settings_sources_lists_and_modal_opens() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-sources.db");
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO sources (kind, name, config_ref, created_at) \
             VALUES ('imap', 'work', 'kc/work', ?)",
        )
        .bind(now)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO channels (source_id, external_id, name, kind) \
             SELECT id, 'INBOX', 'INBOX', 'mailbox' FROM sources WHERE name = 'work'",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, 'Tester', ?)",
        )
        .bind(now)
        .execute(&pool)
        .await?;
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

    // Navigate to /settings/sources.
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("nav.nav"))
        .await?;
    let go = client
        .execute(
            r#"
            const a = document.querySelector('nav.nav a[href="/settings"]');
            if (!a) { return 'missing-nav'; }
            a.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(go.as_str(), Some("ok"));
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("ul.settings-home"))
        .await
        .context("waiting for settings home")?;
    let go2 = client
        .execute(
            r#"
            const a = document.querySelector('ul.settings-home a[href="/settings/sources"]');
            if (!a) { return 'missing-link'; }
            a.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(go2.as_str(), Some("ok"));
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("table.sources-table"))
        .await
        .context("waiting for sources table")?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let body = client
        .find(Locator::Css("body"))
        .await?
        .text()
        .await
        .unwrap_or_default();
    assert!(
        body.contains("work"),
        "expected the seeded source name in sources table, got: {body}"
    );

    // Toggle mute: click the Mute button, assert it flipped to "Unmute".
    let mute = client
        .execute(
            r#"
            const row = document.querySelector('tr.source-row[data-source-id]');
            const btn = Array.from(row.querySelectorAll('button')).find(b => b.textContent.trim() === 'Mute');
            if (!btn) { return 'missing-mute'; }
            btn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(mute.as_str(), Some("ok"));
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let label = client
            .execute(
                r#"
                const row = document.querySelector('tr.source-row[data-source-id]');
                const btn = Array.from(row.querySelectorAll('button')).find(b =>
                    b.textContent.trim() === 'Mute' || b.textContent.trim() === 'Unmute');
                return btn ? btn.textContent.trim() : 'gone';
                "#,
                vec![],
            )
            .await?;
        if label.as_str() == Some("Unmute") {
            break;
        }
    }
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        let (muted,): (i64,) =
            sqlx::query_as("SELECT MAX(muted) FROM channels WHERE source_id IN (SELECT id FROM sources WHERE name = 'work')")
                .fetch_one(&pool)
                .await?;
        assert_eq!(muted, 1, "channels should be muted in DB");
        pool.close().await;
    }

    // Open the Add IMAP modal — don't submit (would need keychain).
    let open = client
        .execute(
            r#"
            const btn = Array.from(document.querySelectorAll('button'))
                .find(b => b.textContent.trim() === 'Add IMAP source');
            if (!btn) { return 'missing'; }
            btn.click();
            return 'ok';
            "#,
            vec![],
        )
        .await?;
    assert_eq!(open.as_str(), Some("ok"));
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css(
            "div.add-source-modal[data-source-kind='imap']",
        ))
        .await
        .context("waiting for add-source modal")?;
    let modal_inputs = client
        .execute(
            r#"
            const modal = document.querySelector("div.add-source-modal[data-source-kind='imap']");
            return String(modal.querySelectorAll('input.settings-input').length);
            "#,
            vec![],
        )
        .await?;
    assert_eq!(
        modal_inputs.as_str(),
        Some("5"),
        "modal should expose name/server/port/username/password inputs"
    );

    client.close().await.ok();
    Ok(())
}

/// Seed exactly two actions linked to messages: one high-confidence (visible
/// up front) and one low-confidence (hidden behind the revealer). Matches
/// the `record_action` insert shape so the same `queries::list_actions` SQL
/// returns them.
/// Phase 4: the Chats tab lists a seeded conversation and renders its
/// transcript. The live send path needs an LLM (covered by engine tests); this
/// asserts the read/render path the user sees, under MNEMIS_DISABLE_LLM.
#[tokio::test(flavor = "current_thread")]
async fn chat_view_lists_and_renders_a_seeded_transcript() -> Result<()> {
    use mnemis_engine::chat::store;

    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-chat.db");
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, 'Smoke Tester', ?)",
        )
        .bind(now)
        .execute(&pool)
        .await?;

        // An action to seed a "Talk about this" chat from (drives the banner).
        let (action_id,): (i64,) = sqlx::query_as(
            "INSERT INTO actions (title, details, confidence, rationale, status, extracted_at) \
             VALUES ('Renew the TLS cert', 'x', 'high', 'r', 'auto_claimed', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(&pool)
        .await?;

        // Seed a small transcript on a chat that's about that action.
        let chat_id = store::create_chat(&pool, Some("action"), Some(action_id)).await?;
        let q = "why is the renewal flagged?";
        store::append_turn(&pool, chat_id, "user", Some(q), None, None, None).await?;
        store::ensure_title(&pool, chat_id, q).await?;
        let a = store::append_turn(
            &pool,
            chat_id,
            "assistant",
            Some("Because Ana asked you to renew the cert by Friday."),
            None,
            None,
            None,
        )
        .await?;
        store::append_reasoning(&pool, a, "Checked the evidence message.").await?;

        // Pre-flight: the app's queries see the chat + its turns.
        assert_eq!(
            store::list_chats(&pool).await?.len(),
            1,
            "pre-flight: one chat"
        );
        assert_eq!(
            store::load_turns(&pool, chat_id).await?.len(),
            2,
            "pre-flight: two turns"
        );
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
        .for_element(Locator::Css("nav.nav"))
        .await
        .context("waiting for nav.nav")?;

    // Navigate to the Chats tab.
    client
        .execute(
            r#"document.querySelector('nav.nav a[href="/chats"]').click(); return 'ok';"#,
            vec![],
        )
        .await?;

    // The chat is listed under its derived title.
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("a.chat-list-item"))
        .await
        .context("waiting for a.chat-list-item")?;
    let list_text = client
        .find(Locator::Css("body"))
        .await?
        .text()
        .await
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        list_text.contains("why is the renewal flagged"),
        "chat list should show the derived title. got: {list_text}"
    );

    // Open it; the transcript shows both turns.
    client
        .execute(
            r#"document.querySelector('a.chat-list-item').click(); return 'ok';"#,
            vec![],
        )
        .await?;
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.chat-transcript"))
        .await
        .context("waiting for div.chat-transcript")?;
    tokio::time::sleep(Duration::from_millis(800)).await;

    let chat_text = client
        .find(Locator::Css("body"))
        .await?
        .text()
        .await
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        chat_text.contains("why is the renewal flagged"),
        "transcript should show the user message. got: {chat_text}"
    );
    assert!(
        chat_text.contains("ana asked you to renew"),
        "transcript should show the assistant message. got: {chat_text}"
    );
    // The "Talk about this" seed banner shows what the chat is grounded in.
    assert!(
        chat_text.contains("renew the tls cert"),
        "seeded chat should show its 'About …' context banner. got: {chat_text}"
    );

    let html = client
        .find(Locator::Css("body"))
        .await?
        .html(true)
        .await
        .unwrap_or_default();
    assert!(
        html.contains("chat-input"),
        "the chat composer should be present. html: {html}"
    );
    assert!(
        html.contains("chat-seed-banner"),
        "the seed banner element should be present. html: {html}"
    );

    Ok(())
}

/// Phase 4: the model answers in markdown, so the assistant bubble renders it
/// as formatted HTML — and because that markdown can carry prompt-injected
/// markup from ingested mail, the render is sanitized: raw `<script>` is
/// dropped and `javascript:` links degrade to plain text, while a real https
/// link survives. No LLM needed — runs under MNEMIS_DISABLE_LLM.
#[tokio::test(flavor = "current_thread")]
async fn chat_view_renders_assistant_markdown_safely() -> Result<()> {
    use mnemis_engine::chat::store;

    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-markdown.db");
    let md = concat!(
        "Here is **bold emphasis** and `inline code`.\n\n",
        "```\nlet x = 1;\n```\n\n",
        "See [the docs](https://example.com/docs) and [danger](javascript:alert(1)).\n\n",
        "<script>alert('xss')</script>\n",
    );
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, 'Smoke Tester', ?)",
        )
        .bind(now)
        .execute(&pool)
        .await?;

        let chat_id = store::create_chat(&pool, None, None).await?;
        let q = "show me some formatting";
        store::append_turn(&pool, chat_id, "user", Some(q), None, None, None).await?;
        store::ensure_title(&pool, chat_id, q).await?;
        store::append_turn(&pool, chat_id, "assistant", Some(md), None, None, None).await?;
        pool.close().await;
    }

    let app_bin = sibling_app_binary()?;
    let env = HashMap::from([
        ("MNEMIS_DB_PATH".to_string(), db_path.display().to_string()),
        ("MNEMIS_DISABLE_LLM".to_string(), "1".to_string()),
    ]);
    let harness = Harness::start(HarnessOpts::default(), env).await?;
    let client = harness.open_session(&app_bin).await?;

    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("nav.nav"))
        .await
        .context("waiting for nav.nav")?;
    client
        .execute(
            r#"document.querySelector('nav.nav a[href="/chats"]').click(); return 'ok';"#,
            vec![],
        )
        .await?;
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("a.chat-list-item"))
        .await
        .context("waiting for a.chat-list-item")?;
    client
        .execute(
            r#"document.querySelector('a.chat-list-item').click(); return 'ok';"#,
            vec![],
        )
        .await?;
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.chat-assistant"))
        .await
        .context("waiting for div.chat-assistant")?;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let rendered = client
        .find(Locator::Css("div.chat-assistant"))
        .await?
        .html(true)
        .await
        .unwrap_or_default();

    // Formatting survived as real HTML elements.
    assert!(
        rendered.contains("<strong>bold emphasis</strong>"),
        "bold markdown should render as <strong>. got: {rendered}"
    );
    assert!(
        rendered.contains("<pre>") && rendered.contains("<code>"),
        "a fenced block should render as <pre><code>. got: {rendered}"
    );
    // The safe link is a real anchor pointing at the original https URL.
    assert!(
        rendered.contains("https://example.com/docs"),
        "an https link should survive. got: {rendered}"
    );

    // Sanitization: nothing dangerous made it through.
    assert!(
        !rendered.contains("<script"),
        "raw <script> must be dropped. got: {rendered}"
    );
    assert!(
        !rendered.contains("alert('xss')"),
        "raw HTML script body must be dropped. got: {rendered}"
    );
    assert!(
        !rendered.to_lowercase().contains("javascript:"),
        "a javascript: link must degrade to plain text. got: {rendered}"
    );
    // …but the disallowed link's visible text is preserved.
    assert!(
        rendered.contains("danger"),
        "the unsafe link's text should remain. got: {rendered}"
    );

    Ok(())
}

/// Phase 4: a tall transcript auto-scrolls to the newest message on open, so
/// the user sees the latest content rather than the top. Guards the sticky-
/// scroll behavior (the bug: scroll reset to the top on every new message).
/// No LLM needed — runs under MNEMIS_DISABLE_LLM.
#[tokio::test(flavor = "current_thread")]
async fn chat_view_auto_scrolls_to_the_latest_message() -> Result<()> {
    use mnemis_engine::chat::store;

    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("ui-smoke-scroll.db");
    {
        let pool = mnemis_engine::db::open(&db_path).await?;
        mnemis_engine::db::migrate(&pool).await?;
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO user_profile (id, display_name, updated_at) VALUES (1, 'Smoke Tester', ?)",
        )
        .bind(now)
        .execute(&pool)
        .await?;

        // Many turns so the transcript overflows the 1280x720 viewport.
        let chat_id = store::create_chat(&pool, None, None).await?;
        for i in 0..40 {
            let (role, text) = if i % 2 == 0 {
                (
                    "user",
                    format!("Question number {i} — tell me about action item {i}."),
                )
            } else {
                (
                    "assistant",
                    format!(
                        "Answer number {i}. Here is a longer paragraph so the transcript grows tall enough to need scrolling and we can tell whether the view pinned to the bottom."
                    ),
                )
            };
            store::append_turn(&pool, chat_id, role, Some(&text), None, None, None).await?;
        }
        // A unique marker on the very last turn.
        store::append_turn(
            &pool,
            chat_id,
            "assistant",
            Some("FINAL-MARKER-LINE: this should be visible at the bottom on open."),
            None,
            None,
            None,
        )
        .await?;
        pool.close().await;
    }

    let app_bin = sibling_app_binary()?;
    let env = HashMap::from([
        ("MNEMIS_DB_PATH".to_string(), db_path.display().to_string()),
        ("MNEMIS_DISABLE_LLM".to_string(), "1".to_string()),
    ]);
    let harness = Harness::start(HarnessOpts::default(), env).await?;
    let client = harness.open_session(&app_bin).await?;

    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("nav.nav"))
        .await?;
    client
        .execute(
            r#"document.querySelector('nav.nav a[href="/chats"]').click(); return 'ok';"#,
            vec![],
        )
        .await?;
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("a.chat-list-item"))
        .await?;
    client
        .execute(
            r#"document.querySelector('a.chat-list-item').click(); return 'ok';"#,
            vec![],
        )
        .await?;
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.chat-transcript"))
        .await?;
    // Give the resource + the post-render rAF a moment to settle.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let metrics = client
        .execute(
            r#"const el = document.querySelector('.chat-transcript');
               return JSON.stringify({top: el.scrollTop, sh: el.scrollHeight, ch: el.clientHeight});"#,
            vec![],
        )
        .await?;
    let m: Value = serde_json::from_str(metrics.as_str().unwrap_or("{}"))?;
    let top = m["top"].as_f64().unwrap_or(0.0);
    let sh = m["sh"].as_f64().unwrap_or(0.0);
    let ch = m["ch"].as_f64().unwrap_or(0.0);

    // The transcript must actually be scrollable (else the test proves nothing).
    assert!(
        sh > ch + 100.0,
        "transcript should overflow (scrollHeight {sh} vs clientHeight {ch})"
    );
    // And it must have auto-scrolled to (near) the bottom on open.
    assert!(
        top >= sh - ch - 80.0,
        "transcript should be pinned to the bottom on open: scrollTop {top}, max {}",
        sh - ch
    );

    Ok(())
}

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

    // A completed-with-errors sync must read as a warning (amber), not a
    // clean green success — the whole point of the toast is to flag that
    // something needs attention.
    let toast_class = client
        .find(Locator::Css("div.status-toast"))
        .await?
        .attr("class")
        .await?
        .unwrap_or_default();
    assert!(
        toast_class.contains("status-toast-warning"),
        "partial-error sync should use the warning toast style. got class: {toast_class}"
    );
    assert!(
        !toast_class.contains("status-toast-ok"),
        "partial-error sync must not use the green success style. got class: {toast_class}"
    );

    client.close().await.ok();
    Ok(())
}
