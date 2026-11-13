-- What a server's previous owner had set when its Discord owner changed (D47): the bot switches
-- Ban Sync off and removes their Extra Owners and Trusted Admins, and the new owner may keep them.
CREATE TABLE owner_handovers (
    guild_id        BIGINT PRIMARY KEY,
    old_owner       BIGINT NOT NULL,
    new_owner       BIGINT NOT NULL,
    extra_owners    BIGINT[] NOT NULL DEFAULT '{}',
    trusted_admins  BIGINT[] NOT NULL DEFAULT '{}',
    ban_sync_was_on BOOLEAN NOT NULL DEFAULT false,
    at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    kept_at         TIMESTAMPTZ
);
