-- New TOTP enrollments must also echo a code the bot DMs the Discord account, so a stolen session can't enroll.
ALTER TABLE dash_totp ADD COLUMN dm_code TEXT;

-- Keep the newest 200 config versions per guild (what the history page lists); older ones are pruned on insert.
CREATE FUNCTION guild_config_cap() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM guild_config WHERE guild_id = NEW.guild_id AND version <= NEW.version - 200;
    RETURN NULL;
END $$;

CREATE TRIGGER guild_config_cap AFTER INSERT ON guild_config
    FOR EACH ROW EXECUTE FUNCTION guild_config_cap();
