use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MailboxState {
    pub last_seen_uid: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub mailboxes: HashMap<String, MailboxState>,
}

pub struct StateStore {
    path: PathBuf,
    state: State,
    /// Max UIDs observed during the current run, per mailbox.
    pending: HashMap<String, u32>,
}

impl StateStore {
    pub async fn load(path: PathBuf) -> Result<Self> {
        let state = match tokio::fs::read_to_string(&path).await {
            Ok(content) => serde_json::from_str(&content)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path,
            state,
            pending: HashMap::new(),
        })
    }

    /// Get the watermark for a mailbox (0 if never seen).
    pub fn watermark(&self, mailbox: &str) -> u32 {
        self.state
            .mailboxes
            .get(mailbox)
            .map(|s| s.last_seen_uid)
            .unwrap_or(0)
    }

    /// Record that UIDs up to `max_uid` were observed for a mailbox during this run.
    pub fn record_seen(&mut self, mailbox: &str, max_uid: u32) {
        let entry = self.pending.entry(mailbox.to_string()).or_insert(0);
        if max_uid > *entry {
            *entry = max_uid;
        }
    }

    /// Commit pending watermarks to state and persist to disk.
    /// Call this after a successful agent run.
    pub async fn commit(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }

        for (mailbox, max_uid) in self.pending.drain() {
            let entry = self.state.mailboxes.entry(mailbox).or_default();
            if max_uid > entry.last_seen_uid {
                entry.last_seen_uid = max_uid;
            }
        }

        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(&self.state)?;
        tokio::fs::write(&self.path, json).await?;
        Ok(())
    }
}
