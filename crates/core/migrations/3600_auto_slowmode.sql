-- Auto slowmode: one row per channel the bot slowed, until its slowmode_off job reverts it.
CREATE TABLE auto_slowmode (
    channel_id BIGINT PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    prior_secs INT    NOT NULL,
    set_secs   INT    NOT NULL,
    state      TEXT   NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'on')),
    -- Staff changed the slowmode while the row was pending: put this value back.
    staff_secs INT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX auto_slowmode_guild ON auto_slowmode (guild_id);
