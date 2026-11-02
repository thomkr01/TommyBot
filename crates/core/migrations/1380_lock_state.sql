-- 'locking' until every lock step ran, 'unlocking' once an unlock claimed it; the bot finishes
-- either after a restart. Existing rows are complete locks.
ALTER TABLE lock_snapshots ADD COLUMN state TEXT NOT NULL DEFAULT 'locked';
