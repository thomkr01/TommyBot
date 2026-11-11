-- One row per staff action, append-only, for moderator stats. Pruned after 365 days by the bot.
-- No foreign key to cases: a row must survive the case being deleted.
CREATE TABLE mod_actions (
    id         BIGSERIAL PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    mod_id     BIGINT NOT NULL,
    kind       TEXT   NOT NULL,
    source     TEXT   NOT NULL,
    target_id  BIGINT,
    case_no    INT,
    audit_id   BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX mod_actions_audit ON mod_actions (guild_id, audit_id) WHERE audit_id IS NOT NULL;
CREATE INDEX mod_actions_mod ON mod_actions (guild_id, mod_id, created_at DESC);
CREATE INDEX mod_actions_time ON mod_actions (guild_id, created_at);

-- Cases from the last year become history; cases deleted before now are gone.
INSERT INTO mod_actions (guild_id, mod_id, kind, source, target_id, case_no, created_at)
SELECT guild_id, mod_id, kind, 'bot', user_id, case_no, created_at
FROM cases WHERE created_at > now() - interval '365 days';
