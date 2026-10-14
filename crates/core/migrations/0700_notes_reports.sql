CREATE TABLE notes (
    id         BIGSERIAL PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    user_id    BIGINT NOT NULL,
    author_id  BIGINT NOT NULL,
    text       TEXT   NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX notes_user ON notes (guild_id, user_id);

CREATE TABLE reports (
    id          BIGSERIAL PRIMARY KEY,
    guild_id    BIGINT NOT NULL,
    reporter_id BIGINT NOT NULL,
    target_id   BIGINT NOT NULL,
    channel_id  BIGINT NOT NULL,
    message_id  BIGINT NOT NULL,
    content     TEXT   NOT NULL,
    status      TEXT   NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'dismissed', 'actioned')),
    action      TEXT,
    handled_by  BIGINT,
    handled_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX reports_guild ON reports (guild_id, created_at DESC);
-- One report per reporter and message.
CREATE UNIQUE INDEX reports_once ON reports (guild_id, reporter_id, message_id);

CREATE TABLE report_blacklist (
    guild_id BIGINT NOT NULL,
    user_id  BIGINT NOT NULL,
    added_by BIGINT NOT NULL,
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, user_id)
);
