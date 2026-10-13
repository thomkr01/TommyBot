-- A claimed job stays in the table until it finishes; the lease hides it from other claimers,
-- and a crash lets it come back when the lease runs out.
ALTER TABLE scheduled_jobs
    ADD COLUMN locked_until TIMESTAMPTZ,
    ADD COLUMN attempts     INT NOT NULL DEFAULT 0;
