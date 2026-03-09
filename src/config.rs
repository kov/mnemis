use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub llm: LlmConfig,
    pub imap: ImapConfig,
    #[serde(default)]
    pub paths: PathsConfig,
}

#[derive(Debug, Deserialize)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub bearer_token: Option<String>,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: usize,
}

fn default_max_tool_calls() -> usize {
    200
}

#[derive(Debug, Deserialize)]
pub struct ImapConfig {
    pub server: String,
    #[serde(default = "default_imap_port")]
    pub port: u16,
    pub username: String,
    pub password: String,
}

fn default_imap_port() -> u16 {
    993
}

#[derive(Debug, Deserialize)]
pub struct PathsConfig {
    #[serde(default = "default_memory_dir")]
    pub memory_dir: String,
    #[serde(default = "default_guidance_file")]
    pub guidance_file: String,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            memory_dir: default_memory_dir(),
            guidance_file: default_guidance_file(),
        }
    }
}

fn default_memory_dir() -> String {
    "~/.config/mnemis/memory".to_string()
}

fn default_guidance_file() -> String {
    "~/.config/mnemis/guidance.md".to_string()
}

/// Expand `~` at the start of a path to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    } else if path == "~"
        && let Some(home) = home_dir()
    {
        return home;
    }
    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

impl Config {
    pub async fn load(path: &Path) -> Result<Self> {
        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let config: Config =
            toml::from_str(&content).with_context(|| "failed to parse config file")?;
        Ok(config)
    }

    pub fn memory_dir(&self) -> PathBuf {
        expand_tilde(&self.paths.memory_dir)
    }

    pub fn guidance_file(&self) -> PathBuf {
        expand_tilde(&self.paths.guidance_file)
    }
}
