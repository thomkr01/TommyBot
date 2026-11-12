-- NSFW image scan results: an image hash and the model's scores, never the image or its URL.
-- Kept 30 days (the bot's sweep deletes older rows). feedback: 'fp' (staff: not NSFW, that hash is then
-- allowed in the server) or 'ok' (staff: correct).
CREATE TABLE nsfw_scans (
    id          BIGSERIAL PRIMARY KEY,
    guild_id    BIGINT NOT NULL,
    channel_id  BIGINT NOT NULL,
    message_id  BIGINT NOT NULL,
    user_id     BIGINT NOT NULL,
    sha256      BYTEA NOT NULL,
    -- drawings, hentai, neutral, porn, sexy
    scores      REAL[] NOT NULL,
    score       REAL NOT NULL,
    threshold   REAL NOT NULL,
    hit         BOOLEAN NOT NULL,
    feedback    TEXT CHECK (feedback IN ('fp', 'ok')),
    feedback_by BIGINT,
    at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (guild_id, message_id, sha256)
);
CREATE INDEX nsfw_scans_allowed ON nsfw_scans (guild_id, sha256) WHERE feedback = 'fp';
CREATE INDEX nsfw_scans_at ON nsfw_scans (at);
