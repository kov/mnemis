# mnemis

An email agent that connects to your IMAP mailbox, reads new messages using
an LLM, and produces attention reports. It maintains persistent memory between
runs so it can track contacts, action items, and patterns over time.

mnemis talks to any LLM server that implements the
[Responses API](https://platform.openai.com/docs/api-reference/responses)
(OpenAI, compatible local servers, etc.) and uses tool-calling to interact
with your mailbox autonomously.

## Building

Requires Rust 2024 edition (nightly or recent stable).

```sh
cargo build --release
```

## Quick start

1. Create the config file at `~/.config/mnemis/config.toml`:

```toml
[llm]
base_url = "http://localhost:8080"
model = "qwen2.5-7b-instruct"

[imap]
server = "imap.gmail.com"
username = "you@gmail.com"
password = "your-app-password"

mailboxes = ["INBOX"]
```

2. Run it:

```sh
mnemis
```

mnemis will connect to your mailbox, scan for new unread messages, and print
a report to stdout.

## Configuration

The config file is TOML. The default path is `~/.config/mnemis/config.toml`,
overridden with `--config <path>`. All path fields support `~` expansion.

### `[llm]` — LLM connection (required)

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `base_url` | yes | | Base URL of the Responses API server |
| `model` | yes | | Model identifier to use |
| `bearer_token` | no | | Bearer token for API authentication |
| `max_tool_calls` | no | `200` | Safety limit on tool calls per agent run |

### `[imap]` — IMAP connection (required)

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `server` | yes | | IMAP server hostname |
| `port` | no | `993` | IMAP port (TLS) |
| `username` | yes | | IMAP username |
| `password` | yes | | IMAP password |

### `[paths]` — File paths (optional)

| Field | Default | Description |
|-------|---------|-------------|
| `memory_dir` | `~/.config/mnemis/memory` | Directory for persistent memory notes |
| `guidance_file` | `~/.config/mnemis/guidance.md` | Path to the guidance file |
| `state_file` | `~/.config/mnemis/state.json` | Path to the watermark state file |

### `mailboxes` — Mailboxes to scan (top-level)

A list of IMAP mailbox names to scan in default (autonomous) mode:

```toml
mailboxes = ["INBOX", "Work", "Notifications"]
```

## Guidance file

The guidance file lets you customize the agent's behavior. It is a markdown
file loaded from `~/.config/mnemis/guidance.md` by default (configurable via
`[paths].guidance_file`). If the file doesn't exist or is empty, mnemis still
works using its built-in system prompt.

The guidance content is appended to the built-in system prompt, so you can
use it to add domain-specific instructions. Example:

```markdown
## What to focus on

- Emails from direct reports and customers are high priority.
- Ignore automated notifications from CI systems.
- Flag anything with a deadline in the next 7 days.

## Report format

Start with a 2-3 sentence summary, then list action items.

## Memory

- Keep `action-items` updated with status and deadlines.
- Track new contacts in `contacts`.
```

## Usage

### Default mode (autonomous scanning)

```sh
mnemis
```

Scans each mailbox listed in the config, one at a time. For each mailbox,
the agent checks for new unread messages since the last run, processes them
per the guidance, and prints a report to stdout. Reports are printed as each
mailbox completes.

State watermarks are saved after all mailboxes finish, so subsequent runs
only see new messages.

### Ask mode

```sh
mnemis ask "summarize the last 5 emails from alice@example.com"
```

Runs a single agent conversation with a custom prompt.

### Chat mode

```sh
mnemis chat
```

Interactive REPL. Type prompts, get responses. History is saved to
`~/.local/share/mnemis/history.txt`. Type `quit` or `exit` to leave.

### Flags

| Flag | Description |
|------|-------------|
| `--config <path>` | Config file path (default: `~/.config/mnemis/config.toml`) |
| `--thinking` | Print LLM reasoning and tool calls to stderr |

## How it works

mnemis runs an LLM tool-use loop. The agent has access to 8 tools:

- **list_mailboxes** — list available IMAP mailboxes
- **list_messages** — list messages in a mailbox (subject, from, date, flags)
- **read_messages** — fetch full message content by UID
- **mark_as_read** — set the `\Seen` flag (only when explicitly instructed)
- **write_memory** / **read_memory** / **search_memory** — persistent key-value note store (markdown files)
- **write_report** — emit the final report (required to end a run)

The agent decides which tools to call based on the prompt and guidance. Tool
errors are returned to the LLM as context rather than crashing the program.

### Memory

Memory notes are stored as individual `.md` files in the memory directory.
Keys are sanitized to alphanumeric characters, hyphens, and underscores.
The agent can read, write, list, and search notes across runs.

### State

mnemis tracks per-mailbox watermarks (the highest UID seen) in a JSON state
file. In default mode, `list_messages` automatically filters out messages
from previous runs unless `include_old: true` is passed. Watermarks are
committed to disk after all mailboxes finish scanning.
