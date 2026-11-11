-- MCP write access (specs/mcp-plan.md, PR 4). The dashboard owns both tables; the tommybot_mcp reader role
-- gets no grant. A token is stored only as its SHA-256 (it is 32 random bytes, so a slow hash adds nothing).
CREATE TABLE mcp_tokens (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    token_hash BYTEA NOT NULL UNIQUE,
    created_by BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ
);

-- Changes a token proposed and what became of them. body is the request and is cleared once the change is
-- no longer pending, so only the audit trail (who, what, when, outcome) stays.
CREATE TABLE mcp_writes (
    id BIGSERIAL PRIMARY KEY,
    token_id BIGINT NOT NULL REFERENCES mcp_tokens (id),
    client TEXT NOT NULL,
    guild_id BIGINT NOT NULL,
    action TEXT NOT NULL,
    target TEXT,
    body JSONB,
    code_hash BYTEA NOT NULL,
    attempts INT NOT NULL DEFAULT 0,
    -- pending, running, done, failed, refused (too many wrong codes), expired, dm_failed, revoked
    status TEXT NOT NULL DEFAULT 'pending',
    http_status INT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    decided_at TIMESTAMPTZ
);
CREATE INDEX mcp_writes_pending ON mcp_writes (status) WHERE status = 'pending';
