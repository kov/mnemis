-- When the server received the message (IMAP INTERNALDATE), unix seconds;
-- NULL until known. A Date header far from it marks forged mail.
ALTER TABLE messages ADD COLUMN received_at INTEGER;
