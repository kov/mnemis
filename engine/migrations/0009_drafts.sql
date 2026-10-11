-- Outgoing mail, composed by the user or drafted by the chat agent. Only the
-- user's send button sends one; the agent can only write drafts.
CREATE TABLE drafts (
    id INTEGER PRIMARY KEY,
    -- The account it is sent from.
    source_id INTEGER REFERENCES sources(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,                   -- 'new'|'reply'|'reply_all'|'forward'
    -- The message replied to or forwarded.
    message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    -- The chat whose agent drafted it, when it did.
    chat_id INTEGER REFERENCES chats(id) ON DELETE SET NULL,
    to_addrs TEXT NOT NULL DEFAULT '',    -- comma-separated, as typed
    cc_addrs TEXT NOT NULL DEFAULT '',
    bcc_addrs TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    -- Threading headers for a reply, '<id>' form.
    in_reply_to TEXT,
    "references" TEXT,
    status TEXT NOT NULL DEFAULT 'draft', -- 'draft'|'sent'|'discarded'
    -- The last failed send's error; cleared by a successful one.
    error TEXT,
    created_by TEXT NOT NULL,             -- 'user'|'agent'
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    sent_at INTEGER,
    -- Message-ID given to the sent mail.
    sent_message_id TEXT
);

CREATE INDEX idx_drafts_status ON drafts(status, updated_at DESC);
