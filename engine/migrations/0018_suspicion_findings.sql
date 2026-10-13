-- What the briefing agent concluded after looking into an email that looked
-- forged (see `mail::suspicion`), keyed by Message-ID so every copy shares
-- it. Shown with the email's warnings wherever they appear.
CREATE TABLE suspicion_findings (
    external_id TEXT PRIMARY KEY,
    phishing INTEGER NOT NULL,            -- 1: judged phishing; 0: judged genuine
    finding TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
