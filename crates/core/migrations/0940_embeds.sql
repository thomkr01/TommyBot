-- Embed builder: each embed is a draft until the bot posts it; the posted message is then edited in place.
CREATE TABLE embeds (
    id              BIGSERIAL PRIMARY KEY,
    guild_id        BIGINT NOT NULL,
    name            TEXT   NOT NULL,
    embed           JSONB  NOT NULL,
    channel_id      BIGINT,
    -- where the bot's copy lives; can differ from channel_id until the next post moves it
    message_channel BIGINT,
    message_id      BIGINT,
    updated_by      BIGINT NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX embeds_guild ON embeds (guild_id);
