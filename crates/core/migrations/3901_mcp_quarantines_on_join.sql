-- The MCP quarantines view shows whether a row is verification's join quarantine.
CREATE OR REPLACE VIEW mcp_quarantines AS
    SELECT guild_id, user_id, cardinality(saved_roles) AS saved_roles, created_at, on_join FROM quarantines;
