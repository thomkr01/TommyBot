-- Thomas 2026-11-13 21:23: a ban by someone without a TommyBot ban permit (and not Owner, Extra
-- Owner or Trusted Admin) is shared only as an alert with Ban here, never as an automatic ban.
ALTER TABLE ban_sync_links ADD COLUMN alert_only BOOLEAN NOT NULL DEFAULT false;
-- Rows waiting from before this column (pending, or held by the hourly cap for Send the rest)
-- whose ban came from Discord's own ban, not TommyBot: who made it is not known here, so they
-- become alerts (fail closed).
UPDATE ban_sync_links SET alert_only = true WHERE origin = 'discord' AND state IN ('pending', 'skipped');
