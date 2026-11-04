-- Clip history tables. Timestamps are Unix milliseconds.

CREATE TABLE clip (
    -- AUTOINCREMENT: an id shown in a list must never come back pointing at another clip.
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    content_hash TEXT    NOT NULL UNIQUE,  -- BLAKE3 of every format, see store::content_hash
    kind         TEXT    NOT NULL CHECK (kind IN ('text', 'image', 'html', 'rtf', 'files')),
    preview      TEXT    NOT NULL,
    source_app   TEXT,
    byte_size    INTEGER NOT NULL,
    pinned       INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    use_count    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL
);

-- One row per format of a clip. Small payloads live inline; larger ones in the
-- content-addressed blob store, named by the BLAKE3 hash of the payload.
CREATE TABLE clip_format (
    id          INTEGER PRIMARY KEY,
    clip_id     INTEGER NOT NULL REFERENCES clip (id) ON DELETE CASCADE,
    mime        TEXT    NOT NULL,
    inline_data BLOB,
    blob_hash   TEXT,
    byte_size   INTEGER NOT NULL,
    CHECK ((inline_data IS NULL) <> (blob_hash IS NULL)),
    UNIQUE (clip_id, mime)
);

-- Full text of each clip's plain-text format; rowid = clip.id.
CREATE VIRTUAL TABLE clip_fts USING fts5 (body, tokenize = 'unicode61 remove_diacritics 2');
-- Remove deleted text from the index at once, instead of leaving it until segments merge.
INSERT INTO clip_fts (clip_fts, rank) VALUES ('secure-delete', 1);
CREATE TRIGGER clip_fts_delete AFTER DELETE ON clip BEGIN
    DELETE FROM clip_fts WHERE rowid = old.id;
END;

CREATE TABLE setting (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
