-- Every Join Gate punishment (also join lock, join raid and native automod kicks), for the dashboard's
-- Join Gate records: what matched, whether the member is a bot and who added it, and whether Discord
-- took the action. Kept 30 days, at most 1000 rows per server.
CREATE TABLE joingate_actions (
    id         BIGSERIAL PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    user_id    BIGINT NOT NULL,
    is_bot     BOOLEAN NOT NULL DEFAULT false,
    adder_id   BIGINT,
    filters    JSONB NOT NULL DEFAULT '[]',
    reason     TEXT NOT NULL,
    action     TEXT NOT NULL,
    ok         BOOLEAN NOT NULL,
    error      TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX joingate_actions_guild ON joingate_actions (guild_id, id DESC);

CREATE FUNCTION joingate_actions_cap() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM joingate_actions WHERE guild_id = NEW.guild_id
        AND (created_at < now() - interval '30 days'
             OR id <= (SELECT id FROM joingate_actions WHERE guild_id = NEW.guild_id
                       ORDER BY id DESC OFFSET 1000 LIMIT 1));
    RETURN NULL;
END $$;

CREATE TRIGGER joingate_actions_cap AFTER INSERT ON joingate_actions
    FOR EACH ROW EXECUTE FUNCTION joingate_actions_cap();
