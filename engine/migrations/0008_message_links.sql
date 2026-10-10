-- Where a mail's links lead (JSON array of {host, shown?}), for phishing
-- signals. NULL when not looked at: chat sources, and mail ingested before
-- links were kept until `backfill-links` fills it in.
ALTER TABLE messages ADD COLUMN links_json TEXT;
