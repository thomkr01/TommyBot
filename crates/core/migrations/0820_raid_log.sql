-- Accounts a join raid caught; a raid is named by the millisecond it started.
CREATE TABLE raid_log (
    guild_id   BIGINT NOT NULL,
    raid_ms    BIGINT NOT NULL,
    user_id    BIGINT NOT NULL,
    name       TEXT   NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, raid_ms, user_id)
);
