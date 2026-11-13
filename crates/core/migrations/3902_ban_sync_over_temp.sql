-- A copy laid over a local temp ban keeps that ban's reason on Discord (a second ban PUT changes
-- nothing), so the row remembers it: over_reason is the reason Discord shows for the copy, and
-- temp_end the local temp ban's end, which a lift before then puts back.
ALTER TABLE ban_sync_links ADD COLUMN over_reason TEXT, ADD COLUMN temp_end TIMESTAMPTZ;
