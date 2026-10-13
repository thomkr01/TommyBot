-- One row per nuke case (`NukeCase` as jsonb): who was quarantined, what was reverted.
CREATE TABLE nuke_incidents (
    guild_id   BIGINT NOT NULL,
    case_id    TEXT NOT NULL,
    data       JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, case_id)
);
