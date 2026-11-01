-- When the appellant saw the verdict in the dashboard bell; null means unread.
ALTER TABLE appeals ADD COLUMN seen_at TIMESTAMPTZ;
-- Verdicts before this migration are not news.
UPDATE appeals SET seen_at = now() WHERE status <> 'open';
