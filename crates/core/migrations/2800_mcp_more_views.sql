-- More read-only views for the MCP server (see 2700). Left out on purpose: reported message content,
-- appeal answers, job payloads, backup images and saved roles (personal data or bulky snapshots).
CREATE VIEW mcp_action_log AS
    SELECT id, guild_id, actor_id, action, target_id, case_no, reverted, created_at FROM action_log;

CREATE VIEW mcp_nuke_incidents AS
    SELECT guild_id, case_id, created_at,
        jsonb_array_length(coalesce(data->'quarantined', '[]')) AS quarantined,
        jsonb_array_length(coalesce(data->'reverted', '[]')) AS reverted,
        jsonb_array_length(coalesce(data->'failed', '[]')) AS failed
    FROM nuke_incidents;

CREATE VIEW mcp_quarantines AS
    SELECT guild_id, user_id, cardinality(saved_roles) AS saved_roles, created_at FROM quarantines;

CREATE VIEW mcp_strikes AS
    SELECT guild_id, user_id, strikes, last_strike_ms, cap_end_ms FROM automod_strikes;

CREATE VIEW mcp_appeals AS
    SELECT id, guild_id, user_id, kind, status, decided_by, created_at FROM appeals;

CREATE VIEW mcp_reports AS
    SELECT id, guild_id, reporter_id, target_id, channel_id, reason, status, action, handled_by, handled_at, created_at
    FROM reports;

CREATE VIEW mcp_notes AS
    SELECT id, short_id, guild_id, user_id, author_id, text, created_at FROM notes;

CREATE VIEW mcp_jobs AS
    SELECT id, guild_id, kind, run_at, attempts, locked_until FROM scheduled_jobs;

CREATE VIEW mcp_backups AS
    SELECT id, guild_id, code, created_by, created_at FROM backups;

GRANT SELECT ON mcp_action_log, mcp_nuke_incidents, mcp_quarantines, mcp_strikes, mcp_appeals, mcp_reports,
    mcp_notes, mcp_jobs, mcp_backups TO tommybot_mcp;
