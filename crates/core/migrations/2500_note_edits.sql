-- Every change to a note's text, so the dashboard can show the full history (the note keeps its created_at).
CREATE TABLE note_edits (
    id        BIGSERIAL PRIMARY KEY,
    note_id   BIGINT NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
    guild_id  BIGINT NOT NULL,
    old_text  TEXT   NOT NULL,
    new_text  TEXT   NOT NULL,
    editor_id BIGINT NOT NULL,
    edited_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX note_edits_note ON note_edits (note_id);
