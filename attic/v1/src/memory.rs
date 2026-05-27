use anyhow::{Result, bail};
use std::path::PathBuf;

pub struct MemoryStore {
    dir: PathBuf,
}

#[derive(Debug)]
pub struct SearchHit {
    pub key: String,
    pub line_number: usize,
    pub line: String,
}

impl MemoryStore {
    pub async fn new(dir: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&dir).await?;
        Ok(Self { dir })
    }

    pub async fn write(&self, key: &str, content: &str) -> Result<()> {
        let path = self.key_path(key)?;
        tokio::fs::write(&path, content).await?;
        Ok(())
    }

    pub async fn read(&self, key: &str) -> Result<Option<String>> {
        let path = self.key_path(key)?;
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn list_keys(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(keys),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                keys.push(stem.to_string());
            }
        }
        keys.sort();
        Ok(keys)
    }

    pub async fn search(&self, pattern: &str) -> Result<Vec<SearchHit>> {
        let pattern_lower = pattern.to_lowercase();
        let mut hits = Vec::new();
        for key in self.list_keys().await? {
            let path = self.key_path(&key)?;
            let content = tokio::fs::read_to_string(&path).await?;
            for (i, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&pattern_lower) {
                    hits.push(SearchHit {
                        key: key.clone(),
                        line_number: i + 1,
                        line: line.to_string(),
                    });
                }
            }
        }
        Ok(hits)
    }

    fn key_path(&self, key: &str) -> Result<PathBuf> {
        let sanitized = sanitize_key(key);
        if sanitized.is_empty() {
            bail!("invalid memory key: {key:?}");
        }
        Ok(self.dir.join(format!("{sanitized}.md")))
    }
}

/// Sanitize a key to only allow `[a-zA-Z0-9_-]` characters.
fn sanitize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_key() {
        assert_eq!(sanitize_key("hello-world_1"), "hello-world_1");
        assert_eq!(sanitize_key("../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_key("foo bar"), "foobar");
    }

    #[tokio::test]
    async fn test_memory_roundtrip() -> Result<()> {
        let dir = std::env::temp_dir().join("mnemis-test-memory");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let store = MemoryStore::new(dir.clone()).await?;

        store.write("test-note", "line one\nline two\n").await?;
        assert_eq!(
            store.read("test-note").await?,
            Some("line one\nline two\n".to_string())
        );

        let keys = store.list_keys().await?;
        assert_eq!(keys, vec!["test-note"]);

        let hits = store.search("two").await?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "test-note");
        assert_eq!(hits[0].line_number, 2);

        tokio::fs::remove_dir_all(&dir).await?;
        Ok(())
    }
}
