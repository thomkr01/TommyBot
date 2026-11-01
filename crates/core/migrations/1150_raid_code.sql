-- A raid gets a public report code; each caught account remembers the invite it joined with.
ALTER TABLE raid_log ADD COLUMN code TEXT, ADD COLUMN invite_code TEXT;
CREATE INDEX raid_log_code ON raid_log (code);
