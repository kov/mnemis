-- Embeddings now come from an in-process model (EmbeddingGemma-300M via
-- fastembed) instead of the LLM server. Vectors from different models aren't
-- comparable, so drop every stored vector and re-queue every source row; the
-- embed drain rebuilds them. Same 768 dimensions, so the vec0 tables stay.
DELETE FROM messages_vec;
DELETE FROM actions_vec;
DELETE FROM memory_notes_vec;

-- `text_hash` only dedupes re-enqueues of unchanged text; a placeholder is fine
-- (the next real enqueue of a row overwrites it). Existing queue rows get their
-- retry budget reset.
INSERT INTO embed_queue (target_kind, target_id, text_hash, enqueued_at)
SELECT 'message', id, 'reembed', unixepoch() FROM messages WHERE true
ON CONFLICT(target_kind, target_id) DO UPDATE SET attempts = 0, last_error = NULL;

INSERT INTO embed_queue (target_kind, target_id, text_hash, enqueued_at)
SELECT 'action', id, 'reembed', unixepoch() FROM actions WHERE true
ON CONFLICT(target_kind, target_id) DO UPDATE SET attempts = 0, last_error = NULL;

INSERT INTO embed_queue (target_kind, target_id, text_hash, enqueued_at)
SELECT 'memory_note', id, 'reembed', unixepoch() FROM memory_notes WHERE true
ON CONFLICT(target_kind, target_id) DO UPDATE SET attempts = 0, last_error = NULL;
