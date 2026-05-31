//! Headed UI probe that drives the running app through the shared
//! `test_support` harness and dumps the rendered DOM + text + captured
//! console output so we can see what the user sees without asking them.
//!
//! Usage:
//!   cargo build -p mnemis-app
//!   MNEMIS_DB_PATH=... cargo run -p mnemis-app \
//!       --bin ui-probe --features ui-probe -- [--keep-open] [--headless]
//!
//! `--headless` (or `MNEMIS_TEST_HEADLESS=1`) routes through a private
//! weston compositor — useful for "does this still work without my display
//! manager?" smoke checks. Default attaches to whatever Wayland/X session
//! the developer is already in.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use fantoccini::Locator;
use mnemis_app::test_support::{Harness, HarnessOpts, sibling_app_binary};
use serde_json::Value;

const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let keep_open = args.iter().any(|a| a == "--keep-open");
    let headless = args.iter().any(|a| a == "--headless");

    let app_bin = sibling_app_binary()?;
    println!("== driving app: {} ==", app_bin.display());

    let opts = HarnessOpts {
        headless: headless || HarnessOpts::default().headless,
        ..HarnessOpts::default()
    };

    let mut env = HashMap::new();
    if let Ok(db) = std::env::var("MNEMIS_DB_PATH") {
        env.insert("MNEMIS_DB_PATH".to_string(), db);
    }
    if let Ok(rl) = std::env::var("RUST_LOG") {
        env.insert("RUST_LOG".to_string(), rl);
    }
    let harness = Harness::start(opts, env).await?;
    let client = harness.open_session(&app_bin).await?;
    println!("== session up, waiting for shell ==");

    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.app"))
        .await?;
    // Suspense beat.
    tokio::time::sleep(Duration::from_millis(800)).await;

    println!("\n== visible text ==");
    println!(
        "{}",
        client
            .find(Locator::Css("body"))
            .await?
            .text()
            .await
            .unwrap_or_else(|_| "(no body text)".to_string())
    );

    println!("\n== body innerHTML ==");
    println!(
        "{}",
        client
            .find(Locator::Css("body"))
            .await?
            .html(true)
            .await
            .unwrap_or_else(|_| "(no html)".to_string())
    );

    println!("\n== captured console / errors ==");
    let raw = client
        .execute("return JSON.stringify(window.__mnemis_log || []);", vec![])
        .await?;
    match raw.as_str() {
        Some(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(arr)) if arr.is_empty() => println!("(none)"),
            Ok(Value::Array(arr)) => {
                for entry in arr {
                    println!("  {entry}");
                }
            }
            Ok(other) => println!("(unexpected log shape: {other})"),
            Err(e) => println!("(could not parse log JSON: {e}; raw={s})"),
        },
        None => println!("(execute returned non-string: {raw})"),
    }

    if keep_open {
        println!("\n== --keep-open: session staying alive; ctrl-c to exit ==");
        tokio::signal::ctrl_c().await.ok();
    }

    client.close().await.ok();
    Ok(())
}
