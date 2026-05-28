# mnemis

A desktop app that watches your mailboxes, reads new messages with a local
LLM, and turns them into tracked **action items** — things you've been asked to
do or committed to — so they don't get lost in the inbox.

mnemis runs an extraction agent against any LLM server implementing the
[Responses API](https://platform.openai.com/docs/api-reference/responses)
(local servers like [omlx](https://github.com/) / LM Studio, or OpenAI). It
ingests IMAP email today; chat connectors and calendar sync are planned.

## Layout

mnemis is a Cargo workspace:

```
engine/   mnemis-engine — library: db, sources, ingest, embed, extract, queries
types/    mnemis-types  — pure-serde DTOs shared with the wasm frontend
cli/      mnemis-cli    — one-shot ops against the same SQLite database
app/      mnemis-app    — the Tauri desktop app (Leptos/wasm UI under app/ui/)
```

The app is the primary interface; the CLI runs the same engine for scripting
and debugging against the same database file.

## Building

Requires a recent Rust toolchain (2024 edition) and, for the desktop UI,
[Trunk](https://trunkrs.dev/) plus the `wasm32-unknown-unknown` target.

```sh
# Engine + CLI + types
cargo build --release

# Desktop app: build the wasm UI bundle first, then the Tauri binary
(cd app/ui && trunk build --release)
cargo build --release -p mnemis-app
```

## Configuration

Settings live in TOML at `~/.config/mnemis/config.toml` (override with the
`MNEMIS_CONFIG_PATH` env var, or `--config <path>` on the CLI). Connection
secrets for mail sources are **not** stored here — they go in the OS keychain.

```toml
[llm]
base_url = "http://localhost:1234/v1"
chat_model = "qwen3-30b-a3b"
embedding_model = "nomic-embed-text"
# bearer_token = "..."          # optional, for authenticated servers
# max_context_tokens = 32768    # optional; the server's context window
# request_timeout_secs = 300    # optional; per-request timeout

[paths]
# db = "~/.local/share/mnemis/mnemis.db"   # optional; this is the default
```

### `[llm]`

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `base_url` | yes | | Base URL of the Responses API server (include `/v1` if your server expects it) |
| `chat_model` | yes | | Model used for extraction. Must match a model loaded on the server |
| `embedding_model` | yes | | Model used for message/action embeddings |
| `bearer_token` | no | | Bearer token for API authentication |
| `max_context_tokens` | no | `32768` | The server's context window. mnemis sizes its message-window batches from this so a single call doesn't overflow the server |
| `request_timeout_secs` | no | `300` | Total per-request timeout for chat calls. Bounds a hang if the server accepts a request and never answers |

### `[paths]`

| Field | Default | Description |
|-------|---------|-------------|
| `db` | `~/.local/share/mnemis/mnemis.db` | SQLite database path (supports `~`) |

`MNEMIS_DB_PATH` overrides the database location at runtime regardless of the
config file — handy for tests and one-off setups.

### Adding a mailbox

Sources are stored in the database; the password is read once and kept in the
OS keychain. Add one from the app (Settings → Sources → *Add IMAP source*) or
from the CLI:

```sh
mnemis add-source imap --name work --server imap.example.com --username you@example.com
# prompts for the password, stores it in the keychain
```

Channels (mailboxes) are discovered on the first sync. In the app you can mute
individual channels so mnemis only watches the ones you care about.

## Running

### Desktop app

```sh
MNEMIS_DB_PATH=~/.local/share/mnemis/mnemis.db ./target/release/mnemis-app
```

The app polls each source, extracts action items, and shows them grouped by
confidence. High-confidence items are auto-claimed; lower-confidence
resolutions are queued for you to confirm or reject. "Sync now" runs a cycle on
demand.

On macOS the app is tray-resident (tray menu with *Show window / Sync now /
Quit*; closing the window hides it). On Linux it runs as a regular window. A
second launch focuses the existing instance.

### CLI

The CLI shares the database and config with the app. Useful subcommands:

| Command | Description |
|---------|-------------|
| `mnemis init [--display-name <name>]` | Create the database and run migrations |
| `mnemis add-source imap --server … --username … --name …` | Add an IMAP source (password → keychain) |
| `mnemis sync` | Run one polling + extraction cycle over all enabled sources |
| `mnemis list-actions [--status <s,…>] [--json]` | List actions (default: pending, auto-claimed, claimed) |
| `mnemis dump-prompt <channel_id>` | Print the extraction prompt for a channel without calling the LLM |
| `mnemis extract <channel_id>` | Run extraction for one channel (debugging) |
| `mnemis embed-once` | Drain the embed queue once (debugging) |
| `mnemis reset-data [--yes]` | Wipe ingested data (messages, actions, embeddings) but keep sources, settings, and your profile |

`--config <path>` is global. Channel ids come from
`sqlite3 <db> 'SELECT id, name FROM channels'`.

## How it works

Each sync, for every non-muted channel:

1. **Poll & ingest** new messages above the channel's watermark, then embed
   them.
2. **Extract**: an LLM agent reads a window of the new messages and records
   action items. The window is split into batches sized to fit
   `max_context_tokens` (with headroom for the agent's multi-turn growth); each
   batch is a self-contained agent session, and the watermark advances per
   batch so a mid-window failure only loses the unfinished part.

The agent has five tools:

- **search_messages** / **fetch_message** — look beyond the window for context
- **record_action** — create an action item (title, details, confidence,
  optional due date, evidence message ids)
- **update_action** — amend an action it already recorded (rather than
  duplicating it)
- **resolve_action** — propose marking an existing action done/cancelled

Actions carry a **confidence** (high / medium / low). High-confidence new
actions are auto-claimed; medium/low `resolve_action` calls are queued as
*suggestions* you confirm or reject in the app. When you dismiss an item or
undo a wrong auto-claim, that feedback is fed back into later extraction
prompts as a negative example.

Per-channel **watermarks** (highest message id processed) mean each sync only
sees new mail.

## Logging and traces

### Log output (`RUST_LOG`)

The app and CLI read the standard `RUST_LOG` env var (via
`tracing_subscriber`'s `EnvFilter`); the default is `info`.

The LLM call cadence is logged from the `mnemis_engine::extract` target at
`trace` level. To watch just the LLM send/recv markers — turn number, channel,
elapsed time, status — without anything else:

```sh
env RUST_LOG=mnemis_engine::extract=trace ./target/debug/mnemis-app
```

A single target directive silences everything else. To keep warnings from the
rest of the app, prepend a global level:

```sh
env RUST_LOG=warn,mnemis_engine::extract=trace ./target/debug/mnemis-app
```

These are lightweight markers only: they tell you *that* a call happened and
how long it took, not what was said.

### Per-turn traces (the full chatter)

The complete prompts, tool calls, and responses for every extraction turn are
written as JSONL — one file per batch session — to `<db parent>/traces/`
(default `~/.local/share/mnemis/traces/{ran_at}-ch{channel_id}.jsonl`). They're
written regardless of `RUST_LOG` and are where you read the actual LLM
conversation. Inspect them with `jq`:

```sh
# Newest trace file
ls -t ~/.local/share/mnemis/traces/*.jsonl | head -1

# The event flow for one session, in order
jq -r '.event' <file>.jsonl

# The system prompt that was sent
jq -r 'select(.event=="system_prompt").text' <file>.jsonl

# Each turn's input, and the model's output
jq 'select(.event=="llm_send") | {turn, input}' <file>.jsonl
jq 'select(.event=="llm_recv") | {turn, status, elapsed_secs, output}' <file>.jsonl

# Tool calls and their results
jq 'select(.event=="tool_dispatch") | {turn, name, arguments, output}' <file>.jsonl
```
