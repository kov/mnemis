use anyhow::{Context, Result};
use secret_service::{EncryptionType, SecretService};
use std::collections::HashMap;

const APPLICATION: &str = "mnemis";

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
