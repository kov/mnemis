-- Distinguish the two negative-signal sources the extractor learns from:
--   'dismissed'         — user said "not an action item"
--   'wrong_auto_claim'  — user undid an auto-claim with a comment
-- Existing rows are all dismissals, so backfill takes the default.
ALTER TABLE dismissal_feedback
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'dismissed';
