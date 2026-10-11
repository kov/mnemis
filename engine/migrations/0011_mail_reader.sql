-- The mail reader. A `messages` row is one email in one folder, so the
-- server placement lives on it: the UID it has there, and when it left the
-- folder (rows are never deleted — to-dos, search and embeddings point at
-- them). Threading headers group rows into threads across folders.
ALTER TABLE messages ADD COLUMN uid INTEGER;
ALTER TABLE messages ADD COLUMN uidvalidity INTEGER;
ALTER TABLE messages ADD COLUMN removed_at INTEGER;
ALTER TABLE messages ADD COLUMN in_reply_to TEXT;
-- JSON array of Message-IDs, oldest first, without angle brackets.
ALTER TABLE messages ADD COLUMN references_json TEXT;
ALTER TABLE messages ADD COLUMN thread_id INTEGER;
ALTER TABLE messages ADD COLUMN size INTEGER;
ALTER TABLE messages ADD COLUMN has_attachments INTEGER NOT NULL DEFAULT 0;
-- 0 while only the headers are known; the body is filled in when the full
-- message is fetched.
ALTER TABLE messages ADD COLUMN has_body INTEGER NOT NULL DEFAULT 1;
-- 1 for mail fetched for reading only (older history, muted folders): never
-- extracted for to-dos, never embedded.
ALTER TABLE messages ADD COLUMN reader_only INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_messages_channel_uid ON messages(channel_id, uid) WHERE uid IS NOT NULL;
CREATE INDEX idx_messages_thread ON messages(thread_id, posted_at) WHERE thread_id IS NOT NULL;

CREATE TABLE mail_threads (
    id INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    -- The subject without reply/forward prefixes, lowercased: joins replies
    -- from clients that drop the threading headers.
    subject_key TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_mail_threads_subject ON mail_threads(source_id, subject_key);

-- Every Message-ID a thread's mail carries (its own, In-Reply-To,
-- References), so a message finds its thread even when it arrives before
-- the one it answers.
CREATE TABLE mail_thread_refs (
    source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    ref TEXT NOT NULL,
    thread_id INTEGER NOT NULL REFERENCES mail_threads(id) ON DELETE CASCADE,
    PRIMARY KEY (source_id, ref)
);

CREATE INDEX idx_mail_thread_refs_thread ON mail_thread_refs(thread_id);

-- Per-folder reader sync state (JSON): UIDVALIDITY, the highest MODSEQ seen,
-- the UID the header sync has reached. Separate from `cursor`, which is the
-- to-do extraction poll's position.
ALTER TABLE channels ADD COLUMN mail_state TEXT;

-- Flag changes made in the app, waiting to reach the server.
CREATE TABLE mail_ops (
    id INTEGER PRIMARY KEY,
    channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    uid INTEGER NOT NULL,
    uidvalidity INTEGER NOT NULL,
    op TEXT NOT NULL,                     -- 'seen'|'unseen'
    created_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
);

-- Re-index search only when the text changes: the reader updates flags and
-- placements far more often than bodies.
DROP TRIGGER messages_au;
CREATE TRIGGER messages_au AFTER UPDATE OF body, subject ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body, subject) VALUES ('delete', old.id, old.body, old.subject);
    INSERT INTO messages_fts(rowid, body, subject) VALUES (new.id, new.body, new.subject);
END;
