-- D47 (S-h3 round 2): the quarantine whitelist is an exemption by user id, so an ownership change
-- clears it like EO/TA and keeps the ids here for the new owner to keep one by one.
ALTER TABLE owner_handovers ADD COLUMN quarantine_whitelist BIGINT[] NOT NULL DEFAULT '{}';
