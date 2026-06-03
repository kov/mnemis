-- Chat context-window compaction checkpoints. When a conversation's
-- reconstructed model input approaches the server's context window, the older
-- turns are folded into a single summary row here and only the verbatim tail
-- (turns past `up_to_turn_id`) is replayed to the model. `chat_turns` is never
-- modified or deleted — the UI still renders the full conversation; only
-- `build_history` (the model's input) substitutes this summary for the
-- compacted prefix.
--
-- Append-only: the row with the greatest `up_to_turn_id` is the active
-- checkpoint, and re-compaction folds the prior summary plus the newly-evicted
-- turns into a fresh row. Keeping every row doubles as an audit trail of when
-- the conversation was condensed.
CREATE TABLE chat_summaries (
    id INTEGER PRIMARY KEY,
    chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    up_to_turn_id INTEGER NOT NULL,   -- highest chat_turns.id folded into `summary`
    summary TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_chat_summaries_chat ON chat_summaries(chat_id, up_to_turn_id);
