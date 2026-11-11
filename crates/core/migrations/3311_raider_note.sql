-- Raider List: a reporter's own words, kept apart from the counts-only detail and shown only to
-- the bot owner (the MCP view leaves it out).
ALTER TABLE raider_evidence ADD COLUMN note TEXT CHECK (length(note) <= 400);
