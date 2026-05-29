-- Capture email To/Cc recipients for the metadata-first extraction window.
-- JSON array of {kind:'to'|'cc', name?, address?}; NULL when unknown (chat
-- sources, or mail without recipient headers). Kept as its own migration
-- (not folded into 0001) so existing DBs pick it up in place without an
-- 0001 checksum mismatch.
ALTER TABLE messages ADD COLUMN recipients_json TEXT;
