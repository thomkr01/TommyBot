-- D20: the newest audit log entry the bot saw per guild, so after a restart or a new gateway
-- session it reads the entries made in the gap (at most 100, oldest first).
CREATE TABLE audit_seen (
    guild_id BIGINT PRIMARY KEY,
    last_id BIGINT NOT NULL
);
