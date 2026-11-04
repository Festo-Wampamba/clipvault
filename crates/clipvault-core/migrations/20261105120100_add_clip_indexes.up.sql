-- History is always listed newest first.
CREATE INDEX clip_recent ON clip (last_used_at DESC);
-- Blob garbage collection asks "is this blob still referenced?".
CREATE INDEX clip_format_blob ON clip_format (blob_hash) WHERE blob_hash IS NOT NULL;
