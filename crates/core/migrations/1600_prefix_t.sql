-- The default prefix is now t!. Servers still on the old default move with it.
UPDATE guild_config SET config = jsonb_set(config, '{misc,prefix}', '"t!"')
WHERE config #>> '{misc,prefix}' = 'w!';
