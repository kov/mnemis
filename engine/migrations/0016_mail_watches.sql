-- Mail the user asked the chat agent to watch for, and what to do when it
-- arrives (see `extract::watch`). Mail after `checked_up_to` hasn't been
-- checked against the watch yet.
CREATE TABLE mail_watches (
    id INTEGER PRIMARY KEY,
    chat_id INTEGER REFERENCES chats(id) ON DELETE SET NULL,
    looking_for TEXT NOT NULL,
    instruction TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open',  -- 'open'|'matched'|'expired'|'cancelled'
    created_at INTEGER NOT NULL,
    expires_at INTEGER,                   -- NULL: never
    checked_up_to INTEGER NOT NULL,       -- messages.id
    matched_message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    closed_at INTEGER
);

CREATE INDEX idx_mail_watches_status ON mail_watches(status);
