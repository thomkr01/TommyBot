-- D20: the audit entry a row was counted from, so a catch-up after a crash skips entries it
-- already counted (the saved last id is written in batches and can lag).
ALTER TABLE action_log ADD COLUMN audit_id BIGINT;
CREATE INDEX action_log_audit_id ON action_log (guild_id, audit_id) WHERE audit_id IS NOT NULL;
