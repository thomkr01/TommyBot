-- Raider List: a global list of raider accounts the bot caught, reviewed by the bot owner.
-- Global tables use source_guild (not guild_id) so purge_guild() keeps list evidence.
CREATE TABLE raiders (
    user_id BIGINT PRIMARY KEY,
    status TEXT NOT NULL CHECK (status IN ('pending', 'confirmed', 'rejected', 'removed')),
    category TEXT NOT NULL CHECK (category IN ('nuke', 'raid', 'scam', 'spam')),
    -- false while an automatic listing waits for the bot owner's check.
    verified BOOLEAN NOT NULL DEFAULT false,
    public_note TEXT NOT NULL DEFAULT '' CHECK (length(public_note) <= 200),
    reviewed_by BIGINT,
    reviewed_at TIMESTAMPTZ,
    -- Scam listings end by themselves (hacked accounts get recovered).
    expires_at TIMESTAMPTZ,
    appeal_after TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX raiders_status ON raiders (status, updated_at);

-- detail holds counts, ids and timestamps only, never message text.
CREATE TABLE raider_evidence (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    source_guild BIGINT NOT NULL,
    source_owner BIGINT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('auto_nuke', 'auto_raid', 'auto_scam', 'auto_spam', 'report')),
    ref TEXT NOT NULL,
    detail JSONB NOT NULL DEFAULT '{}',
    reporter_id BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, source_guild, kind, ref)
);
CREATE INDEX raider_evidence_user ON raider_evidence (user_id, created_at);
CREATE INDEX raider_evidence_reporter ON raider_evidence (source_owner, kind, created_at);

-- Append-only trail of every review decision.
CREATE TABLE raider_reviews (
    id BIGSERIAL PRIMARY KEY,
    actor BIGINT NOT NULL,
    user_id BIGINT NOT NULL,
    from_status TEXT,
    to_status TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX raider_reviews_user ON raider_reviews (user_id, at);

CREATE TABLE raider_appeals (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    text TEXT NOT NULL CHECK (length(text) BETWEEN 1 AND 1000),
    status TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'accepted', 'denied')),
    decided_by BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    decided_at TIMESTAMPTZ
);
CREATE UNIQUE INDEX raider_appeals_one_open ON raider_appeals (user_id) WHERE status = 'open';

-- Per server (guild_id, so purge_guild() removes them): what the list did here, and who staff let back in.
CREATE TABLE raider_actions (
    guild_id BIGINT NOT NULL,
    user_id BIGINT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('alert', 'quarantine', 'kick', 'ban')),
    case_no INT,
    -- Set just before the lift asks Discord to unban, so a retry that finds no ban knows it was ours.
    unbanning BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, user_id)
);
CREATE TABLE raider_allow (
    guild_id BIGINT NOT NULL,
    user_id BIGINT NOT NULL,
    by BIGINT NOT NULL,
    at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, user_id)
);
