-- D24 (R-h3 4): when the bot sent the verdict DM, so a retried accept that finds the appeal
-- already accepted by the same staff member sends the DM it missed, and only once.
ALTER TABLE appeals ADD COLUMN verdict_sent_at TIMESTAMPTZ;
