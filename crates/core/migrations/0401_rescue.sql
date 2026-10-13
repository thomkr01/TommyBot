CREATE TABLE rescue_key (guild_id BIGINT PRIMARY KEY, key_hash TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now());
