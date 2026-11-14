-- Thomas Q5 (2026-11-14): roles Discord hands out by itself carry no permit in any permit lookup.
-- `source` is 'tags' (linked roles, server subscription or purchase roles) or 'onboarding' (roles
-- an Onboarding prompt gives); each is replaced as a whole when the bot or dashboard reads it.
-- `reported`: the anti-nuke log already said a permit on this role is ignored.
CREATE TABLE unpermittable_roles (
    guild_id BIGINT NOT NULL,
    role_id BIGINT NOT NULL,
    source TEXT NOT NULL,
    reported BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (guild_id, role_id, source)
);
