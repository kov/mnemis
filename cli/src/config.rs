use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub llm: LlmSection,
    #[serde(default)]
    pub paths: PathsSection,
}

#[derive(Debug, Deserialize)]
pub struct LlmSection {
    pub base_url: String,
    pub chat_model: String,
    pub embedding_model: String,
    #[serde(default)]
    pub bearer_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PathsSection {
    #[serde(default)]
    pub db: Option<String>,
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.paths
            .db
            .as_deref()
            .map(expand_tilde)
            .unwrap_or_else(default_db_path)
    }
}

pub fn load(override_path: Option<&Path>) -> Result<Config> {
    let path = override_path
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| "parsing config TOML")?;
    Ok(cfg)
}

pub fn default_config_path() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        return home.join(".config/mnemis/config.toml");
    }
    PathBuf::from("config.toml")
}

pub fn default_db_path() -> PathBuf {
    if let Some(data) = dirs::data_dir() {
        return data.join("mnemis/mnemis.db");
    }
    PathBuf::from("mnemis.db")
}

pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}
