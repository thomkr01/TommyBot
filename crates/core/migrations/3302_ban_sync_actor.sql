-- Who made the source ban, so a copy is checked again against anti-nuke right before it acts.
-- actor_id is the audit log's actor (the bot for its own commands), mod_id the staff member
-- behind it, banned_at when the source ban was made.
ALTER TABLE ban_sync_links ADD COLUMN actor_id BIGINT, ADD COLUMN mod_id BIGINT, ADD COLUMN banned_at TIMESTAMPTZ;
CREATE INDEX ban_sync_links_source_user ON ban_sync_links (source_guild, user_id);
