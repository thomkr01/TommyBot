-- D20: every audit entry anti-nuke handled, counted or not (tripwires, lock set-backs, voice,
-- exempt entries), so a catch-up after a crash or restart never runs one twice. Pruned after
-- Discord's 45-day audit log window.
CREATE TABLE audit_handled (
    guild_id BIGINT NOT NULL,
    audit_id BIGINT NOT NULL,
    at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, audit_id)
);
CREATE INDEX audit_handled_at ON audit_handled (at);
