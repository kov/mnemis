-- When a reminder alerts on its day (`due_at`), in minutes after midnight
-- local time; NULL: the default morning time.
ALTER TABLE actions ADD COLUMN remind_minutes INTEGER;
