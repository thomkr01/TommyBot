-- Web verification: a salted hash of the visitor's IP, kept 30 days, to flag alt accounts.
CREATE TABLE verify_ips (
    guild_id BIGINT NOT NULL,
    ip_hash  BYTEA  NOT NULL,
    user_id  BIGINT NOT NULL,
    at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, ip_hash, user_id)
);
CREATE INDEX verify_ips_at ON verify_ips (at);
