-- When the bot was last up (written every minute), so a startup knows when the downtime began.
CREATE TABLE bot_seen (
    id BOOL PRIMARY KEY DEFAULT true CHECK (id),
    at TIMESTAMPTZ NOT NULL
);
