//! Bakes the Google OAuth client ids into the engine (see `src/oauth.rs`):
//! from the `MNEMIS_GOOGLE_*` variables, else the builder's `git config`.

use std::process::Command;

const VALUES: [(&str, &str); 3] = [
    (
        "MNEMIS_GOOGLE_DESKTOP_CLIENT_ID",
        "mnemis.googleDesktopClientId",
    ),
    (
        "MNEMIS_GOOGLE_DESKTOP_CLIENT_SECRET",
        "mnemis.googleDesktopClientSecret",
    ),
    ("MNEMIS_GOOGLE_IOS_CLIENT_ID", "mnemis.googleIosClientId"),
];

fn main() {
    if let Some(home) = std::env::var_os("HOME") {
        let gitconfig = std::path::Path::new(&home).join(".gitconfig");
        // A missing file would rerun this on every build.
        if gitconfig.exists() {
            println!("cargo:rerun-if-changed={}", gitconfig.display());
        }
    }
    for (env, git_key) in VALUES {
        println!("cargo:rerun-if-env-changed={env}");
        let value = std::env::var(env)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| {
                let out = Command::new("git")
                    .args(["config", "--get", git_key])
                    .output()
                    .ok()?;
                let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
                (out.status.success() && !value.is_empty()).then_some(value)
            });
        if let Some(value) = value {
            println!("cargo:rustc-env={env}={value}");
        }
    }
}
