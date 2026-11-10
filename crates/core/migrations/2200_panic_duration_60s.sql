-- Panic mode now lasts 60 seconds by default. Servers still on the old 600 s default move with it.
UPDATE guild_config SET config = jsonb_set(config, '{panic,duration_secs}', '60')
WHERE config #> '{panic,duration_secs}' = '600'::jsonb;
