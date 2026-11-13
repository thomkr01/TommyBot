-- Ban Sync is removed (Thomas 2026-11-13 22:50; Raider List stays). Copies already made stay as
-- normal bans in Discord. guild_owners stays: Raider List and the owner handover (D47) read it.
DROP VIEW IF EXISTS mcp_ban_sync;
DROP TABLE IF EXISTS ban_sync_links;
DELETE FROM scheduled_jobs WHERE kind LIKE 'ban_sync_%';
ALTER TABLE owner_handovers DROP COLUMN IF EXISTS ban_sync_was_on;
-- The section is gone from the config; old versions keep it as history only.
