-- Vector index for actions, mirroring messages_vec / memory_notes_vec.
-- 768-dim to match the ModernBERT embedding model used everywhere else.
CREATE VIRTUAL TABLE actions_vec USING vec0(embedding float[768]);
