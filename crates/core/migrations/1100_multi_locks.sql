-- Several lock types can be active together, one snapshot per kind.
ALTER TABLE lock_snapshots DROP CONSTRAINT lock_snapshots_pkey;
ALTER TABLE lock_snapshots ADD PRIMARY KEY (guild_id, kind);
