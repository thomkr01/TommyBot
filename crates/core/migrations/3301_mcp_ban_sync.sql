-- Ban Sync copies for the MCP server (see 2700). The reason is already capped at 400 characters.
CREATE VIEW mcp_ban_sync AS
    SELECT guild_id, user_id, source_guild, source_case, origin, reason, expires_at, state, skip_reason, case_no,
        created_at, done_at
    FROM ban_sync_links;

GRANT SELECT ON mcp_ban_sync TO tommybot_mcp;
