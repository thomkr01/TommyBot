-- Sticky roles: the roles of each member, keyed by membership (joined_at), kept 30 days after they leave.
CREATE TABLE member_roles (
    guild_id   BIGINT NOT NULL,
    user_id    BIGINT NOT NULL,
    roles      BIGINT[] NOT NULL,
    joined_at  TIMESTAMPTZ NOT NULL,
    left_at    TIMESTAMPTZ,
    pending    BOOLEAN NOT NULL DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, user_id)
);
CREATE INDEX member_roles_left ON member_roles (left_at) WHERE left_at IS NOT NULL;
