-- When the bot was removed from a guild; its data is deleted 30 days later unless the bot comes back.
CREATE TABLE guild_left (
    guild_id BIGINT PRIMARY KEY,
    left_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Deletes everything stored for a guild: every public table with a guild_id column, found at call
-- time so new tables are covered too. The NOTIFY makes the bot drop its cached config.
CREATE FUNCTION purge_guild(g BIGINT) RETURNS void LANGUAGE plpgsql AS $$
DECLARE t TEXT;
BEGIN
    FOR t IN
        SELECT c.table_name FROM information_schema.columns c
        JOIN information_schema.tables x USING (table_schema, table_name)
        WHERE c.table_schema = 'public' AND c.column_name = 'guild_id'
          AND x.table_type = 'BASE TABLE' AND c.table_name <> '_sqlx_migrations'
    LOOP
        EXECUTE format('DELETE FROM %I WHERE guild_id = $1', t) USING g;
    END LOOP;
    UPDATE ticket_users SET last_guild = NULL WHERE last_guild = g;
    PERFORM pg_notify('config_changed', g::text);
END $$;
