use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub llm: LlmSection,
    #[serde(default)]
    pub paths: PathsSection,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LlmSection {
    pub base_url: String,
    pub chat_model: String,
    pub embedding_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// The server's context window, in tokens. The extractor derives its
    /// per-batch message-window budget from this (reserving headroom for the
    /// prompt scaffolding and the agent loop's multi-turn growth) and splits
    /// larger windows into sequential batches so no single call overflows the
    /// server. `None` falls back to [`crate::extract::DEFAULT_MAX_CONTEXT_TOKENS`].
    /// Set it to whatever your omlx/model is actually configured for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<usize>,
    /// Total per-request timeout for chat/extraction LLM calls, in seconds.
    /// A degraded server can accept a request and never answer; without this
    /// the sync hangs forever. `None` falls back to
    /// [`crate::llm::DEFAULT_REQUEST_TIMEOUT_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_secs: Option<u64>,
}

impl LlmSection {
    /// Resolve the configured server context window, falling back to the
    /// engine default when unset.
    pub fn resolved_max_context_tokens(&self) -> usize {
        self.max_context_tokens
            .unwrap_or(crate::extract::DEFAULT_MAX_CONTEXT_TOKENS)
    }

    /// Resolve the configured request timeout, falling back to the engine
    /// default when unset.
    pub fn resolved_request_timeout_secs(&self) -> u64 {
        self.request_timeout_secs
            .unwrap_or(crate::llm::DEFAULT_REQUEST_TIMEOUT_SECS)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PathsSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

    /// `<db_parent>/traces/`. Per-run JSONL trace files live here. Caller
    /// is responsible for `create_dir_all` (TraceWriter does it).
    pub fn traces_dir(&self) -> PathBuf {
        traces_dir_for(&self.db_path())
    }
}

/// Derive a traces directory next to a given DB path. Pulled out as a free
/// function so the Tauri app — which resolves the DB path via env, not
/// `Config::load` — can compute it the same way.
pub fn traces_dir_for(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(|p| p.join("traces"))
        .unwrap_or_else(|| PathBuf::from("traces"))
}

/// Write the LLM section to the configured `config.toml`, preserving the
/// `[paths]` section if one was present. Creates parent directories as
/// needed; if the file didn't exist, an empty `[paths]` is omitted so the
/// resulting file is the minimum we can read back.
pub fn save_llm(llm: &LlmSection) -> Result<()> {
    let path = default_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    // Preserve sections the caller didn't supply: the `[paths]` block and
    // max_context_tokens, both of which are config-file-only knobs the LLM
    // settings form never touches. Without this, saving from the UI would
    // silently wipe a hand-edited max_context_tokens.
    let (paths, existing_max_ctx, existing_timeout) = match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(c) => (
                c.paths,
                c.llm.max_context_tokens,
                c.llm.request_timeout_secs,
            ),
            Err(_) => (PathsSection::default(), None, None),
        },
        Err(_) => (PathsSection::default(), None, None),
    };
    let cfg = Config {
        llm: LlmSection {
            base_url: llm.base_url.clone(),
            chat_model: llm.chat_model.clone(),
            embedding_model: llm.embedding_model.clone(),
            bearer_token: llm.bearer_token.clone(),
            max_context_tokens: llm.max_context_tokens.or(existing_max_ctx),
            request_timeout_secs: llm.request_timeout_secs.or(existing_timeout),
        },
        paths,
    };
    let text = toml::to_string_pretty(&cfg).context("serializing config")?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
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

/// Where to look for `config.toml` when no explicit override is passed.
///
/// `MNEMIS_CONFIG_PATH` wins so tests (and one-off setups) can point at a
/// temp file without writing to `~/.config/mnemis/`. Falls back to the
/// XDG-style default.
pub fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("MNEMIS_CONFIG_PATH") {
        return PathBuf::from(p);
    }
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
