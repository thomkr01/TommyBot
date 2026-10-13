CREATE TABLE appeals (
    id         BIGSERIAL PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    user_id    BIGINT NOT NULL,
    username   TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('ban', 'timeout')),
    answers    JSONB NOT NULL,
    status     TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'accepted', 'denied')),
    decided_by BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX appeals_guild ON appeals (guild_id, created_at DESC);
-- One open appeal per user and server.
CREATE UNIQUE INDEX appeals_one_open ON appeals (guild_id, user_id) WHERE status = 'open';

CREATE TABLE appeal_blacklist (
    guild_id BIGINT NOT NULL,
    user_id  BIGINT NOT NULL,
    added_by BIGINT NOT NULL,
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, user_id)
);
