-- mnemis v2 initial schema.
-- vec0 virtual tables require the sqlite-vec extension to be auto-registered
-- via sqlite_vec::sqlite3_auto_extension() before any connection is opened.

-- ========== Sources, channels, people, messages ==========

CREATE TABLE sources (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,                   -- 'imap'|'mattermost'|...
    name TEXT NOT NULL,
    config_ref TEXT NOT NULL,             -- OS keychain entry name
    last_synced_at INTEGER,
    last_error TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'ok',    -- 'ok'|'warning'|'failed'|'disabled'
    created_at INTEGER NOT NULL
);

CREATE TABLE channels (
    id INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL REFERENCES sources(id),
    external_id TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,                   -- 'mailbox'|'channel'|'dm'|'group'
    cursor TEXT,                          -- opaque per-source position
    last_synced_at INTEGER,
    muted INTEGER NOT NULL DEFAULT 0,
    UNIQUE(source_id, external_id)
);

CREATE TABLE contacts (
    id INTEGER PRIMARY KEY,
    display_name TEXT NOT NULL,
    notes TEXT,
    relationship TEXT,                    -- 'self'|'boss'|'report'|'family'|'friend'|'colleague'|freeform
    external_uid TEXT,                    -- reserved for CardDAV
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE contact_identifiers (
    id INTEGER PRIMARY KEY,
    contact_id INTEGER NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,                   -- 'email'|'mattermost_handle'|'discord_id'|'phone'
    value TEXT NOT NULL,
    UNIQUE(kind, value)
);

CREATE TABLE people (
    id INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL REFERENCES sources(id),
    external_id TEXT NOT NULL,
    display_name TEXT,
    handle TEXT,
    contact_id INTEGER REFERENCES contacts(id),
    UNIQUE(source_id, external_id)
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    channel_id INTEGER NOT NULL REFERENCES channels(id),
    external_id TEXT NOT NULL,
    parent_external_id TEXT,
    author_id INTEGER REFERENCES people(id),
    posted_at INTEGER NOT NULL,
    subject TEXT,
    body TEXT NOT NULL,
    body_format TEXT NOT NULL,            -- 'text'|'markdown'|'html'
    raw_json TEXT,
    flags INTEGER NOT NULL DEFAULT 0,
    ingested_at INTEGER NOT NULL,
    UNIQUE(channel_id, external_id)
);

CREATE INDEX idx_messages_channel_posted ON messages(channel_id, posted_at DESC);
CREATE INDEX idx_messages_parent ON messages(parent_external_id) WHERE parent_external_id IS NOT NULL;

CREATE VIRTUAL TABLE messages_fts USING fts5(
    body, subject,
    content='messages',
    content_rowid='id'
);

CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body, subject) VALUES (new.id, new.body, new.subject);
END;
CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body, subject) VALUES ('delete', old.id, old.body, old.subject);
END;
CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body, subject) VALUES ('delete', old.id, old.body, old.subject);
    INSERT INTO messages_fts(rowid, body, subject) VALUES (new.id, new.body, new.subject);
END;

CREATE VIRTUAL TABLE messages_vec USING vec0(embedding float[768]);

-- ========== User profile (singleton) ==========

CREATE TABLE user_profile (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    display_name TEXT NOT NULL,
    custom_prompt TEXT,
    updated_at INTEGER NOT NULL
);

-- ========== Actions, evidence, events, extraction support ==========

CREATE TABLE actions (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    details TEXT,
    confidence TEXT NOT NULL,             -- 'low'|'medium'|'high'
    rationale TEXT,
    status TEXT NOT NULL,                 -- 'pending'|'auto_claimed'|'claimed'|'done'|'dismissed'
    dismissed_reason TEXT,
    due_at INTEGER,
    external_calendar_uid TEXT,
    extracted_at INTEGER NOT NULL,
    claimed_at INTEGER,
    resolved_at INTEGER
);

CREATE INDEX idx_actions_status ON actions(status);
CREATE INDEX idx_actions_due ON actions(due_at) WHERE due_at IS NOT NULL;

CREATE TABLE action_evidence (
    action_id INTEGER NOT NULL REFERENCES actions(id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages(id),
    kind TEXT NOT NULL DEFAULT 'source',  -- 'source'|'resolution'
    is_primary INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (action_id, message_id, kind)
);

CREATE TABLE action_events (
    id INTEGER PRIMARY KEY,
    action_id INTEGER NOT NULL REFERENCES actions(id),
    event_kind TEXT NOT NULL,             -- 'created'|'updated'|'claimed'|'resolved'|'unresolved'|'dismissed'
    actor TEXT NOT NULL,                  -- 'user'|'agent_auto'|'agent_queued'|'caldav_sync'
    data_json TEXT,
    evidence_external_ids TEXT,
    occurred_at INTEGER NOT NULL
);

CREATE INDEX idx_action_events_action ON action_events(action_id, occurred_at DESC);

CREATE TABLE extraction_runs (
    id INTEGER PRIMARY KEY,
    channel_id INTEGER NOT NULL REFERENCES channels(id),
    ran_at INTEGER NOT NULL,
    up_to_message_id INTEGER,
    model TEXT NOT NULL,
    prompt_version INTEGER NOT NULL,
    result TEXT NOT NULL,                 -- 'ok'|'error'|'no_activity'
    embeddings_partial INTEGER NOT NULL DEFAULT 0,
    messages_pending_embed INTEGER NOT NULL DEFAULT 0,
    summary TEXT
);

CREATE TABLE dismissal_feedback (
    id INTEGER PRIMARY KEY,
    scope_kind TEXT NOT NULL,             -- 'global'|'source'|'channel'
    scope_id INTEGER,
    example_text TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE extraction_directives (
    id INTEGER PRIMARY KEY,
    scope_kind TEXT NOT NULL,             -- 'source'|'channel'
    scope_id INTEGER NOT NULL,
    directive TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- ========== Embed queue ==========

CREATE TABLE embed_queue (
    id INTEGER PRIMARY KEY,
    target_kind TEXT NOT NULL,            -- 'message'|'memory_note'|'action'|'contact'
    target_id INTEGER NOT NULL,
    text_hash TEXT NOT NULL,
    enqueued_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    UNIQUE(target_kind, target_id)
);

-- ========== Settings ==========

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL                   -- JSON
);

-- ========== Chats ==========

CREATE TABLE chats (
    id INTEGER PRIMARY KEY,
    title TEXT,
    seeded_from_kind TEXT,                -- 'message'|'action'|'memory'|'report'|null
    seeded_from_id INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    archived INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE chat_turns (
    id INTEGER PRIMARY KEY,
    chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    role TEXT NOT NULL,                   -- 'user'|'assistant'|'tool'
    content TEXT,
    tool_name TEXT,
    tool_call_id TEXT,
    response_id TEXT,                     -- omlx response id
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_chat_turns_chat ON chat_turns(chat_id, created_at);

-- Stored separately so reconstruction code that joins chat_turns alone
-- can never accidentally replay reasoning into model history.
CREATE TABLE chat_turn_reasoning (
    turn_id INTEGER PRIMARY KEY REFERENCES chat_turns(id) ON DELETE CASCADE,
    content TEXT NOT NULL
);

-- ========== Memory notes ==========

CREATE TABLE memory_notes (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE VIRTUAL TABLE memory_notes_fts USING fts5(
    key, content,
    content='memory_notes',
    content_rowid='id'
);

CREATE TRIGGER memory_notes_ai AFTER INSERT ON memory_notes BEGIN
    INSERT INTO memory_notes_fts(rowid, key, content) VALUES (new.id, new.key, new.content);
END;
CREATE TRIGGER memory_notes_ad AFTER DELETE ON memory_notes BEGIN
    INSERT INTO memory_notes_fts(memory_notes_fts, rowid, key, content) VALUES ('delete', old.id, old.key, old.content);
END;
CREATE TRIGGER memory_notes_au AFTER UPDATE ON memory_notes BEGIN
    INSERT INTO memory_notes_fts(memory_notes_fts, rowid, key, content) VALUES ('delete', old.id, old.key, old.content);
    INSERT INTO memory_notes_fts(rowid, key, content) VALUES (new.id, new.key, new.content);
END;

CREATE VIRTUAL TABLE memory_notes_vec USING vec0(embedding float[768]);

-- ========== Reports ==========

CREATE TABLE reports (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,                  -- 'mailbox'|'channel'|'attention'|...
    scope_ref TEXT,
    content TEXT NOT NULL,
    generated_at INTEGER NOT NULL
);
