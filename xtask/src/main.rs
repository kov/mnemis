//! Workspace dev tasks. Run via `cargo xtask <subcommand>`.
//!
//! Subcommands:
//! - `build-macos` — build the UI bundle and a signed macOS `.app` using the
//!   developer's Apple Team ID and signing identity. Reads from `git config`
//!   (`mnemis.appleTeamId`, `mnemis.appleSigningIdentity`) first, falling back
//!   to the `MNEMIS_APPLE_TEAM_ID` / `APPLE_SIGNING_IDENTITY` env vars.
//!
//! Store the values once, globally:
//! ```
//! git config --global mnemis.appleTeamId WDNHP64H9G
//! git config --global mnemis.appleSigningIdentity "Apple Development: gustavo@noronha.eti.br (P3TJZGGJP5)"
//! ```

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("build-macos") => build_macos(&args[1..]),
        Some("run-macos") => run_macos(&args[1..]),
        Some(cmd) => {
            eprintln!("unknown subcommand: {cmd}");
            print_help();
            std::process::exit(2);
        }
        None => {
            print_help();
            Ok(())
        }
    }
}

fn print_help() {
    eprintln!(
        "cargo xtask <subcommand>\n\n\
         Subcommands:\n  \
         build-macos          Bundle a signed mnemis.app for macOS\n  \
         run-macos            build-macos then `open` the resulting .app\n"
    );
}

fn build_macos(_args: &[String]) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("build-macos only runs on macOS");
    }

    let team_id = resolve("mnemis.appleTeamId", "MNEMIS_APPLE_TEAM_ID")?;
    let identity = resolve("mnemis.appleSigningIdentity", "APPLE_SIGNING_IDENTITY")?;

    let root = workspace_root()?;

    // 1. Materialise the entitlements.plist Tauri will pass to codesign. Done
    //    here (not in app/build.rs) so we control when it regenerates — cargo's
    //    rerun-if-env-changed doesn't reliably fire when the env var is set
    //    only on the child cargo process, and we'd silently sign a bundle with
    //    a stale UNSET_TEAM_ID placeholder.
    println!("==> render entitlements.plist (team {team_id})");
    render_entitlements(&root, &team_id)?;

    // 2. UI bundle. Trunk is the standalone build for the wasm frontend.
    println!("==> trunk build (UI)");
    run(Command::new("trunk")
        .arg("build")
        .current_dir(root.join("app/ui")))
    .context("trunk build failed — is `trunk` on PATH?")?;

    // 3. Signed Tauri bundle. `APPLE_SIGNING_IDENTITY` is read by cargo-tauri
    //    to choose the codesign identity.
    println!("==> cargo tauri build (signed .app)");
    run(Command::new("cargo")
        .args(["tauri", "build"])
        .env("APPLE_SIGNING_IDENTITY", &identity)
        .current_dir(&root))
    .context("cargo tauri build failed")?;

    // 4. Embed the Xcode-generated provisioning profile and re-sign. Apple
    //    Development certs (free personal team) require the bundle to carry
    //    an embedded.provisionprofile signed by Apple that authorizes the
    //    `com.apple.application-identifier` entitlement, otherwise the
    //    data-protection keychain returns errSecMissingEntitlement.
    let bundle_id = read_bundle_identifier(&root)?;
    let bundle_path = root.join("target/release/bundle/macos/mnemis.app");
    println!("==> embed provisioning profile for {bundle_id}");
    embed_provisioning_profile(&bundle_path, &bundle_id)?;
    println!("==> re-sign with embedded profile");
    resign_bundle(
        &bundle_path,
        &identity,
        &root.join("app/entitlements.plist"),
    )?;

    Ok(())
}

/// Read `bundle.identifier` from `app/tauri.conf.json`. Kept naive — there's
/// exactly one matching line and it's stable.
fn read_bundle_identifier(root: &std::path::Path) -> Result<String> {
    let conf = root.join("app/tauri.conf.json");
    let text =
        std::fs::read_to_string(&conf).with_context(|| format!("reading {}", conf.display()))?;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("\"identifier\"") {
            let value = rest
                .trim_start_matches([':', ' '])
                .trim_end_matches(',')
                .trim_matches('"');
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    bail!("could not find \"identifier\" in {}", conf.display())
}

/// Locate the newest Xcode-managed provisioning profile whose entitlements
/// authorize `bundle_id`, and copy it into the bundle as
/// `Contents/embedded.provisionprofile`.
fn embed_provisioning_profile(bundle_path: &std::path::Path, bundle_id: &str) -> Result<()> {
    let dir = std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME unset")?
        .join("Library/Developer/Xcode/UserData/Provisioning Profiles");
    if !dir.exists() {
        bail!(
            "no provisioning profiles dir at {} — open the Xcode stub project and \
             build it once to generate one (see CLAUDE.md)",
            dir.display()
        );
    }

    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("provisionprofile") {
            continue;
        }
        // Profiles are CMS-signed plists; `security cms -D` peels the signature.
        let out = Command::new("security")
            .args(["cms", "-D", "-i"])
            .arg(&path)
            .stderr(Stdio::null())
            .output()
            .context("invoking `security cms -D`")?;
        if !out.status.success() {
            continue;
        }
        let plist = String::from_utf8_lossy(&out.stdout);
        // Match on the application-identifier value (`<TEAMID>.<bundle-id>`).
        let needle = format!(".{bundle_id}<");
        if plist.contains(&needle) {
            let mtime = entry.metadata()?.modified()?;
            candidates.push((mtime, path));
        }
    }

    candidates.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    let profile = candidates
        .first()
        .map(|(_, p)| p.clone())
        .with_context(|| {
            format!(
                "no provisioning profile under {} matches bundle id {bundle_id}. \
             Open the Xcode stub project, set its identifier to {bundle_id}, \
             add the Keychain Sharing capability, and Build (⌘B) to register it.",
                dir.display()
            )
        })?;

    let dest = bundle_path.join("Contents/embedded.provisionprofile");
    std::fs::copy(&profile, &dest)
        .with_context(|| format!("copying {} → {}", profile.display(), dest.display()))?;
    println!("    using {}", profile.display());
    Ok(())
}

/// Re-sign the bundle. Tauri already signed it once, but adding
/// `embedded.provisionprofile` invalidates that signature; we have to redo
/// it with the same entitlements file.
fn resign_bundle(
    bundle_path: &std::path::Path,
    identity: &str,
    entitlements: &std::path::Path,
) -> Result<()> {
    let binary = bundle_path.join("Contents/MacOS/mnemis-app");
    // Sign the inner executable first, then the bundle wrapping it. `--force`
    // replaces the previous signature; `--options runtime` matches what Tauri
    // emits by default and is required for notarization later.
    for target in [binary.as_path(), bundle_path] {
        run(Command::new("codesign")
            .arg("--force")
            .arg("--options")
            .arg("runtime")
            .arg("--entitlements")
            .arg(entitlements)
            .arg("--sign")
            .arg(identity)
            .arg(target))
        .with_context(|| format!("codesign failed for {}", target.display()))?;
    }
    Ok(())
}

fn render_entitlements(root: &std::path::Path, team_id: &str) -> Result<()> {
    let template_path = root.join("app/entitlements.plist.template");
    let output_path = root.join("app/entitlements.plist");
    let template = std::fs::read_to_string(&template_path)
        .with_context(|| format!("reading {}", template_path.display()))?;
    let rendered = template.replace("__TEAM_ID__", team_id);
    std::fs::write(&output_path, rendered)
        .with_context(|| format!("writing {}", output_path.display()))?;
    Ok(())
}

fn run_macos(args: &[String]) -> Result<()> {
    build_macos(args)?;
    let root = workspace_root()?;
    let app_path = root.join("target/release/bundle/macos/mnemis.app");
    if !app_path.exists() {
        bail!(
            "expected bundle at {} after build, but it's missing",
            app_path.display()
        );
    }
    println!("==> open {}", app_path.display());
    run(Command::new("open").arg(&app_path))
}

/// Resolve a value by checking `git config --get <key>` first, then the env
/// var. Returns a clear error if neither is set.
fn resolve(git_key: &str, env_key: &str) -> Result<String> {
    if let Some(v) = git_config(git_key)? {
        return Ok(v);
    }
    if let Ok(v) = std::env::var(env_key)
        && !v.is_empty()
    {
        return Ok(v);
    }
    bail!(
        "neither `git config {git_key}` nor ${env_key} is set.\n\
         Run one of:\n  \
         git config --global {git_key} <value>\n  \
         set -x {env_key} <value>"
    )
}

fn git_config(key: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["config", "--get", key])
        .stderr(Stdio::null())
        .output()
        .context("invoking `git config`")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout)
        .context("git config output was not UTF-8")?
        .trim()
        .to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().context("spawning subprocess")?;
    if !status.success() {
        bail!("command exited with {status}");
    }
    Ok(())
}

/// The workspace root — the parent directory of `xtask/`.
fn workspace_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest
        .parent()
        .context("xtask has no parent directory")?
        .to_path_buf())
}
