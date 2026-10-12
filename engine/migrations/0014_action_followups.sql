-- New mail on a to-do's thread goes to a follow-up run with the to-do
-- (`extract::followup`). `followup_from`: the thread's newest message when
-- the to-do was made; mail after it is the follow-up's, not extraction's.
-- `followed_up_to`: the newest message a follow-up has handled. Both are
-- message ids, which stay put when threads merge. NULL: no thread.
ALTER TABLE actions ADD COLUMN followup_from INTEGER;
ALTER TABLE actions ADD COLUMN followed_up_to INTEGER;

UPDATE actions SET followup_from = (
    SELECT MAX(t.id) FROM action_evidence ae
    JOIN messages m ON m.id = ae.message_id
    JOIN messages t ON t.thread_id = m.thread_id
    WHERE ae.action_id = actions.id AND ae.is_primary = 1
);
UPDATE actions SET followed_up_to = followup_from;
