-- The language the user wants to-dos, summaries and the briefing written in
-- (an English name, "Brazilian Portuguese"); NULL: each email's own.
ALTER TABLE user_profile ADD COLUMN language TEXT;
