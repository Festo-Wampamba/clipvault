//! The clip history: SQLite for metadata, full-text search and small payloads, plus a
//! content-addressed blob store for large payloads such as images.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::RegexBuilder;
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, ToSql, TransactionBehavior, params};

use crate::pipeline::app_key;
use crate::{Error, Result};

const PRIVATE_MODE_KEY: &str = "private_mode";

pub const TEXT_MIME: &str = "text/plain;charset=utf-8";
pub const PNG_MIME: &str = "image/png";
const HTML_MIME: &str = "text/html";
const RTF_MIME: &str = "text/rtf";
const URI_LIST_MIME: &str = "text/uri-list";

/// Payloads up to this size live in the database; larger ones in the blob store.
const INLINE_LIMIT: usize = 64 * 1024;
const PREVIEW_CHARS: usize = 500;
/// Caps a compiled search pattern so a pathological regex cannot exhaust memory.
const REGEX_SIZE_LIMIT: usize = 1 << 20;
const SUMMARY_COLUMNS: &str =
    "id, kind, preview, source_app, byte_size, pinned, use_count, created_at, last_used_at";

pub type ClipId = i64;

/// One representation of a clip, e.g. its plain text or its PNG image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClip {
    pub formats: Vec<Format>,
    pub source_app: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
    Html,
    Rtf,
    Files,
}

impl Kind {
    /// Lower-case name, as stored and as shown by `clipvault list`.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Image => "image",
            Kind::Html => "html",
            Kind::Rtf => "rtf",
            Kind::Files => "files",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Kind::Text, Kind::Image, Kind::Html, Kind::Rtf, Kind::Files]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }
}

/// What the history list shows for a clip; fetch the payload with [`Store::formats`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipSummary {
    pub id: ClipId,
    pub kind: Kind,
    pub preview: String,
    pub source_app: Option<String>,
    pub byte_size: u64,
    pub pinned: bool,
    pub use_count: u64,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds; re-copying a clip moves this forward.
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    Inserted(ClipId),
    /// The same content was already stored; it moved to the top instead.
    Bumped(ClipId),
}

/// History limits. `None` means unlimited. Pinned clips never count and are never removed.
/// `Some(0)` means zero, i.e. remove every unpinned clip; a settings UI where "0" means
/// "keep forever" (README §7) must map it to `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_items: Option<u64>,
    pub max_age: Option<Duration>,
    pub max_total_bytes: Option<u64>,
}

impl Default for Limits {
    /// The defaults from README §7.
    fn default() -> Self {
        Self {
            max_items: Some(500),
            max_age: Some(Duration::from_secs(30 * 24 * 60 * 60)),
            max_total_bytes: Some(1024 * 1024 * 1024),
        }
    }
}

pub struct Store {
    conn: Connection,
    blob_dir: PathBuf,
}

impl Store {
    /// Opens the history in `dir` (`clipvault.db` plus `blobs/`), creating it if needed.
    /// On Unix the folders are made private (0700): clipboard history holds secrets.
    pub fn open(dir: &Path) -> Result<Self> {
        create_private_dir(dir)?;
        let blob_dir = dir.join("blobs");
        create_private_dir(&blob_dir)?;
        let mut conn = Connection::open(dir.join("clipvault.db"))?;
        // First, so even the pragmas below wait for a concurrent ClipVault process.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // Zero deleted content instead of leaving it in free pages: history holds passwords.
        // Not stored in the file, so it is set on every connection.
        conn.pragma_update(None, "secure_delete", true)?;
        crate::migrate::migrate_up(&mut conn)?;
        let mut store = Self { conn, blob_dir };
        // Wipe image files a crash left between committing a delete and unlinking them.
        // One unremovable file must not lock the user out of their history, so the error
        // is not returned here; `prune` runs the same sweep and reports it.
        let _ = store.remove_orphan_blobs();
        Ok(store)
    }

    /// Stores `clip`; if the same content is already stored, moves it to the top instead.
    /// `now` is Unix milliseconds.
    pub fn add(&mut self, clip: &NewClip, now: i64) -> Result<Added> {
        if clip.formats.is_empty() {
            return Err(Error::EmptyClip);
        }
        let hash = content_hash(&clip.formats);
        // IMMEDIATE takes the write lock up front: blobs are written only while holding it,
        // which is what lets delete_where and remove_orphan_blobs unlink files safely.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<ClipId> = tx
            .query_row(
                "SELECT id FROM clip WHERE content_hash = ?1",
                [&hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            tx.execute(
                "UPDATE clip SET last_used_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            tx.commit()?;
            return Ok(Added::Bumped(id));
        }

        let byte_size: usize = clip.formats.iter().map(|f| f.data.len()).sum();
        tx.execute(
            "INSERT INTO clip (content_hash, kind, preview, source_app, byte_size, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                hash,
                kind_of(&clip.formats).as_str(),
                preview_of(&clip.formats),
                clip.source_app,
                to_i64(byte_size as u64),
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        for format in &clip.formats {
            let size = to_i64(format.data.len() as u64);
            if format.data.len() <= INLINE_LIMIT {
                tx.execute(
                    "INSERT INTO clip_format (clip_id, mime, inline_data, byte_size) VALUES (?1, ?2, ?3, ?4)",
                    params![id, format.mime, format.data, size],
                )?;
            } else {
                let blob_hash = write_blob(&self.blob_dir, &format.data)?;
                tx.execute(
                    "INSERT INTO clip_format (clip_id, mime, blob_hash, byte_size) VALUES (?1, ?2, ?3, ?4)",
                    params![id, format.mime, blob_hash, size],
                )?;
            }
        }
        if let Some(text) = text_of(&clip.formats) {
            tx.execute(
                "INSERT INTO clip_fts (rowid, body) VALUES (?1, ?2)",
                params![id, text],
            )?;
        }
        tx.commit()?;
        Ok(Added::Inserted(id))
    }

    /// Newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<ClipSummary>> {
        let sql = format!(
            "SELECT {SUMMARY_COLUMNS} FROM clip ORDER BY last_used_at DESC, id DESC LIMIT ?1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([to_i64(limit as u64)], summary_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `/pattern/` searches clip text with a case-insensitive regex; anything else matches
    /// clips containing every word as a prefix ("inv 20" finds "invoice 2025"). Newest first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<ClipSummary>> {
        let query = query.trim();
        let pattern = query
            .strip_prefix('/')
            .and_then(|q| q.strip_suffix('/'))
            .filter(|p| !p.is_empty());
        if let Some(pattern) = pattern {
            return self.regex_search(pattern, limit);
        }
        // Each word becomes a quoted prefix term, so FTS5 operators in the query are literal text.
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
            .collect();
        if terms.is_empty() {
            return self.recent(limit);
        }
        let sql = format!(
            "SELECT {SUMMARY_COLUMNS} FROM clip
             WHERE id IN (SELECT rowid FROM clip_fts WHERE clip_fts MATCH ?1)
             ORDER BY last_used_at DESC, id DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![terms.join(" "), to_i64(limit as u64)],
            summary_from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ponytail: linear scan of every clip's text, fine for the default 500-item history;
    // add a regex-capable index if limits grow by orders of magnitude.
    fn regex_search(&self, pattern: &str, limit: usize) -> Result<Vec<ClipSummary>> {
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()?;
        let sql = format!(
            "SELECT {SUMMARY_COLUMNS}, clip_fts.body FROM clip JOIN clip_fts ON clip_fts.rowid = clip.id
             ORDER BY last_used_at DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut found = Vec::new();
        while let Some(row) = rows.next()? {
            if found.len() == limit {
                break;
            }
            let body: String = row.get(9)?;
            if regex.is_match(&body) {
                found.push(summary_from_row(row)?);
            }
        }
        Ok(found)
    }

    /// Every stored format of a clip, in the order they were added.
    pub fn formats(&self, id: ClipId) -> Result<Vec<Format>> {
        let mut stmt = self.conn.prepare(
            "SELECT mime, inline_data, blob_hash FROM clip_format WHERE clip_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<Vec<u8>>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut formats = Vec::new();
        for row in rows {
            let (mime, inline, blob_hash) = row?;
            let data = match (inline, blob_hash) {
                (Some(data), _) => data,
                (None, Some(hash)) => fs::read(blob_path(&self.blob_dir, &hash)?)?,
                (None, None) => unreachable!("clip_format CHECK requires inline_data or blob_hash"),
            };
            formats.push(Format { mime, data });
        }
        // Every stored clip has at least one format, so none means no such clip.
        if formats.is_empty() {
            return Err(Error::NotFound(id));
        }
        Ok(formats)
    }

    pub fn set_pinned(&mut self, id: ClipId, pinned: bool) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE clip SET pinned = ?1 WHERE id = ?2",
            params![pinned, id],
        )?;
        if changed == 0 {
            return Err(Error::NotFound(id));
        }
        Ok(())
    }

    pub fn delete(&mut self, id: ClipId) -> Result<()> {
        if self.delete_where("id = ?1", &[&id])? == 0 {
            return Err(Error::NotFound(id));
        }
        Ok(())
    }

    /// Removes every clip that is not pinned. Returns how many were removed.
    pub fn clear_unpinned(&mut self) -> Result<usize> {
        self.delete_where("pinned = 0", &[])
    }

    /// Applies `limits` to unpinned clips, oldest first, and removes leftover blob files.
    /// `now` is Unix milliseconds. Returns how many clips were removed.
    pub fn prune(&mut self, limits: &Limits, now: i64) -> Result<usize> {
        let mut removed = 0;
        if let Some(max_age) = limits.max_age {
            let max_age_ms = u64::try_from(max_age.as_millis()).unwrap_or(u64::MAX);
            let cutoff = now.saturating_sub(to_i64(max_age_ms));
            removed += self.delete_where("pinned = 0 AND last_used_at < ?1", &[&cutoff])?;
        }
        if let Some(max_items) = limits.max_items {
            removed += self.delete_where(
                "pinned = 0 AND id NOT IN (
                     SELECT id FROM clip WHERE pinned = 0 ORDER BY last_used_at DESC, id DESC LIMIT ?1)",
                &[&to_i64(max_items)],
            )?;
        }
        if let Some(max_bytes) = limits.max_total_bytes {
            removed += self.delete_where(
                "id IN (SELECT id FROM (
                     SELECT id, SUM(byte_size) OVER (ORDER BY last_used_at DESC, id DESC) AS running
                     FROM clip WHERE pinned = 0)
                 WHERE running > ?1)",
                &[&to_i64(max_bytes)],
            )?;
        }
        // Both always run: one unremovable file must not stop the log being wiped, and a
        // flush an earlier delete could not finish is retried here.
        let swept = self.remove_orphan_blobs();
        let flushed = self.flush_wal();
        swept.and(flushed)?;
        Ok(removed)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// While on, the capture pipeline stores nothing.
    pub fn private_mode(&self) -> Result<bool> {
        Ok(self.setting(PRIVATE_MODE_KEY)?.as_deref() == Some("on"))
    }

    pub fn set_private_mode(&mut self, on: bool) -> Result<()> {
        self.set_setting(PRIVATE_MODE_KEY, if on { "on" } else { "off" })
    }

    /// Apps the user excluded, as normalised names (see [`crate::app_key`]). The built-in
    /// password-manager list is not included; it always applies.
    pub fn excluded_apps(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM excluded_app ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_excluded_app(&mut self, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO excluded_app (name) VALUES (?1)",
            [app_key(name)],
        )?;
        Ok(())
    }

    /// Returns whether the app was on the list.
    pub fn remove_excluded_app(&mut self, name: &str) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM excluded_app WHERE name = ?1", [app_key(name)])?;
        Ok(removed > 0)
    }

    pub(crate) fn is_excluded_app(&self, name: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM excluded_app WHERE name = ?1)",
            [app_key(name)],
            |r| r.get(0),
        )?)
    }

    pub fn set_thumbnail(&mut self, id: ClipId, png: &[u8]) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE clip SET thumbnail = ?1 WHERE id = ?2",
            params![png, id],
        )?;
        if changed == 0 {
            return Err(Error::NotFound(id));
        }
        Ok(())
    }

    /// The clip's PNG thumbnail, if it is an image that could be decoded.
    pub fn thumbnail(&self, id: ClipId) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row("SELECT thumbnail FROM clip WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or(Error::NotFound(id))
    }

    /// Deletes clips matching `condition` and the blob files only they used, then flushes
    /// the write-ahead log, so a deleted secret leaves nothing behind on disk.
    fn delete_where(&mut self, condition: &str, args: &[&dyn ToSql]) -> Result<usize> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let blob_hashes: Vec<String> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT DISTINCT blob_hash FROM clip_format
                 WHERE blob_hash IS NOT NULL AND clip_id IN (SELECT id FROM clip WHERE {condition})"
            ))?;
            stmt.query_map(args, |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let removed = tx.execute(&format!("DELETE FROM clip WHERE {condition}"), args)?;
        tx.commit()?;
        if removed == 0 {
            return Ok(0);
        }
        // Only now, with the deletion committed, unlink blobs nothing references any more.
        // If that is interrupted, the leftovers are orphans swept at the next open or prune.
        // Both steps always run, so a failed unlink never stops the log being wiped.
        let cleaned = self.remove_unreferenced_blobs(&blob_hashes);
        let flushed = self.flush_wal();
        cleaned.and(flushed)?;
        Ok(removed)
    }

    /// Unlinks each blob in `hashes` that no clip references. Holds the write lock, so no
    /// other process is mid-way through re-adding one. Keeps going past a file that cannot
    /// be removed and returns the first error.
    fn remove_unreferenced_blobs(&mut self, hashes: &[String]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut first_error = None;
        for hash in hashes {
            let result = blob_path(&self.blob_dir, hash)
                .and_then(|path| remove_unless_referenced(&tx, hash, &path));
            if let Err(e) = result {
                first_error.get_or_insert(e);
            }
        }
        tx.commit()?;
        first_error.map_or(Ok(()), Err)
    }

    /// Empties the write-ahead log so deleted content is gone from it too. `TRUNCATE` waits
    /// (up to the busy timeout) for other connections; one still reading an older snapshot
    /// makes it report busy, which becomes [`Error::DeletedButNotFlushed`].
    fn flush_wal(&self) -> Result<()> {
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy != 0 {
            return Err(Error::DeletedButNotFlushed);
        }
        Ok(())
    }

    /// Removes blob files no clip references, left behind if ClipVault stopped between
    /// writing a blob and saving its clip. Holds the write lock, under which every blob is
    /// written, so a blob that is about to be referenced is never mistaken for an orphan.
    fn remove_orphan_blobs(&mut self) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut first_error = None;
        for shard in fs::read_dir(&self.blob_dir)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for file in fs::read_dir(shard.path())? {
                let file = file?;
                // Leftover temporary files carry a suffix, so they never match a reference.
                let name = file.file_name().to_string_lossy().into_owned();
                if let Err(e) = remove_unless_referenced(&tx, &name, &file.path()) {
                    first_error.get_or_insert(e);
                }
            }
        }
        tx.commit()?;
        first_error.map_or(Ok(()), Err)
    }
}

fn remove_unless_referenced(conn: &Connection, hash: &str, path: &Path) -> Result<()> {
    if blob_is_referenced(conn, hash)? {
        return Ok(());
    }
    remove_if_exists(path)
}

fn blob_is_referenced(conn: &Connection, hash: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM clip_format WHERE blob_hash = ?1)",
        [hash],
        |r| r.get(0),
    )?)
}

fn summary_from_row(row: &Row<'_>) -> rusqlite::Result<ClipSummary> {
    let kind: String = row.get(1)?;
    Ok(ClipSummary {
        id: row.get(0)?,
        kind: Kind::parse(&kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                Type::Text,
                format!("unknown clip kind {kind:?}").into(),
            )
        })?,
        preview: row.get(2)?,
        source_app: row.get(3)?,
        byte_size: row.get::<_, i64>(4)?.max(0) as u64,
        pinned: row.get(5)?,
        use_count: row.get::<_, i64>(6)?.max(0) as u64,
        created_at: row.get(7)?,
        last_used_at: row.get(8)?,
    })
}

/// BLAKE3 over every format, in MIME order so the order apps offer them does not matter.
fn content_hash(formats: &[Format]) -> String {
    let mut sorted: Vec<&Format> = formats.iter().collect();
    sorted.sort_by(|a, b| a.mime.cmp(&b.mime));
    let mut hasher = blake3::Hasher::new();
    for format in sorted {
        hasher.update(format.mime.as_bytes());
        hasher.update(&[0]);
        hasher.update(&(format.data.len() as u64).to_le_bytes());
        hasher.update(&format.data);
    }
    hasher.finalize().to_hex().to_string()
}

fn find<'a>(formats: &'a [Format], mime: &str) -> Option<&'a [u8]> {
    formats
        .iter()
        .find(|f| f.mime == mime)
        .map(|f| f.data.as_slice())
}

fn kind_of(formats: &[Format]) -> Kind {
    let has = |mime| find(formats, mime).is_some();
    if has(PNG_MIME) {
        Kind::Image
    } else if has(URI_LIST_MIME) {
        Kind::Files
    } else if has(HTML_MIME) {
        Kind::Html
    } else if has(RTF_MIME) {
        Kind::Rtf
    } else {
        Kind::Text
    }
}

fn text_of(formats: &[Format]) -> Option<String> {
    find(formats, TEXT_MIME).map(|data| String::from_utf8_lossy(data).into_owned())
}

fn preview_of(formats: &[Format]) -> String {
    let preview = match kind_of(formats) {
        Kind::Image => png_size(find(formats, PNG_MIME).unwrap_or_default())
            .map_or_else(|| "Image".to_owned(), |(w, h)| format!("Image {w} × {h}")),
        Kind::Files => String::from_utf8_lossy(find(formats, URI_LIST_MIME).unwrap_or_default())
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|uri| uri.rsplit('/').next().unwrap_or(uri))
            .collect::<Vec<_>>()
            .join(", "),
        Kind::Text | Kind::Html | Kind::Rtf => text_of(formats)
            .or_else(|| find(formats, HTML_MIME).map(|d| String::from_utf8_lossy(d).into_owned()))
            .unwrap_or_default(),
    };
    preview.trim().chars().take(PREVIEW_CHARS).collect()
}

/// Width and height from a PNG's IHDR chunk, without decoding the image.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || !png.starts_with(SIGNATURE) || &png[12..16] != b"IHDR" {
        return None;
    }
    let read = |at: usize| u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]);
    Some((read(16), read(20)))
}

fn blob_path(blob_dir: &Path, hash: &str) -> Result<PathBuf> {
    // Hashes come from our own database; refuse anything that is not one, so a damaged
    // or tampered database cannot point reads or deletes outside the blob store.
    let is_hash = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_hash {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid blob reference {hash:?}"),
        )
        .into());
    }
    Ok(blob_dir.join(&hash[..2]).join(hash))
}

/// Writes `data` under its BLAKE3 hash and returns the hash. Called only inside a write
/// transaction (see `Store::add`). Writes a temp file and renames it, so a crash never
/// leaves a half-written blob under a real hash.
fn write_blob(blob_dir: &Path, data: &[u8]) -> Result<String> {
    let hash = blake3::hash(data).to_hex().to_string();
    let path = blob_path(blob_dir, &hash)?;
    if let Some(shard) = path.parent() {
        create_private_dir(shard)?;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = path.with_extension(format!("tmp-{}-{nanos}", std::process::id()));
    fs::write(&temp, data)?;
    fs::rename(&temp, &path)?;
    Ok(hash)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
