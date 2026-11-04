-- A PNG preview, at most 256 px on its longest side, for image clips. Kept in the row so
-- deleting the clip deletes (and, with secure_delete, wipes) the thumbnail with it.
ALTER TABLE clip ADD COLUMN thumbnail BLOB;
