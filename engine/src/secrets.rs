//! OS-keychain-backed storage for source / CalDAV credentials.
//!
//! One async API — `store` / `fetch` / `delete`, keyed by an opaque `reference`
//! string — with a per-platform backend selected at compile time:
//!
//! - **macOS:** Keychain Services generic-password items (`security-framework`),
//!   filed under service `"mnemis"` with the account set to `reference`.
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
        delete_generic_password, get_generic_password, set_generic_password,
    };

    /// `errSecItemNotFound` — returned by Keychain Services when no item matches.
    /// Treated as "already absent" for delete and as a clean not-found for fetch.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    // Keychain Services calls are synchronous and may block (e.g. an ACL prompt
    // when the keychain is locked), so each runs on the blocking pool to keep the
    // async runtime responsive. `set_generic_password` creates or updates the
    // item, matching the Secret Service backend's overwrite semantics.

    pub async fn store(reference: &str, password: &str) -> Result<()> {
        let service = APPLICATION.to_string();
        let account = reference.to_string();
        let password = password.to_string();
        tokio::task::spawn_blocking(move || {
            set_generic_password(&service, &account, password.as_bytes())
                .map_err(|e| anyhow::anyhow!("storing secret in keychain: {e}"))
        })
        .await
        .context("keychain store task panicked")?
    }

    pub async fn fetch(reference: &str) -> Result<String> {
        let service = APPLICATION.to_string();
        let account = reference.to_string();
        let bytes = tokio::task::spawn_blocking(move || {
            get_generic_password(&service, &account).map_err(|e| {
                if e.code() == ERR_SEC_ITEM_NOT_FOUND {
                    anyhow::anyhow!("no secret stored for ref {account}")
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
        let service = APPLICATION.to_string();
        let account = reference.to_string();
        tokio::task::spawn_blocking(move || match delete_generic_password(&service, &account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(anyhow::anyhow!("deleting secret from keychain: {e}")),
        })
        .await
        .context("keychain delete task panicked")?
    }
}
