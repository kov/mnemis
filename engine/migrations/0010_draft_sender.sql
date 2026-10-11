-- The address a draft is sent as: one of the user's, picked to match the
-- one the original was sent to. NULL uses the account's sender address.
ALTER TABLE drafts ADD COLUMN from_addr TEXT;
