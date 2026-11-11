-- What the owner was shown in the DM (SHA-256 of the preview). confirm_change recomputes the preview and refuses
-- when it differs, so a change runs only as it was reviewed (PITFALLS: "An action ran something other than what was reviewed").
-- A change refused for that gets status 'stale'.
ALTER TABLE mcp_writes ADD COLUMN preview_hash BYTEA;
