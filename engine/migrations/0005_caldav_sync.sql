-- Per-action CalDAV/VTODO sync state. The action↔reminder UID already lives in
-- `external_calendar_uid` (0001); this adds what two-way sync needs on top:
--   external_href  -- the resource path within the task collection, for PUT/DELETE
--   external_etag  -- server ETag for optimistic concurrency (If-Match)
--   sync_status    -- NULL (never synced / not a reminder) | 'synced' | 'dirty'
--                  --   (local change awaiting push) | 'needs_review' (repeated
--                  --   409 conflict — stop retrying, surface in UI)
--   sync_error     -- last sync error string for the UI, NULL when clean
-- The CalDAV account itself is stored in `settings` under `caldav/account`
-- (keychain ref + JSON), mirroring how IMAP sources are persisted.
ALTER TABLE actions ADD COLUMN external_href TEXT;
ALTER TABLE actions ADD COLUMN external_etag TEXT;
ALTER TABLE actions ADD COLUMN sync_status TEXT;
ALTER TABLE actions ADD COLUMN sync_error TEXT;
