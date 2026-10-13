-- Roles saved when a member is quarantined, restored on release.
CREATE TABLE quarantines (
    guild_id    BIGINT NOT NULL,
    user_id     BIGINT NOT NULL,
    saved_roles BIGINT[] NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, user_id)
);
