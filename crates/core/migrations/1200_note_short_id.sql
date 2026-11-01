-- Wick-style 6-char base36 note ids; the default also covers dashboard inserts.
CREATE FUNCTION note_short_id() RETURNS TEXT LANGUAGE sql VOLATILE AS $$
    SELECT string_agg(substr('0123456789abcdefghijklmnopqrstuvwxyz', 1 + floor(random() * 36)::int, 1), '')
    FROM generate_series(1, 6)
$$;
ALTER TABLE notes ADD COLUMN short_id TEXT;
UPDATE notes SET short_id = note_short_id();
ALTER TABLE notes ALTER COLUMN short_id SET NOT NULL, ALTER COLUMN short_id SET DEFAULT note_short_id();
CREATE UNIQUE INDEX notes_short_id ON notes (guild_id, short_id);
