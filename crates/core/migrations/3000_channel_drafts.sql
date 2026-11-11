-- Channel Setup: the dashboard's unpushed channel changes, one draft per guild. Push runs it in Discord and deletes it.
CREATE TABLE channel_drafts (
    guild_id BIGINT PRIMARY KEY,
    draft JSONB NOT NULL,
    updated_by BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
