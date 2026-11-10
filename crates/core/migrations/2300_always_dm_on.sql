-- Punishment DMs are now on by default. Servers that saved the Misc page stored the old
-- default (off), so turn it on for them once.
UPDATE guild_config SET config = jsonb_set(config, '{misc,always_dm}', 'true')
WHERE jsonb_typeof(config -> 'misc') = 'object';
