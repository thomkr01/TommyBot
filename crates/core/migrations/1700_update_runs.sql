-- Self-host updates started from the dashboard and carried out by the updater container.
CREATE TABLE update_runs (
    id              BIGSERIAL PRIMARY KEY,
    requested_by    BIGINT NOT NULL,
    -- queued, running, done, failed (nothing changed or rollback failed), rolled_back
    status          TEXT NOT NULL DEFAULT 'queued',
    -- 0 check, 1 download, 2 restart bot, 3 restart dashboard, 4 health check
    step            INT NOT NULL DEFAULT 0,
    -- Seconds each finished step took; done runs feed the time estimate.
    step_times      INT[] NOT NULL DEFAULT '{}',
    step_started_at TIMESTAMPTZ,
    to_sha          TEXT,
    error           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at     TIMESTAMPTZ
);
-- At most one update queued or running.
CREATE UNIQUE INDEX update_runs_one_active ON update_runs ((true)) WHERE status IN ('queued', 'running');

-- The updater's heartbeat, so the dashboard can tell whether anyone will pick a request up.
CREATE TABLE updater_seen (
    id BOOL PRIMARY KEY DEFAULT true CHECK (id),
    at TIMESTAMPTZ NOT NULL
);
