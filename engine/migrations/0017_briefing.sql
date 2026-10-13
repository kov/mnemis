-- The briefing (see `briefing`). Each new email is described once, as it
-- arrives: what kind of mail it is and how much it is worth the user's time.
CREATE TABLE mail_tags (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    category TEXT NOT NULL,               -- `mnemis_types::MailCategory`
    interest INTEGER NOT NULL,            -- 0 noise, 1 skim, 2 worth reading, 3 important
    reason TEXT NOT NULL DEFAULT '',
    tagged_at INTEGER NOT NULL
);

-- The latest summary the briefing agent wrote for each period length.
CREATE TABLE briefings (
    days INTEGER PRIMARY KEY,
    since INTEGER NOT NULL,               -- start of the period it covers
    generated_at INTEGER NOT NULL,
    up_to INTEGER NOT NULL,               -- newest messages.id it saw
    summary TEXT NOT NULL,
    highlights_json TEXT NOT NULL         -- [{"external_id", "why"}], most important first
);
