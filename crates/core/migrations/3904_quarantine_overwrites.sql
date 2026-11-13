-- D14: a quarantine also takes channel access the member has through their own overwrites or a
-- managed role's (booster, bot role): a member overwrite denies those bits. The overwrite it
-- replaced (had = false: none) is put back when the quarantine is lifted.
CREATE TABLE quarantine_overwrites (
    guild_id BIGINT NOT NULL,
    user_id BIGINT NOT NULL,
    channel_id BIGINT NOT NULL,
    had BOOLEAN NOT NULL,
    allow BIGINT NOT NULL,
    deny BIGINT NOT NULL,
    PRIMARY KEY (guild_id, user_id, channel_id)
);
