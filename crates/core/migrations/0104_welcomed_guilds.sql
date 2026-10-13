-- Servers that already got the welcome message, so a restart never sends it twice.
CREATE TABLE welcomed_guilds (
    guild_id BIGINT PRIMARY KEY,
    sent_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
