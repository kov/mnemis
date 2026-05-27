//! Reusable plumbing for driving a real `mnemis-app` instance through
//! `tauri-driver` + `WebKitWebDriver`, with an optional weston-headless
//! compositor in front so the same test can run on a developer desktop
//! (use existing display) or on a CI host with no display at all.
//!
//! Used by:
//!   - `src/bin/ui_probe.rs` (interactive debug; default flow stays attached
//!     to the developer's existing display)
//!   - `tests/ui_smoke.rs` (CI-bound regression test)

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use fantoccini::{Client, ClientBuilder};
use serde_json::{Map, Value};

/// Default port `tauri-driver` listens on. Single test process at a time;
/// if you ever parallelise UI tests, randomise this.
pub const TAURI_DRIVER_PORT: u16 = 4444;

/// JS injected at session start: patches `console.{log,warn,error,info,debug}`
/// and listens for `error` / `unhandledrejection` so we can later read
/// `window.__mnemis_log` and assert on it.
pub const CONSOLE_CAPTURE_JS: &str = r#"
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
"#;

/// Configuration for the harness. Sensible defaults work for both the
/// developer-desktop probe and the CI smoke test.
pub struct HarnessOpts {
    /// Start a `weston --backend=headless-backend.so` instance and route
    /// the app's GDK to it. Default: respects `MNEMIS_TEST_HEADLESS=1`.
    pub headless: bool,
    /// Path to the WebKitWebDriver binary.
    pub webkit_driver: PathBuf,
}

impl Default for HarnessOpts {
    fn default() -> Self {
        Self {
            headless: std::env::var("MNEMIS_TEST_HEADLESS").as_deref() == Ok("1"),
            webkit_driver: PathBuf::from("/usr/bin/WebKitWebDriver"),
        }
    }
}

/// A live UI test environment: optional weston compositor + tauri-driver.
/// Open one or more sessions via [`Self::open_session`]; cleanup is automatic
/// on drop.
pub struct Harness {
    // Held to keep weston alive for the duration of the harness; cleaned
    // up via Drop. We never read it after `start`.
    #[allow(dead_code)]
    weston: Option<WestonGuard>,
    driver: Child,
}

/// Wraps a weston child + its socket name; on drop, kills weston and removes
/// the socket file if it lingers.
struct WestonGuard {
    child: Child,
    socket_name: String,
}

impl Drop for WestonGuard {
    fn drop(&mut self) {
        terminate_process_group(&mut self.child);
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            let p = Path::new(&rt).join(&self.socket_name);
            let _ = std::fs::remove_file(&p);
            let lock = p.with_extension(format!(
                "{}.lock",
                p.extension().and_then(|e| e.to_str()).unwrap_or("")
            ));
            let _ = std::fs::remove_file(lock);
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Kill the entire process group so descendants (WebKitWebDriver,
        // the mnemis-app under test) come down with the driver. Without
        // this, a panicked test leaves orphans that block the next run
        // with "Maximum number of active sessions".
        terminate_process_group(&mut self.driver);
        // weston drops after driver thanks to field order.
    }
}

/// SIGTERM the process group, wait briefly, then SIGKILL anything that
/// didn't exit. Children inherit the process-group id we set at spawn,
/// so this reaches descendants too.
fn terminate_process_group(child: &mut Child) {
    let pgid = child.id() as libc::pid_t;
    unsafe {
        // Negative pid → kill(2) targets the process group.
        libc::kill(-pgid, libc::SIGTERM);
    }
    // Brief grace period for orderly shutdown.
    for _ in 0..20 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

impl Harness {
    /// Bring up the test environment.
    ///
    /// `app_env` is exported into **this process** before tauri-driver
    /// forks, so the env naturally flows down the process tree to the
    /// spawned app (tauri-driver → WebKitWebDriver → mnemis-app). This is
    /// the workaround for tauri-driver 2.0.6 silently dropping any
    /// `tauri:options` field other than `application` and `args`.
    ///
    /// Tests using the harness must be single-threaded (use
    /// `#[tokio::test(flavor = "current_thread")]` and don't share a
    /// process with other env-mutating tests).
    pub async fn start(opts: HarnessOpts, app_env: HashMap<String, String>) -> Result<Self> {
        let weston = if opts.headless {
            Some(spawn_weston().await?)
        } else {
            None
        };

        // Export env BEFORE driver spawn — env on a forked child is frozen
        // at fork time; mutating it in the parent later has no effect.
        for (k, v) in &app_env {
            // SAFETY: documented single-threaded contract.
            unsafe { std::env::set_var(k, v) };
        }
        if let Some(w) = &weston {
            // SAFETY: same.
            unsafe {
                std::env::set_var("WAYLAND_DISPLAY", &w.socket_name);
                std::env::set_var("GDK_BACKEND", "wayland");
                std::env::remove_var("DISPLAY");
            }
        }

        let mut driver_cmd = Command::new("tauri-driver");
        driver_cmd
            .arg("--port")
            .arg(TAURI_DRIVER_PORT.to_string())
            .arg("--native-driver")
            .arg(&opts.webkit_driver)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe and only modifies the child's
        // process group, which is exactly what we want for tree cleanup.
        unsafe {
            driver_cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let driver = driver_cmd.spawn().with_context(|| {
            format!(
                "spawning tauri-driver (is it installed? `cargo install tauri-driver`); \
                     webkit driver path: {}",
                opts.webkit_driver.display()
            )
        })?;

        wait_for_tcp_port(TAURI_DRIVER_PORT, Duration::from_secs(5)).await?;
        Ok(Self { weston, driver })
    }

    /// Open a new WebDriver session pointing at the given Tauri app binary.
    /// Env was already set in [`Self::start`].
    pub async fn open_session(&self, app_bin: &Path) -> Result<Client> {
        let mut tauri_opts = Map::new();
        tauri_opts.insert(
            "application".to_string(),
            Value::String(app_bin.display().to_string()),
        );

        let mut caps = Map::new();
        caps.insert("tauri:options".to_string(), Value::Object(tauri_opts));
        caps.insert("browserName".to_string(), Value::String("wry".to_string()));

        let client = ClientBuilder::native()
            .capabilities(caps)
            .connect(&format!("http://localhost:{TAURI_DRIVER_PORT}"))
            .await
            .map_err(|e| anyhow!("connecting to tauri-driver: {e}"))?;

        // Best-effort: install before any user code logs.
        let _ = client.execute(CONSOLE_CAPTURE_JS, vec![]).await;

        Ok(client)
    }
}

async fn spawn_weston() -> Result<WestonGuard> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR not set; needed for weston socket"))?;
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let socket_name = format!("mnemis-test-{pid}-{nanos}");

    let mut cmd = Command::new("weston");
    cmd.arg("--backend=headless-backend.so")
        .arg(format!("--socket={socket_name}"))
        .arg("--width=1280")
        .arg("--height=720")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| "spawning weston (install: `dnf install weston`)")?;

    let socket = Path::new(&rt).join(&socket_name);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if socket.exists() {
            return Ok(WestonGuard { child, socket_name });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "weston did not create socket {} within 5s",
        socket.display()
    )
}

async fn wait_for_tcp_port(port: u16, timeout: Duration) -> Result<()> {
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
    bail!("timed out waiting for port {port} to accept connections")
}

/// Locate the built `mnemis-app` binary that lives next to the calling test
/// or probe binary. Works for both `target/debug/` (cargo test/run) and
/// release profiles.
pub fn sibling_app_binary() -> Result<PathBuf> {
    // Cargo writes integration test binaries to `target/{profile}/deps/<name>`
    // and the main binary to `target/{profile}/<name>`. Walk up until we
    // find a sibling `mnemis-app`.
    let exe = std::env::current_exe()?;
    for ancestor in exe.ancestors().skip(1).take(4) {
        let candidate = ancestor.join("mnemis-app");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!(
        "could not find mnemis-app binary near {} — did you `cargo build -p mnemis-app`?",
        exe.display()
    )
}
