# mnemis project notes

Tauri desktop app (in progress). Ingests email/chat, extracts action items via a local LLM (omlx), tracks them, syncs to calendar. See the auto-memory at `/home/kov/.claude/projects/-home-kov-Projects-mnemis/memory/` for design rationale and gotchas — don't re-derive them.

## Workspace layout

```
mnemis/
  Cargo.toml          # workspace: engine, app, cli, types
  engine/             # mnemis-engine (lib): db, sources, ingest, embed, extract, queries
  types/              # mnemis-types (lib): pure-serde DTOs shared with the wasm frontend
  cli/                # mnemis-cli (bin): one-shot ops against the same SQLite
  app/                # mnemis-app (Tauri bin) + ui-probe (debug bin)
    src/main.rs       # Tauri setup + commands
    tauri.conf.json
    capabilities/default.json
    icons/icon.png
    ui/               # standalone workspace (wasm32 target) — Leptos frontend
      Cargo.toml      # has its own [workspace] table
      Trunk.toml
      index.html
      styles.css
      src/{main.rs,components.rs}
  attic/v1/           # old CLI, kept for reference, excluded from workspace
```

## Build & run

From the project root (`/home/kov/Projects/mnemis`):

```
# Engine + CLI
cargo build                          # builds engine, cli, types
cargo test                           # all unit + integration tests (mock LLM)
MNEMIS_TEST_LLM=live \
  MNEMIS_TEST_LLM_URL=http://alface:1234/v1 \
  MNEMIS_TEST_LLM_MODEL=qwen3.5-35b-a3b-4bit \
  cargo test --test extract          # live LLM mode

# Frontend (wasm bundle)
(cd app/ui && trunk build)           # writes app/ui/dist/

# Tauri app
cargo build -p mnemis-app
MNEMIS_DB_PATH=/path/to/mnemis.db ./target/debug/mnemis-app
# MNEMIS_CONFIG_PATH=/path/to/config.toml works the same way for the
# config file (otherwise defaults to ~/.config/mnemis/config.toml).
```

### macOS: signed bundle (required for the keychain)

The keychain backend uses the **data-protection keychain** with the
`com.apple.application-identifier` entitlement. The app reads its own secrets
without prompting; writes/edits are gated by a single Touch ID (or device
password) prompt. The data-protection keychain refuses unentitled processes
with `errSecMissingEntitlement (-34018)`, so a plain `cargo run` binary
**cannot** read or write secrets — only the signed `.app` bundle can.

One-time setup: register your team ID and signing identity in git config so
they're picked up automatically by the xtask wrapper.

```
git config --global mnemis.appleTeamId WDNHP64H9G
git config --global mnemis.appleSigningIdentity \
    "Apple Development: gustavo@noronha.eti.br (P3TJZGGJP5)"
```

Then any time you want a signed bundle:

```
cargo xtask build-macos                 # trunk build + cargo tauri build
cargo xtask run-macos                   # same, then `open` the .app
```

The xtask reads the two git-config values (falling back to
`MNEMIS_APPLE_TEAM_ID` / `APPLE_SIGNING_IDENTITY` env vars), runs the UI
bundle, then invokes `cargo tauri build` with the env vars exported.
`app/build.rs` reads `MNEMIS_APPLE_TEAM_ID` and materialises
`app/entitlements.plist` from `entitlements.plist.template`; Tauri passes that
file to `codesign` along with the identity. The generated `entitlements.plist`
is git-ignored. Without those values set, the build emits a warning and
produces a bundle that runs but cannot use the data-protection keychain.

The signing identity is the exact string `security find-identity -v -p
codesigning` prints in quotes; the team ID is the OU field on the cert
(`security find-certificate -c "Apple Development: <your-email>" -p |
openssl x509 -noout -subject` — usually visible in Xcode → Settings →
Accounts → Team).

Verify after signing:

```
codesign -d --entitlements - ./target/release/bundle/macos/mnemis.app
# Should print the application-identifier entry above.
security find-generic-password -s mnemis -g     # also shows items
```

The `mnemis-cli` binary is **not** signed by this flow, so it cannot read or
write secrets on macOS. CLI-driven credential management is unsupported on
the Mac for now — use the app's settings UI.

## UI debugging & integration tests

The Tauri webview can't be inspected the way regular Rust code can. Two complementary tools, both gated behind the `ui-probe` feature so default builds stay small:

### `ui-probe` (interactive)

```
cargo build -p mnemis-app
cargo build -p mnemis-app --bin ui-probe --features ui-probe
MNEMIS_DB_PATH=/path/to/mnemis.db ./target/debug/ui-probe [--keep-open] [--headless]
```

Spawns `tauri-driver` + `WebKitWebDriver`, drives the app via fantoccini, dumps body text + innerHTML + captured `console.*`/`window.error` events to stdout. `--keep-open` leaves the session up for poking; `--headless` (or `MNEMIS_TEST_HEADLESS=1`) routes through a private weston compositor.

### `ui_smoke` integration test

```
cargo build -p mnemis-app
cargo test  -p mnemis-app --features ui-probe --test ui_smoke -- --nocapture
# Or, no display required:
MNEMIS_TEST_HEADLESS=1 cargo test -p mnemis-app --features ui-probe \
    --test ui_smoke -- --nocapture
```

Seeds a tempdir SQLite, brings up the harness, asserts what the user would see. Follows project [testing-tenets](file:///home/kov/.claude/projects/-home-kov-Projects-mnemis/memory/testing_tenets.md): outcomes only, loose `contains`-checks.

### Headless choice

We use `weston --backend=headless-backend.so` instead of Xvfb. Wayland is what users actually run today (GNOME/KDE both default to it), GTK 3 + webkit2gtk + WebKitWebDriver all support it natively, and we don't pay for an X11 compatibility layer. Mesa's llvmpipe fallback handles software rendering when there's no GPU. If you ever need to confirm: `LIBGL_DEBUG=1 MNEMIS_TEST_HEADLESS=1 cargo test ...` shows which renderer was selected.

### Process cleanup

The harness uses process groups (`setsid` at spawn, `kill(-pgid, ...)` at drop) so descendants come down with the driver. If a panic somehow leaks orphans anyway: `pgrep -af 'tauri-driver|WebKitWebDriver|mnemis-app'` to spot them, then `kill <pids>`. Never use `pkill -f weston` blindly — your real desktop session is probably also a weston process.

## Hygiene

- `.githooks/pre-commit` runs `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`, plus separate `--manifest-path app/ui/Cargo.toml` fmt + clippy passes for the standalone UI workspace (clippy on `wasm32-unknown-unknown`, so `rustup target add wasm32-unknown-unknown` is required). Active via `git config core.hooksPath .githooks`.
- For clippy with the `ui-probe` feature: `cargo clippy -p mnemis-app --bin ui-probe --features ui-probe -- -D warnings` (the feature is mnemis-app-only).
- `app/ui` is a standalone workspace — its deps don't pollute the host build graph. Don't `cd` into it casually; if you do, `cd -` afterwards or use `--manifest-path`. Format it with `cargo fmt --manifest-path app/ui/Cargo.toml`.

## Platform notes

- **macOS** is the tray-resident primary target. `app/src/tray.rs` (cfg-gated) installs a tray icon + "Show window / Sync now / Quit" menu; closing the main window hides it instead of exiting (Cmd+Q or tray Quit actually exits).
- **Linux** runs as a regular window today — tray support (runtime detection of `org.kde.StatusNotifierWatcher` via zbus) is **Phase 7** per `v2-redesign` memory. Closing the window quits as usual.
- Single-instance is wired everywhere via `tauri-plugin-single-instance`: a second launch focuses the existing window.

## Pointers (don't duplicate here)

- Design rationale → memory `v2-redesign`
- Engine/LLM crate quirks → memory `v1-gotchas`
- Tauri/Leptos/Trunk quirks → memory `v2-app-gotchas`
- Test approach → memory `testing-tenets`
