-- Read-only role for the MCP server (specs/mcp-plan.md). It sees only these views, never tables with
-- secrets (dash_session, dash_totp, rescue_key, verify_ips). The bot sets its password from MCP_DB_PASSWORD.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tommybot_mcp') THEN
        CREATE ROLE tommybot_mcp NOLOGIN CONNECTION LIMIT 5;
    END IF;
END $$;
ALTER ROLE tommybot_mcp SET default_transaction_read_only = on;
ALTER ROLE tommybot_mcp SET statement_timeout = '5s';

CREATE VIEW mcp_guilds AS
    SELECT DISTINCT guild_id FROM guild_config
    WHERE guild_id NOT IN (SELECT guild_id FROM guild_left);

CREATE VIEW mcp_guild_config AS
    SELECT guild_id, version, config, changed_by, created_at FROM guild_config;

CREATE VIEW mcp_cases AS
    SELECT guild_id, case_no, kind, user_id, mod_id, reason, proof, points, duration_s, expires_at, active, created_at
    FROM cases;

CREATE VIEW mcp_raid_log AS
    SELECT guild_id, raid_ms, user_id, name, code, created_at FROM raid_log;

CREATE VIEW mcp_joingate_log AS
    SELECT guild_id, user_id, is_bot, adder_id, filters, reason, action, ok, error, created_at FROM joingate_actions;

CREATE VIEW mcp_update_runs AS
    SELECT id, status, step, to_sha, error, created_at, finished_at FROM update_runs;

GRANT USAGE ON SCHEMA public TO tommybot_mcp;
GRANT SELECT ON mcp_guilds, mcp_guild_config, mcp_cases, mcp_raid_log, mcp_joingate_log, mcp_update_runs TO tommybot_mcp;
