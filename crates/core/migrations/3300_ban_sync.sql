-- Ban Sync: servers grouped by their Discord owner, and one row per ban copied between them.
-- guild_owners is a candidate list only; the bot checks the owner live before every copy.
CREATE TABLE guild_owners (
    guild_id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL,
    seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX guild_owners_owner ON guild_owners (owner_id);

-- guild_id = the receiving server (so purge_guild() removes its rows), source_guild = where the ban was made.
CREATE TABLE ban_sync_links (
    guild_id BIGINT NOT NULL,
    user_id BIGINT NOT NULL,
    source_guild BIGINT NOT NULL,
    source_case INT,
    origin TEXT NOT NULL CHECK (origin IN ('staff_cmd', 'discord', 'joinraid', 'automod', 'joingate', 'verify')),
    reason TEXT NOT NULL DEFAULT '' CHECK (length(reason) <= 400),
    expires_at TIMESTAMPTZ,
    state TEXT NOT NULL CHECK (state IN ('pending', 'banned', 'alerted', 'skipped', 'cancelled', 'lifted')),
    skip_reason TEXT,
    case_no INT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    done_at TIMESTAMPTZ,
    PRIMARY KEY (guild_id, user_id, source_guild)
);
CREATE INDEX ban_sync_links_source ON ban_sync_links (source_guild, created_at);
