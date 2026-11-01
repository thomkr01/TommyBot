ALTER TABLE appeals DROP CONSTRAINT appeals_status_check;
ALTER TABLE appeals ADD CONSTRAINT appeals_status_check CHECK (status IN ('open', 'accepted', 'denied', 'dismissed', 'withdrawn'));
-- The Discord log message with the decision buttons, so a dashboard decision can close it.
ALTER TABLE appeals ADD COLUMN log_channel BIGINT;
ALTER TABLE appeals ADD COLUMN log_message BIGINT;
