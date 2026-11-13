-- Thomas 2026-11-13 21:23: a ban by someone without a TommyBot ban permit (and not Owner, Extra
-- Owner or Trusted Admin) is shared only as an alert with Ban here, never as an automatic ban.
ALTER TABLE ban_sync_links ADD COLUMN alert_only BOOLEAN NOT NULL DEFAULT false;
