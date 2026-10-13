-- Heat strikes survive restarts so the reoffend multiplier keeps working.
CREATE TABLE automod_strikes (
    guild_id             BIGINT NOT NULL,
    user_id              BIGINT NOT NULL,
    strikes              INT    NOT NULL,
    last_strike_ms       BIGINT NOT NULL,
    last_cap_duration_ms BIGINT NOT NULL,
    cap_end_ms           BIGINT NOT NULL,
    PRIMARY KEY (guild_id, user_id)
);

-- Exact pre-lock state, restored on unlock (spec 4.5). One active lock per guild.
CREATE TABLE lock_snapshots (
    guild_id   BIGINT PRIMARY KEY,
    kind       TEXT  NOT NULL,
    snapshot   JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
