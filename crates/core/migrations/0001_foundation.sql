-- Per-guild config, versioned: the newest row is current (spec 3.3).
CREATE TABLE guild_config (
    guild_id   BIGINT NOT NULL,
    version    INT    NOT NULL,
    config     JSONB  NOT NULL DEFAULT '{}',
    changed_by BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, version)
);

CREATE TABLE extra_owners (
    guild_id BIGINT NOT NULL,
    user_id  BIGINT NOT NULL,
    PRIMARY KEY (guild_id, user_id)
);

CREATE TABLE trusted_admins (
    guild_id BIGINT NOT NULL,
    user_id  BIGINT NOT NULL,
    PRIMARY KEY (guild_id, user_id)
);

-- Custom permits attach to roles; `rank` orders the hierarchy (higher acts on lower).
CREATE TABLE permits (
    id        BIGSERIAL PRIMARY KEY,
    guild_id  BIGINT NOT NULL,
    name      TEXT   NOT NULL,
    rank      INT    NOT NULL DEFAULT 0,
    perms     TEXT[] NOT NULL DEFAULT '{}',
    -- command rate limits, e.g. {"ban": {"per_min": 5, "per_hour": 20}}
    limits    JSONB  NOT NULL DEFAULT '{}',
    whitelist TEXT[] NOT NULL DEFAULT '{}',
    UNIQUE (guild_id, name)
);

CREATE TABLE permit_roles (
    permit_id BIGINT NOT NULL REFERENCES permits(id) ON DELETE CASCADE,
    guild_id  BIGINT NOT NULL,
    role_id   BIGINT NOT NULL,
    PRIMARY KEY (permit_id, role_id)
);

CREATE TABLE cases (
    guild_id   BIGINT NOT NULL,
    case_no    INT    NOT NULL,
    kind       TEXT   NOT NULL,
    user_id    BIGINT NOT NULL,
    mod_id     BIGINT NOT NULL,
    reason     TEXT,
    proof      TEXT,
    points     INT    NOT NULL DEFAULT 0,
    duration_s BIGINT,
    expires_at TIMESTAMPTZ,
    active     BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, case_no)
);
CREATE INDEX cases_user ON cases (guild_id, user_id);

CREATE TABLE warn_thresholds (
    guild_id   BIGINT NOT NULL,
    points     INT    NOT NULL,
    action     TEXT   NOT NULL,
    duration_s BIGINT,
    PRIMARY KEY (guild_id, points)
);

-- Every mutating action with its inverse, for /revert (spec 2.5.2).
CREATE TABLE action_log (
    id         BIGSERIAL PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    actor_id   BIGINT NOT NULL,
    action     TEXT   NOT NULL,
    target_id  BIGINT,
    inverse    JSONB,
    case_no    INT,
    reverted   BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX action_log_actor ON action_log (guild_id, actor_id, created_at DESC);

-- Timers that must survive restarts (tempbans, timeouts, lock expiry); one scheduler re-arms at boot.
CREATE TABLE scheduled_jobs (
    id        BIGSERIAL PRIMARY KEY,
    guild_id  BIGINT NOT NULL,
    kind      TEXT   NOT NULL,
    payload   JSONB  NOT NULL DEFAULT '{}',
    run_at    TIMESTAMPTZ NOT NULL
);
CREATE INDEX scheduled_jobs_run_at ON scheduled_jobs (run_at);
