-- Moderator stats for the MCP server (see 2700, 2800).
CREATE VIEW mcp_mod_actions AS
    SELECT id, guild_id, mod_id, kind, source, target_id, case_no, created_at FROM mod_actions;

GRANT SELECT ON mcp_mod_actions TO tommybot_mcp;
