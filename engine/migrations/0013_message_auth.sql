-- The receiving server's verdict on the sender (Authentication-Results),
-- as `mail::suspicion::Auth` JSON; NULL until read, `{}` when it gave none.
ALTER TABLE messages ADD COLUMN auth_json TEXT;
