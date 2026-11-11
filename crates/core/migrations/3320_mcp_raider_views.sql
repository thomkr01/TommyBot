-- Raider List entries for the MCP server (see 2700), for the bot owner. Leaves out who reported
-- (reporter_id) and the evidence detail; only counts.
CREATE VIEW mcp_raiders AS
    SELECT r.user_id, r.status, r.category, r.verified, r.public_note, r.expires_at, r.appeal_after,
        r.created_at, r.updated_at,
        (SELECT count(*) FROM raider_evidence e WHERE e.user_id = r.user_id) AS evidence,
        (SELECT count(DISTINCT e.source_owner) FROM raider_evidence e WHERE e.user_id = r.user_id) AS owners,
        (SELECT count(*) FROM raider_appeals a WHERE a.user_id = r.user_id AND a.status = 'open') AS open_appeals
    FROM raiders r;

GRANT SELECT ON mcp_raiders TO tommybot_mcp;
