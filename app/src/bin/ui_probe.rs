//! Headed UI probe that drives the running app through `tauri-driver` +
//! `WebKitWebDriver` and dumps the rendered DOM + text so we can see what
//! the user sees without asking them.
//!
//! Usage:
//!   cargo build -p mnemis-app                                 # ensure binary up to date
//!   MNEMIS_DB_PATH=... cargo run -p mnemis-app \
//!       --bin ui-probe --features ui-probe -- [--keep-open]
//!
//! Default behavior: spawn tauri-driver, open a session against the just-built
//! `mnemis-app` binary, wait for the actions list (or empty/error) to settle,
//! print page source + visible text + a small log of captured console output,
//! then exit. With `--keep-open` the session lingers and you can attach a
//! browser dev-tools yourself.

use std::process::{Command, Stdio};
use std::time::Duration;

use fantoccini::{ClientBuilder, Locator};
use serde_json::{Map, Value, json};

const TAURI_DRIVER_PORT: u16 = 4444;
const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let keep_open = std::env::args().any(|a| a == "--keep-open");

    // sibling binary in the same target/{profile}/ dir.
    let app_bin = std::env::current_exe()?
        .parent()
        .map(|d| d.join("mnemis-app"))
        .ok_or_else(|| anyhow::anyhow!("could not resolve mnemis-app binary path"))?;
    if !app_bin.exists() {
        anyhow::bail!(
            "expected app binary at {}; run `cargo build -p mnemis-app` first",
            app_bin.display()
        );
    }
    println!("== driving app: {} ==", app_bin.display());

    let mut driver = Command::new("tauri-driver")
        .arg("--port")
        .arg(TAURI_DRIVER_PORT.to_string())
        .arg("--native-driver")
        .arg("/usr/bin/WebKitWebDriver")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn tauri-driver: {e}"))?;

    // tauri-driver takes a moment to bind. Poll the port.
    wait_for_port(TAURI_DRIVER_PORT, Duration::from_secs(5)).await?;

    let result = run_probe(&app_bin, keep_open).await;

    // Always kill the driver before reporting result.
    let _ = driver.kill();
    let _ = driver.wait();

    result
}

async fn run_probe(app_bin: &std::path::Path, keep_open: bool) -> anyhow::Result<()> {
    let mut caps = Map::new();
    let mut tauri_opts = Map::new();
    tauri_opts.insert(
        "application".to_string(),
        Value::String(app_bin.display().to_string()),
    );
    if let Ok(db) = std::env::var("MNEMIS_DB_PATH") {
        tauri_opts.insert(
            "env".to_string(),
            json!({
                "MNEMIS_DB_PATH": db,
                "RUST_LOG": std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            }),
        );
    }
    caps.insert("tauri:options".to_string(), Value::Object(tauri_opts));
    caps.insert("browserName".to_string(), Value::String("wry".to_string()));

    let client = ClientBuilder::native()
        .capabilities(caps)
        .connect(&format!("http://localhost:{TAURI_DRIVER_PORT}"))
        .await
        .map_err(|e| anyhow::anyhow!("connecting to tauri-driver: {e}"))?;

    println!("== session up, installing console capture ==");
    // Patch console.{log,warn,error} to push into a window-level buffer we can
    // read later. Must run before any user code logs — Suspense/LocalResource
    // kicks off async work the moment the JS bundle runs, so race exists, but
    // we usually beat the first log lines.
    let _ = client
        .execute(
            r#"
            window.__mnemis_log = window.__mnemis_log || [];
            ['log','warn','error','info','debug'].forEach((k) => {
                const orig = console[k];
                console[k] = function (...args) {
                    try {
                        window.__mnemis_log.push({
                            kind: k,
                            ts: Date.now(),
                            text: args.map(String).join(' '),
                        });
                    } catch (e) {}
                    orig.apply(console, args);
                };
            });
            window.addEventListener('error', (e) => {
                window.__mnemis_log.push({ kind: 'window.error', ts: Date.now(),
                    text: (e && e.message) ? e.message : String(e) });
            });
            window.addEventListener('unhandledrejection', (e) => {
                window.__mnemis_log.push({ kind: 'unhandled', ts: Date.now(),
                    text: (e && e.reason) ? String(e.reason) : 'unknown' });
            });
            "#,
            vec![],
        )
        .await;

    println!("== waiting for app shell ==");
    client
        .wait()
        .at_most(SETTLE_TIMEOUT)
        .for_element(Locator::Css("div.app"))
        .await
        .map_err(|e| anyhow::anyhow!("waiting for div.app: {e}"))?;

    // Give Suspense one extra beat to resolve. Faster than polling for an
    // ".action" element that may legitimately never appear (empty list).
    tokio::time::sleep(Duration::from_millis(800)).await;

    println!("\n== visible text ==");
    let text = client
        .find(Locator::Css("body"))
        .await?
        .text()
        .await
        .unwrap_or_else(|_| "(no body text)".to_string());
    println!("{text}");

    println!("\n== body innerHTML ==");
    let html = client
        .find(Locator::Css("body"))
        .await?
        .html(true)
        .await
        .unwrap_or_else(|_| "(no html)".to_string());
    println!("{html}");

    println!("\n== captured console / errors ==");
    match client
        .execute("return JSON.stringify(window.__mnemis_log || []);", vec![])
        .await
    {
        Ok(v) => match v.as_str() {
            Some(s) => {
                let parsed: Result<Value, _> = serde_json::from_str(s);
                match parsed {
                    Ok(Value::Array(arr)) if arr.is_empty() => println!("(none)"),
                    Ok(Value::Array(arr)) => {
                        for entry in arr {
                            println!("  {entry}");
                        }
                    }
                    Ok(other) => println!("(unexpected log shape: {other})"),
                    Err(e) => println!("(could not parse log JSON: {e}; raw={s})"),
                }
            }
            None => println!("(execute returned non-string: {v})"),
        },
        Err(e) => println!("(could not read window.__mnemis_log: {e})"),
    }

    if keep_open {
        println!("\n== --keep-open: session staying alive; ctrl-c to exit ==");
        tokio::signal::ctrl_c().await.ok();
    }

    client.close().await.ok();
    Ok(())
}

async fn wait_for_port(port: u16, timeout: Duration) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    anyhow::bail!("timed out waiting for tauri-driver to bind on {port}")
}
