-- The sample messages the bot builds at startup; the dashboard's previews show them.
CREATE TABLE bot_previews (
    id BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    messages JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
