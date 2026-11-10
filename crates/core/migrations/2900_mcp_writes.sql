-- Changes the MCP server proposed (specs/mcp-plan.md, PR 4) and what became of them. The dashboard owns this
-- table; the tommybot_mcp reader role gets no grant. body is the request and is cleared once the change is
-- no longer pending, so only the audit trail (what, when, outcome) stays.
CREATE TABLE mcp_writes (
    id BIGSERIAL PRIMARY KEY,
    guild_id BIGINT NOT NULL,
    action TEXT NOT NULL,
    target TEXT,
    body JSONB,
    code_hash BYTEA NOT NULL,
    attempts INT NOT NULL DEFAULT 0,
    -- pending, running, done, failed, refused (too many wrong codes), expired, dm_failed
    status TEXT NOT NULL DEFAULT 'pending',
    http_status INT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    decided_at TIMESTAMPTZ
);
CREATE INDEX mcp_writes_pending ON mcp_writes (status) WHERE status = 'pending';
