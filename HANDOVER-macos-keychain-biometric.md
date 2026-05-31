# Handover: improve macOS keychain UX (stop the repeated password prompts)

**Status:** investigation done on Linux; implementation + testing must happen on
the Mac (the keychain code is `cfg(target_os = "macos")` and the data-protection
keychain can only be exercised in a signed app bundle on macOS).

**Transient doc** — delete it once the work lands. It exists so a Mac-side
session can pick this up with full context instead of re-deriving it.

---

## 1. What the user observed

On macOS, running a sync prompts for the **login keychain password several
times** ("mnemis wants to use your confidential information stored in '…' in
your keychain"), once per stored credential, **every sync**. The user expected
the modern behavior: a single **Touch ID** ("allow with fingerprint") prompt,
ideally "allow once" for the whole batch.

## 2. Current implementation (committed)

The secrets module is `engine/src/secrets.rs`, cfg-gated:

- **Linux:** freedesktop Secret Service over D-Bus (`secret-service`).
- **macOS:** `security_framework::passwords::{set,get,delete}_generic_password`
  (commit `8016d03`).

Public API (unchanged, async): `store(reference, password)`,
`fetch(reference) -> String`, `delete(reference)`. Keychain items are filed
under service `"mnemis"`, account = `reference`. References are deterministic:

- IMAP: `imap/{username}@{server}` (`engine/src/settings.rs:404` stores it)
- CalDAV: `caldav/{username}@{host}` (`engine/src/settings.rs:501` stores it)

### Where the keychain is read (the prompt sites)

Every one of these calls `secrets::fetch`, and each fetch is a separate keychain
item access → a separate prompt:

| Call site | When | Frequency |
|---|---|---|
| `orchestrator::build_imap_source` (`engine/src/orchestrator.rs:383`) | inside `sync_one_source` (`orchestrator.rs:188`) | **once per IMAP source, per sync** |
| `orchestrator::sync_calendar_if_configured` (`engine/src/orchestrator.rs:412`) | once per `sync_now` (`orchestrator.rs:133`) | **once per sync (if CalDAV set up)** |
| `list_caldav_collections` (`app/src/main.rs:216`) | UI "discover calendars" probe | per discover click |
| `build_imap_source` via CLI (`cli/src/commands.rs:105`) | `cli sync` | per CLI sync |

So one sync = (number of IMAP sources) + (1 if CalDAV) prompts, **repeated on
every hourly background sync**. That is the core annoyance.

### Where secrets are written (for cache invalidation, see Tier 1)

`secrets::store`: `settings.rs:404` (IMAP), `settings.rs:501` (CalDAV),
`cli/src/commands.rs:67`. `secrets::delete`: `settings.rs:543` (CalDAV forget).

## 3. Root cause (verified from the crate source, not assumed)

Two independent problems stack up:

1. **We use the legacy login keychain.** `passwords::set_generic_password` builds
   a `PasswordOptions` with **no** `use_protected_keychain()` and **no** access
   control, so items go to the file-based login keychain with classic ACLs. That
   ACL dialog is the "type your keychain password" prompt — the "old style"
   behavior. The modern Touch-ID flow comes from the **data-protection
   keychain** + an access-control object, which we don't set.

2. **One item read per credential, re-read every sync, with an unstable dev
   signature.** Even on the legacy keychain, "Always Allow" *should* pin the app
   into the item's ACL so it stops prompting — but that pin is keyed to the
   app's **code signature**, and an unsigned/ad-hoc `cargo build`/`tauri dev`
   binary changes identity every build, so the pin never holds and it re-prompts.

## 4. Improvement tiers (do Tier 1 regardless; evaluate 2 & 3 on the Mac)

### Tier 1 — in-process secret cache (cross-platform, safe, no entitlements) ★ do first

Fetch each secret **once per app run** and cache it in memory; reuse on
subsequent syncs. This collapses "prompt every sync" → "prompt at most once per
secret per app launch." It does **not** remove the first-launch prompts, but it
kills the recurring hourly ones, which is the worst part.

Sketch (all in `engine/src/secrets.rs`, so both backends benefit and there's one
chokepoint):

```rust
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn cache() -> &'static Mutex<HashMap<String, String>> {
    static C: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

// fetch(): check cache first; on miss, hit the backend, then insert.
// store(): write-through — update the cache entry after a successful store.
// delete(): evict the cache entry after a successful delete.
```

Notes:
- Wrap the existing per-platform `store`/`fetch`/`delete` as `*_backend` and put
  the cache in the public wrappers, so the cache logic isn't duplicated per OS.
- Trade-off: secrets stay resident in process memory for the app's lifetime.
  They already transit memory each sync; this just keeps them longer. Acceptable
  for a single-user desktop app. Optional hardening: store in a `zeroize`-ing
  wrapper, or cap with a TTL. Don't over-engineer first pass.
- **Verify** on the Mac: first sync prompts N times (one per credential), every
  subsequent sync in the same run prompts **zero** times. That alone likely
  satisfies the complaint for normal use.

### Tier 2 — data-protection keychain + biometrics (the real "one fingerprint" UX) — macOS only

Gives a Touch ID prompt (with "allow once" / reusable auth) instead of the
password dialog. Same crate, lower-level than the `passwords` convenience fns:

- `security_framework::passwords_options::PasswordOptions`
  - `.use_protected_keychain()`  → `kSecUseDataProtectionKeychain = true`
  - `.set_access_control_options(AccessControlOptions::…)`  (user-presence /
    biometry flags) **or** `.set_access_control(SecAccessControl)` for full
    control via `security_framework::access_control::SecAccessControl`
  - `.set_access_group("…")` if an access group is needed
- Store with `passwords_options::set_generic_password_options(pw, opts)`; for
  read, use the `item` module search with `use_protected_keychain` set.
- **Single prompt for a batch** needs an `LAContext` reused across reads
  (`kSecUseAuthenticationContext` + `touchIDAuthenticationAllowableReuseDuration`).
  `security-framework` may not expose `kSecUseAuthenticationContext` directly —
  expect to drop to `security-framework-sys` / `core-foundation` for that piece.
  If LAContext reuse is too fiddly, Tier 1's cache already gives "read once,"
  so Tier 2 can stay "one Touch ID per credential on first read."

**Hard requirements / landmines (verify on the Mac):**
- The data-protection keychain refuses **unentitled** apps with
  `errSecMissingEntitlement (-34018)`. The app must be a **signed `.app` bundle**
  with the `keychain-access-groups` entitlement (+ an application-identifier).
  Configure via Tauri bundling + a `*.entitlements` file; `tauri dev` unsigned
  builds will likely fail here — test with a signed `tauri build` bundle.
- **Migration:** items already written by Tier-0 `set_generic_password` live in
  the *login* keychain; moving to the data-protection keychain means the user
  re-enters credentials once (simplest) or we migrate them. Re-enter is fine.

### Tier 3 — stable dev code signing (lightest fix for the dev loop)

If the dev build has a **stable** signing identity, clicking **"Always Allow"**
once pins mnemis into each item's ACL and the legacy prompts stop across
rebuilds — no code change, just a Tauri `signingIdentity` / `codesign` config.
Good stopgap while Tier 2 is built; also needed for distribution anyway.

## 5. Recommended plan for the Mac session

1. **Tier 1 cache** first — safe, cross-platform, removes the recurring prompts.
   Land it, confirm "subsequent syncs: 0 prompts."
2. Decide if a single **Touch ID** prompt is still wanted. If yes, it's coupled
   to signing/entitlements work (Tier 2) you'll want for distribution regardless.
3. Set up signed bundling + entitlements (Tier 3 gets you "Always Allow" pinning
   for free; Tier 2 builds on the same signing to unlock the data-protection
   keychain).
4. Implement Tier 2 against the signed bundle; verify in **Keychain Access.app**
   that items now appear in the data-protection keychain with an access-control
   rule, and that access shows the Touch ID sheet.

## 6. Verification tips

- **Build/run on the Mac:** `cargo build -p mnemis-app`; run with
  `MNEMIS_DB_PATH=… ./target/debug/mnemis-app`. For entitlement testing you need
  a signed bundle: `cargo tauri build` (or the project's bundling command) + a
  `.entitlements` with `keychain-access-groups`.
- **Inspect:** open **Keychain Access.app** → search "mnemis" to see which
  keychain the items live in and their ACLs. `security find-generic-password -s
  mnemis` from Terminal also works for the login keychain.
- **Count prompts:** with Tier 1, instrument `secrets::fetch` with a
  `tracing::debug!` on cache miss vs hit to confirm the backend is hit once.
- **Type-checking macOS-only code from Linux (if you bounce back here):** the
  full `engine` can't cross-check (`libsqlite3-sys` needs a darwin C
  cross-compiler), but you can isolate the keychain module into a throwaway crate
  depending only on `security-framework` and run
  `cargo check --target aarch64-apple-darwin` (pure FFI, no Mac needed). This was
  how `8016d03` was verified. See memory `v2-app-gotchas` →
  "Verifying macOS-only code from a Linux host."

## 7. Pointers

- Code: `engine/src/secrets.rs` (backends), fetch sites in
  `engine/src/orchestrator.rs`, store/delete in `engine/src/settings.rs`.
- Memory: `v2-redesign` (Phase 5 / secrets storage decision, line ~263),
  `v2-app-gotchas` (macOS cross-target verification), `verify-before-concluding`
  (don't assert macOS behavior without testing it on the Mac).
- Crate: `security-framework` 3.7.0 — modules `passwords`, `passwords_options`,
  `access_control`, `item`. Docs: https://docs.rs/security-framework/3.7.0/
