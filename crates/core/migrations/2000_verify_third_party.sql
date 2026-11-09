-- The third-party verification mode was the website mode under another name; it is now just Website.
UPDATE guild_config SET config = jsonb_set(config, '{verification,mode}', '"Web"')
WHERE config #>> '{verification,mode}' = 'ThirdParty';
