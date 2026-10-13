-- Server images (roles, channels, overwrites) taken every 3 hours and on demand.
CREATE TABLE backups (
    id         BIGSERIAL PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    image      JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX backups_guild_created ON backups (guild_id, created_at DESC);
