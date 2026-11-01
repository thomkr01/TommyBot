ALTER TABLE reports ADD COLUMN reason TEXT NOT NULL DEFAULT '';
ALTER TABLE appeals DROP CONSTRAINT appeals_status_check;
ALTER TABLE appeals ADD CONSTRAINT appeals_status_check CHECK (status IN ('open', 'accepted', 'denied', 'dismissed'));
ALTER TABLE appeals ADD COLUMN reply TEXT;
