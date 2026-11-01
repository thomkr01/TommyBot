-- Wick-style 6-char backup ids and who took the backup (NULL = automatic).
ALTER TABLE backups
    ADD COLUMN code TEXT NOT NULL DEFAULT substr(md5(random()::text), 1, 6),
    ADD COLUMN created_by BIGINT;
CREATE UNIQUE INDEX backups_guild_code ON backups (guild_id, code);
