-- Commands waiting for their Confirm click, kept here so a restart does not lose them.
CREATE TABLE pending_confirms (
    nonce      BIGINT PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    actor_id   BIGINT NOT NULL,
    action     JSONB  NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
