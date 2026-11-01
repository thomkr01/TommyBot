-- Messages posted with `lock ... ?t update`; the announcement shows the newest five.
CREATE TABLE lock_updates (
    id         BIGSERIAL PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    author_id  BIGINT NOT NULL,
    message    TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX lock_updates_guild ON lock_updates (guild_id, id DESC);
