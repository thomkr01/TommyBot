-- Read-only view for the MCP server (see 2700), so the scan threshold can be tuned from real scores.
-- The image hash is left out.
CREATE VIEW mcp_nsfw_scans AS
    SELECT id, guild_id, channel_id, message_id, user_id, scores, score, threshold, hit, feedback, at FROM nsfw_scans;
GRANT SELECT ON mcp_nsfw_scans TO tommybot_mcp;
