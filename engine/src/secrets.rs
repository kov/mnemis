//! OS-keychain-backed storage for source / CalDAV credentials.
//!
//! One async API — `store` / `fetch` / `delete`, keyed by an opaque `reference`
//! string — with a per-platform backend selected at compile time:
//!
//! - **macOS:** the **data-protection keychain** (`kSecUseDataProtectionKeychain`)
//!   via `security-framework`'s `PasswordOptions` API, filed under service
//!   `"mnemis"` with the account set to `reference`. Items live in the app's
//!   own access group (derived from the `com.apple.application-identifier`
//!   entitlement), so reads happen with no user prompt. Writes/edits/deletes
//!   are gated behind a single `LAContext` evaluation — Touch ID with device-
//!   password fallback. **Requires a signed `.app` bundle** with the matching
//!   entitlement; running an unsigned binary will fail keychain calls with
//!   `errSecMissingEntitlement (-34018)`.
//! - **everything else (Linux):** the freedesktop Secret Service over D-Bus
//!   (`secret-service`), with attributes `application=mnemis`, `ref=<reference>`.
//!
//! `reference` is what `sources.config_ref` / the CalDAV account record store in
//! SQLite; the secret value itself never touches the database. Both backends
//! treat a delete of a missing entry as success and surface a missing fetch as
//! "no secret stored for ref …", so callers can stay platform-agnostic.

/// Service / application name under which every mnemis secret is filed.
const APPLICATION: &str = "mnemis";

#[cfg(not(target_os = "macos"))]
pub use secret_service_backend::{delete, fetch, store};

#[cfg(target_os = "macos")]
pub use keychain_backend::{delete, fetch, store};

#[cfg(not(target_os = "macos"))]
mod secret_service_backend {
    use super::APPLICATION;
    use anyhow::{Context, Result};
    use secret_service::{EncryptionType, SecretService};
    use std::collections::HashMap;

    pub async fn store(reference: &str, password: &str) -> Result<()> {
        let ss = SecretService::connect(EncryptionType::Dh)
            .await
            .context("connecting to Secret Service")?;
        let collection = ss
            .get_default_collection()
            .await
            .context("opening default keychain collection")?;
        if collection.is_locked().await.unwrap_or(false) {
            collection.unlock().await.ok();
        }
        let attrs: HashMap<&str, &str> =
            HashMap::from([("application", APPLICATION), ("ref", reference)]);
        collection
            .create_item(
                &format!("mnemis: {reference}"),
                attrs,
                password.as_bytes(),
                true,
                "text/plain",
            )
            .await
            .context("storing secret")?;
        Ok(())
    }

    pub async fn fetch(reference: &str) -> Result<String> {
        let ss = SecretService::connect(EncryptionType::Dh)
            .await
            .context("connecting to Secret Service")?;
        let collection = ss
            .get_default_collection()
            .await
            .context("opening default keychain collection")?;
        if collection.is_locked().await.unwrap_or(false) {
            collection.unlock().await.ok();
        }
        let attrs: HashMap<&str, &str> =
            HashMap::from([("application", APPLICATION), ("ref", reference)]);
        let items = collection
            .search_items(attrs)
            .await
            .context("searching secrets")?;
        let item = items
            .first()
            .with_context(|| format!("no secret stored for ref {reference}"))?;
        let bytes = item.get_secret().await.context("reading secret value")?;
        String::from_utf8(bytes).context("secret was not valid UTF-8")
    }

    /// Delete every secret stored under `reference`. A missing entry is not an
    /// error — the post-condition (no such secret) already holds.
    pub async fn delete(reference: &str) -> Result<()> {
        let ss = SecretService::connect(EncryptionType::Dh)
            .await
            .context("connecting to Secret Service")?;
        let collection = ss
            .get_default_collection()
            .await
            .context("opening default keychain collection")?;
        if collection.is_locked().await.unwrap_or(false) {
            collection.unlock().await.ok();
        }
        let attrs: HashMap<&str, &str> =
            HashMap::from([("application", APPLICATION), ("ref", reference)]);
        let items = collection
            .search_items(attrs)
            .await
            .context("searching secrets")?;
        for item in items {
            item.delete().await.context("deleting secret")?;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod keychain_backend {
    use super::APPLICATION;
    use anyhow::{Context, Result};
    use security_framework::passwords::{
        PasswordOptions, delete_generic_password_options, generic_password,
        set_generic_password_options,
    };

    /// `errSecItemNotFound` — returned by Keychain Services when no item matches.
    /// Treated as "already absent" for delete and as a clean not-found for fetch.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    fn options(reference: &str) -> PasswordOptions {
        let mut opts = PasswordOptions::new_generic_password(APPLICATION, reference);
        // Route the item into the data-protection keychain. The app's own
        // access group (from `com.apple.application-identifier`) is implicit,
        // so we don't set one explicitly — that keeps the Rust code free of
        // any team-ID baked-in string.
        opts.use_protected_keychain();
        opts
    }

    pub async fn store(reference: &str, password: &str) -> Result<()> {
        // Biometric gate sits outside the blocking task so it owns the
        // foreground UI thread; the keychain write then runs unattended.
        super::biometric::prompt(&format!("Save credentials for {reference} to mnemis")).await?;

        let reference = reference.to_string();
        let password = password.to_string();
        tokio::task::spawn_blocking(move || {
            let opts = options(&reference);
            set_generic_password_options(password.as_bytes(), opts)
                .map_err(|e| anyhow::anyhow!("storing secret in keychain: {e}"))
        })
        .await
        .context("keychain store task panicked")?
    }

    pub async fn fetch(reference: &str) -> Result<String> {
        let reference = reference.to_string();
        let bytes = tokio::task::spawn_blocking(move || {
            let opts = options(&reference);
            generic_password(opts).map_err(|e| {
                if e.code() == ERR_SEC_ITEM_NOT_FOUND {
                    anyhow::anyhow!("no secret stored for ref {reference}")
                } else {
                    anyhow::anyhow!("reading secret from keychain: {e}")
                }
            })
        })
        .await
        .context("keychain fetch task panicked")??;
        String::from_utf8(bytes).context("secret was not valid UTF-8")
    }

    /// Delete the secret stored under `reference`. A missing entry is not an
    /// error — the post-condition (no such secret) already holds.
    pub async fn delete(reference: &str) -> Result<()> {
        super::biometric::prompt(&format!("Remove credentials for {reference} from mnemis"))
            .await?;

        let reference = reference.to_string();
        tokio::task::spawn_blocking(move || {
            let opts = options(&reference);
            match delete_generic_password_options(opts) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
                Err(e) => Err(anyhow::anyhow!("deleting secret from keychain: {e}")),
            }
        })
        .await
        .context("keychain delete task panicked")?
    }
}

#[cfg(target_os = "macos")]
mod biometric {
    //! Single-prompt biometric gate used to authorise keychain writes.
    //!
    //! Wraps `LAContext.evaluatePolicy(.deviceOwnerAuthentication, …)` —
    //! Touch ID (or Face ID on supported hardware) with a fall-back to the
    //! device password. The completion handler runs on an arbitrary thread,
    //! so we bridge it back via a oneshot channel and `spawn_blocking` so
    //! the async caller stays cancellation-safe.
    use anyhow::{Result, anyhow};
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSError, NSString};
    use objc2_local_authentication::{LAContext, LAError, LAPolicy};
    use std::sync::mpsc;

    pub async fn prompt(reason: &str) -> Result<()> {
        let reason = reason.to_string();
        tokio::task::spawn_blocking(move || prompt_blocking(&reason))
            .await
            .map_err(|e| anyhow!("biometric prompt task panicked: {e}"))?
    }

    fn prompt_blocking(reason: &str) -> Result<()> {
        let context: Retained<LAContext> = unsafe { LAContext::new() };
        let ns_reason = NSString::from_str(reason);

        let (tx, rx) = mpsc::channel::<std::result::Result<(), BiometricError>>();
        let block = RcBlock::new(move |success: Bool, error: *mut NSError| {
            let outcome = if success.as_bool() {
                Ok(())
            } else {
                // Safety: Apple's contract — on failure `error` is a valid
                // autoreleased NSError; on success it is null. We only read
                // it on the failure branch.
                let code = if error.is_null() {
                    0
                } else {
                    unsafe { (*error).code() }
                };
                let message = if error.is_null() {
                    "biometric authentication failed".to_string()
                } else {
                    unsafe { (*error).localizedDescription() }.to_string()
                };
                Err(BiometricError { code, message })
            };
            // If the receiver is gone the caller no longer cares — drop quietly.
            let _ = tx.send(outcome);
        });

        unsafe {
            context.evaluatePolicy_localizedReason_reply(
                LAPolicy::DeviceOwnerAuthentication,
                &ns_reason,
                &block,
            );
        }

        match rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(map_error(e)),
            Err(_) => Err(anyhow!("biometric prompt completion channel closed")),
        }
    }

    struct BiometricError {
        code: isize,
        message: String,
    }

    fn map_error(e: BiometricError) -> anyhow::Error {
        // Translate the common LAError codes into messages the UI can show
        // verbatim. Anything else falls through to whatever the framework
        // produced (locale-aware).
        let user_cancel = LAError::UserCancel.0;
        let app_cancel = LAError::AppCancel.0;
        let user_fallback = LAError::UserFallback.0;
        let auth_failed = LAError::AuthenticationFailed.0;
        let biometry_unavailable = LAError::BiometryNotAvailable.0;
        let biometry_not_enrolled = LAError::BiometryNotEnrolled.0;
        let passcode_not_set = LAError::PasscodeNotSet.0;

        let friendly = match e.code {
            c if c == user_cancel || c == app_cancel => "authentication cancelled",
            c if c == user_fallback => "authentication cancelled (fallback declined)",
            c if c == auth_failed => "authentication failed",
            c if c == biometry_unavailable => {
                "biometric authentication is unavailable on this device"
            }
            c if c == biometry_not_enrolled => {
                "no biometric identities are enrolled on this device"
            }
            c if c == passcode_not_set => "device has no passcode set, cannot authenticate",
            _ => return anyhow!("biometric authentication error ({}): {}", e.code, e.message),
        };
        anyhow!(friendly.to_string())
    }
}
