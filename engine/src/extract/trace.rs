//! Per-run JSONL trace writer. One file per extraction run holds the full
//! agent loop transcript: system prompt, every LLM request, every LLM
//! response, every tool dispatch + output, and a final finish/error line.
//!
//! Read with `jq -c . path/to/trace.jsonl` or `jq 'select(.event=="...")'`.
//! Files sit under `<db_parent>/traces/<ran_at>-ch<channel_id>.jsonl`, where
//! `ran_at` matches the corresponding row in `extraction_runs.ran_at` so
//! `SELECT * FROM extraction_runs WHERE ran_at = N` joins one-for-one.
//!
//! The writer is best-effort: a failure to open the file or write a line
//! logs a warning and downgrades to a no-op so extraction itself never
//! fails because of trace I/O.
//!
//! Cleanup is trivial — `rm <db_parent>/traces/*.jsonl` or filter by date.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::llm::{InputItem, ResponsesResponse, ToolDef};

/// Owns a file handle plus its path. Methods serialize each event to a
/// single JSON line; failures log a warning and silently continue.
pub struct TraceWriter {
    file: Option<File>,
    path: PathBuf,
}

impl TraceWriter {
    /// Open a trace at `<traces_dir>/<ran_at>-ch<channel_id>.jsonl`. The
    /// `traces_dir` is created if it doesn't exist. If anything goes wrong
    /// (no permission, disk full, etc.) returns a no-op writer so callers
    /// don't have to guard every call site.
    pub fn open(traces_dir: &Path, ran_at: i64, channel_id: i64) -> Self {
        Self::open_named(traces_dir, &format!("{ran_at}-ch{channel_id}.jsonl"))
    }

    /// Open a chat-run trace at `<traces_dir>/<ran_at>-chat<chat_id>.jsonl`.
    /// One file per user message; the `chat` prefix distinguishes it from the
    /// `ch<channel_id>` extraction traces in the same directory.
    pub fn open_chat(traces_dir: &Path, ran_at: i64, chat_id: i64) -> Self {
        Self::open_named(traces_dir, &format!("{ran_at}-chat{chat_id}.jsonl"))
    }

    fn open_named(traces_dir: &Path, filename: &str) -> Self {
        let path = traces_dir.join(filename);
        let file = (|| -> std::io::Result<File> {
            std::fs::create_dir_all(traces_dir)?;
            OpenOptions::new().create(true).append(true).open(&path)
        })();
        match file {
            Ok(f) => Self {
                file: Some(f),
                path,
            },
            Err(e) => {
                warn!(error = %e, path = %path.display(), "opening trace file; tracing disabled for this run");
                Self { file: None, path }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write the system prompt as the run's first line. Recorded once at
    /// the top so a later diff between runs is just a literal text diff.
    pub fn system_prompt(&mut self, prompt: &str) {
        self.write(&json!({
            "event": "system_prompt",
            "text": prompt,
        }));
    }

    /// Tool definitions passed to the model — same payload it sees. Once
    /// per run, right after the prompt.
    pub fn tools(&mut self, tools: &[ToolDef]) {
        self.write(&json!({
            "event": "tools",
            "tools": tools,
        }));
    }

    pub fn llm_send(
        &mut self,
        turn: usize,
        input: &[InputItem],
        previous_response_id: Option<&str>,
    ) {
        self.write(&json!({
            "event": "llm_send",
            "turn": turn,
            "previous_response_id": previous_response_id,
            "input": input,
        }));
    }

    pub fn llm_recv(&mut self, turn: usize, elapsed_secs: u64, response: &ResponsesResponse) {
        self.write(&json!({
            "event": "llm_recv",
            "turn": turn,
            "elapsed_secs": elapsed_secs,
            "response_id": response.id,
            "status": response.status,
            "output": response.output,
        }));
    }

    pub fn llm_error(&mut self, turn: usize, error: &str) {
        self.write(&json!({
            "event": "llm_error",
            "turn": turn,
            "error": error,
        }));
    }

    pub fn tool_dispatch(
        &mut self,
        turn: usize,
        call_id: &str,
        name: &str,
        arguments: &str,
        output: &str,
    ) {
        // Parse arguments + output as JSON if possible so jq queries work
        // naturally — fall back to raw strings if the model emitted
        // malformed JSON (which itself is interesting to see in the trace).
        let args = serde_json::from_str::<Value>(arguments)
            .unwrap_or_else(|_| Value::String(arguments.to_string()));
        let out = serde_json::from_str::<Value>(output)
            .unwrap_or_else(|_| Value::String(output.to_string()));
        self.write(&json!({
            "event": "tool_dispatch",
            "turn": turn,
            "call_id": call_id,
            "name": name,
            "arguments": args,
            "output": out,
        }));
    }

    pub fn finish(&mut self, actions_created: usize, summary: Option<&str>) {
        self.write(&json!({
            "event": "finish",
            "actions_created": actions_created,
            "summary": summary,
        }));
    }

    pub fn agent_error(&mut self, error: &str) {
        self.write(&json!({
            "event": "agent_error",
            "error": error,
        }));
    }

    fn write<T: Serialize>(&mut self, value: &T) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        match serde_json::to_string(value) {
            Ok(mut line) => {
                line.push('\n');
                if let Err(e) = file.write_all(line.as_bytes()) {
                    warn!(error = %e, path = %self.path.display(), "trace write failed");
                    self.file = None;
                }
            }
            Err(e) => {
                warn!(error = %e, path = %self.path.display(), "trace serialize failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tempfile::TempDir;

    fn read_jsonl(path: &Path) -> Vec<Value> {
        let text = std::fs::read_to_string(path).expect("reading trace file");
        text.lines()
            .map(|l| serde_json::from_str(l).expect("parsing trace line"))
            .collect()
    }

    #[test]
    fn writes_one_line_per_event_in_order() {
        let tmp = TempDir::new().unwrap();
        let mut w = TraceWriter::open(tmp.path(), 1_700_000_000, 4);

        w.system_prompt("hello world");
        w.llm_send(
            0,
            &[InputItem::Message {
                role: crate::llm::Role::User,
                content: "go".into(),
            }],
            None,
        );
        w.tool_dispatch(
            0,
            "call_1",
            "record_action",
            r#"{"title":"x"}"#,
            r#"{"action_id":"A-1"}"#,
        );
        w.finish(1, Some("Recorded 1 action."));

        let path = tmp.path().join("1700000000-ch4.jsonl");
        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["event"], "system_prompt");
        assert_eq!(lines[0]["text"], "hello world");
        assert_eq!(lines[1]["event"], "llm_send");
        assert_eq!(lines[1]["turn"], 0);
        // Pre-parsed JSON args, so jq can query `.arguments.title` directly.
        assert_eq!(lines[2]["event"], "tool_dispatch");
        assert_eq!(lines[2]["arguments"]["title"], "x");
        assert_eq!(lines[2]["output"]["action_id"], "A-1");
        assert_eq!(lines[3]["event"], "finish");
        assert_eq!(lines[3]["actions_created"], 1);
    }

    #[test]
    fn malformed_tool_args_fall_back_to_string() {
        // Pin the fallback: if the model emits garbage as `arguments` the
        // trace still records the raw string instead of dropping the line.
        // (Several historical hallucination cases looked like this.)
        let tmp = TempDir::new().unwrap();
        let mut w = TraceWriter::open(tmp.path(), 1, 1);
        w.tool_dispatch(0, "c", "record_action", "not json", "{}");
        let path = tmp.path().join("1-ch1.jsonl");
        let lines = read_jsonl(&path);
        assert_eq!(lines[0]["arguments"], "not json");
    }

    #[test]
    fn open_returns_noop_when_dir_unwritable() {
        // /proc isn't writable for most users; pick something we can't create.
        let w = TraceWriter::open(Path::new("/proc/cant-create-here"), 1, 1);
        assert!(w.file.is_none(), "expected no-op writer when open fails");
    }
}
