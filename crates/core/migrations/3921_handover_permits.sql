-- D47: the previous owner's immunity and dashboard-edit permits are paused with EO/TA. Each
-- paused (permit, role) pair is kept here until the new owner keeps it; `other_permits` names the
-- weaker permits that stayed on, so the card and the Ban Sync page can list them.
ALTER TABLE owner_handovers
    ADD COLUMN paused_roles  JSONB  NOT NULL DEFAULT '[]',
    ADD COLUMN other_permits TEXT[] NOT NULL DEFAULT '{}';
