-- Per-user ticket DM state: ask before sending, and the server their last ticket message went to.
CREATE TABLE ticket_users (
    user_id      BIGINT PRIMARY KEY,
    confirmation BOOLEAN NOT NULL DEFAULT true,
    last_guild   BIGINT
);

-- One open ticket per member and server: tickets are now opened by DM and found by member.
UPDATE tickets t SET closed_at = now()
WHERE closed_at IS NULL AND EXISTS (
    SELECT 1 FROM tickets u
    WHERE u.guild_id = t.guild_id AND u.user_id = t.user_id AND u.closed_at IS NULL AND u.number > t.number
);
DROP INDEX tickets_open;
CREATE UNIQUE INDEX tickets_open ON tickets (guild_id, user_id) WHERE closed_at IS NULL;
