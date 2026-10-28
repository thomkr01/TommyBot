CREATE TABLE tickets (
    guild_id   BIGINT NOT NULL,
    number     INT    NOT NULL,
    -- NULL until the channel exists.
    channel_id BIGINT UNIQUE,
    user_id    BIGINT NOT NULL,
    opened_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at  TIMESTAMPTZ,
    closed_by  BIGINT,
    PRIMARY KEY (guild_id, number)
);
CREATE INDEX tickets_open ON tickets (guild_id, user_id) WHERE closed_at IS NULL;
