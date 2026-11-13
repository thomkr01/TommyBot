-- N3: anti-nuke windows, permit command limits and the heat panic list survive a restart.
-- counted: how many actions an action_log row adds to its anti-nuke window (a merged voice entry
-- adds its growth; a second inverse row of one entry adds 0).
ALTER TABLE action_log ADD COLUMN counted INTEGER NOT NULL DEFAULT 1;

-- One row per command use counted against a permit limit; kept an hour.
CREATE TABLE command_uses (
    actor_id BIGINT NOT NULL,
    action TEXT NOT NULL,
    at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX command_uses_at ON command_uses (at);

-- The accounts heat panic times out on every message, until `until` (unix seconds).
CREATE TABLE heat_panic (
    guild_id BIGINT PRIMARY KEY,
    raiders BIGINT[] NOT NULL,
    until BIGINT NOT NULL
);
