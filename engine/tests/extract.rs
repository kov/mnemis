//! Integration tests for the extraction agent.
//!
//! By default these run against `MockLlm` with scripted responses (fast,
//! CI-safe). To exercise the real LLM:
//!
//!     MNEMIS_TEST_LLM=live \
//!     MNEMIS_TEST_LLM_URL=http://alface:1234/v1 \
//!     MNEMIS_TEST_LLM_MODEL=gemma-4-26b-a4b-it-4bit \
//!     cargo test --test extract
//!
//! Assertions intentionally check outcomes (action exists, confidence, evidence
//! linking) rather than tool-call sequences so the same tests pass in both
//! modes — see notes in the codebase for the rationale.

use anyhow::Result;
use mnemis_engine::{
    db,
    extract::{DEFAULT_WINDOW_CHAR_BUDGET, extract_for_channel},
    test_util::{
        SeedMessage, assert_evidence_contains, fetch_actions, make_test_llm, mock, seed_messages,
        seed_minimal,
    },
};
use sqlx::SqlitePool;
use tempfile::TempDir;

async fn fresh_db() -> Result<(TempDir, SqlitePool)> {
    let tmp = TempDir::new()?;
    let pool = db::open(&tmp.path().join("test.db")).await?;
    db::migrate(&pool).await?;
    Ok((tmp, pool))
}

#[tokio::test]
async fn extracts_explicit_ask_into_high_confidence_action() -> Result<()> {
    let (_tmp, pool) = fresh_db().await?;
    let ctx = seed_minimal(&pool).await?;

    seed_messages(
        &pool,
        ctx.source_id,
        ctx.channel_id,
        &[SeedMessage {
            external_id: "msg-001",
            author_email: "ana@example.com",
            author_name: "Ana Souza",
            subject: "Q3 roadmap draft",
            body: "Hi, can you take a pass on the Q3 roadmap draft by EOD Wednesday? \
                   I want to send it to the team Thursday morning. — Ana",
        }],
    )
    .await?;

    let llm = make_test_llm(vec![
        mock::record_action("Review Q3 roadmap draft", "high", &["msg-001"]),
        mock::no_tools("Recorded one action."),
    ]);

    let outcome = extract_for_channel(
        &pool,
        &*llm,
        ctx.channel_id,
        &ctx.model,
        DEFAULT_WINDOW_CHAR_BUDGET,
        None,
    )
    .await?;
    assert_eq!(outcome.result, "ok", "expected ok, got {:?}", outcome);

    let actions = fetch_actions(&pool).await?;
    assert_eq!(
        actions.len(),
        1,
        "expected exactly 1 action, got {}",
        actions.len()
    );

    let a = &actions[0];
    assert_eq!(
        a.confidence, "high",
        "expected high confidence, got {}",
        a.confidence
    );
    assert_eq!(
        a.status, "auto_claimed",
        "high-confidence action should be auto_claimed"
    );
    assert!(
        a.title.to_lowercase().contains("review")
            || a.title.to_lowercase().contains("pass")
            || a.title.to_lowercase().contains("roadmap"),
        "title should reference reviewing/the roadmap, got: {}",
        a.title
    );
    assert_evidence_contains(&pool, a.id, "msg-001").await?;
    Ok(())
}

#[tokio::test]
async fn skips_purely_informational_message() -> Result<()> {
    let (_tmp, pool) = fresh_db().await?;
    let ctx = seed_minimal(&pool).await?;

    seed_messages(
        &pool,
        ctx.source_id,
        ctx.channel_id,
        &[SeedMessage {
            external_id: "msg-news",
            author_email: "newsletter@example.com",
            author_name: "Engineering Newsletter",
            subject: "Weekly engineering newsletter",
            body: "This week's newsletter: release 4.2 shipped Tuesday, perf wins on the \
                   indexer, upcoming offsite logistics. No questions asked, no deadlines.",
        }],
    )
    .await?;

    let llm = make_test_llm(vec![mock::no_tools(
        "Nothing to extract — informational only.",
    )]);

    let outcome = extract_for_channel(
        &pool,
        &*llm,
        ctx.channel_id,
        &ctx.model,
        DEFAULT_WINDOW_CHAR_BUDGET,
        None,
    )
    .await?;
    assert_eq!(outcome.result, "ok");

    let actions = fetch_actions(&pool).await?;
    assert!(
        actions.is_empty(),
        "expected no actions from informational message, got {}: {:?}",
        actions.len(),
        actions.iter().map(|a| &a.title).collect::<Vec<_>>()
    );
    Ok(())
}
