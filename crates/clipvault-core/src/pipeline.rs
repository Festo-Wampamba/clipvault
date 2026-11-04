//! The capture pipeline (README §7). Every clip from every backend goes through
//! [`capture`]: filter, normalise, store, thumbnail.

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, ImageReader};

use crate::{Added, ClipId, Format, NewClip, PNG_MIME, Result, Store, TEXT_MIME};

/// Copies larger than this are skipped (README §7).
pub const MAX_CLIP_BYTES: u64 = 20 * 1024 * 1024;
const THUMBNAIL_SIDE: u32 = 256;
/// Clipboard images come from any app or web page, so decoding is capped: a tiny file
/// claiming to be a gigantic image must not exhaust memory or stall an app meant to idle.
/// 128 MiB still fits a full-resolution 6K screenshot (about 81 MiB of RGBA).
const MAX_IMAGE_SIDE: u32 = 16_384;
const MAX_IMAGE_ALLOC: u64 = 128 * 1024 * 1024;

const HTML_MIME: &str = "text/html";
const RTF_MIME: &str = "text/rtf";
const URI_LIST_MIME: &str = "text/uri-list";
/// Text formats, best first; the first one offered becomes `TEXT_MIME`.
const TEXT_SOURCES: &[&str] = &[TEXT_MIME, "UTF8_STRING", "text/plain", "STRING", "TEXT"];
const RTF_SOURCES: &[&str] = &[RTF_MIME, "application/rtf"];
const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";
/// Other image formats, converted to PNG when no PNG is offered.
const IMAGE_SOURCES: &[&str] = &[
    "image/jpeg",
    "image/bmp",
    "image/gif",
    "image/webp",
    "image/tiff",
];

/// Password managers whose copies are never stored. Most also mark copies as sensitive,
/// which backends already honour; this catches the ones that do not.
const BUILT_IN_EXCLUDED_APPS: &[&str] = &[
    "keepassxc",
    "org.keepassxc.keepassxc",
    "keepass",
    "bitwarden",
    "com.bitwarden.desktop",
    "1password",
    "com.1password.1password",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Stored(Added),
    Skipped(Skip),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    PrivateMode,
    ExcludedApp,
    TooLarge {
        bytes: u64,
    },
    /// Nothing ClipVault keeps: blank text, or only formats it does not store.
    Empty,
}

/// Runs one copy through the pipeline. `now` is Unix milliseconds.
pub fn capture(store: &mut Store, clip: NewClip, now: i64) -> Result<Outcome> {
    if store.private_mode()? {
        return Ok(Outcome::Skipped(Skip::PrivateMode));
    }
    if let Some(app) = &clip.source_app {
        let key = app_key(app);
        if BUILT_IN_EXCLUDED_APPS.contains(&key.as_str()) || store.is_excluded_app(&key)? {
            return Ok(Outcome::Skipped(Skip::ExcludedApp));
        }
    }
    // Before normalising, so an oversized copy is never decoded. Per format, because apps
    // offer the same text under several names that normalising merges into one.
    let largest = clip.formats.iter().map(|f| f.data.len() as u64).max();
    if let Some(bytes) = largest.filter(|bytes| *bytes > MAX_CLIP_BYTES) {
        return Ok(Outcome::Skipped(Skip::TooLarge { bytes }));
    }
    let Normalised { formats, decoded } = normalise(clip.formats);
    if formats.is_empty() {
        return Ok(Outcome::Skipped(Skip::Empty));
    }
    let bytes = total_bytes(&formats);
    if bytes > MAX_CLIP_BYTES {
        return Ok(Outcome::Skipped(Skip::TooLarge { bytes }));
    }

    let clip = NewClip {
        formats,
        source_app: clip.source_app,
    };
    let added = store.add(&clip, now)?;
    let (Added::Inserted(id) | Added::Bumped(id)) = added;
    // The clip is stored whatever happens here. A thumbnail that cannot be made or saved
    // now is retried the next time the same image is copied.
    let _ = ensure_thumbnail(store, id, &clip.formats, decoded);
    Ok(Outcome::Stored(added))
}

/// Gives an image clip a thumbnail if it has none yet: new clips, and clips stored before
/// thumbnails existed or whose earlier attempt failed.
fn ensure_thumbnail(
    store: &mut Store,
    id: ClipId,
    formats: &[Format],
    decoded: Option<DynamicImage>,
) -> Result<()> {
    if store.thumbnail(id)?.is_some() {
        return Ok(());
    }
    let image = decoded.or_else(|| {
        formats
            .iter()
            .find(|f| f.mime == PNG_MIME)
            .and_then(|f| decode(&f.data))
    });
    if let Some(png) = image.and_then(|img| to_png(&img.thumbnail(THUMBNAIL_SIDE, THUMBNAIL_SIDE)))
    {
        store.set_thumbnail(id, &png)?;
    }
    Ok(())
}

struct Normalised {
    formats: Vec<Format>,
    /// An image converted to PNG, kept decoded so its thumbnail needs no second decode.
    decoded: Option<DynamicImage>,
}

/// The form app names are compared in: lower case, no folder, no ".exe", so
/// "C:\Program Files\KeePassXC\KeePassXC.exe" and "keepassxc" match.
pub fn app_key(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let lower = base.trim().to_lowercase();
    lower
        .strip_suffix(".exe")
        .map(str::to_owned)
        .unwrap_or(lower)
}

/// Reduces whatever the source app offered to the formats ClipVault stores, under their
/// standard names: text, HTML, RTF, a file list, and PNG. App-private formats, and formats
/// with no content (blank text, an empty file list, a zero-byte image), are dropped.
fn normalise(offered: Vec<Format>) -> Normalised {
    let find = |names: &[&str]| {
        names.iter().find_map(|name| {
            offered
                .iter()
                .find(|f| f.mime.eq_ignore_ascii_case(name))
                .map(|f| f.data.as_slice())
        })
    };
    let mut kept = Vec::new();
    let mut keep = |mime: &str, data: Vec<u8>| {
        kept.push(Format {
            mime: mime.to_owned(),
            data,
        })
    };

    if let Some(text) = find(TEXT_SOURCES) {
        keep(TEXT_MIME, to_utf8(text).into_bytes());
    }
    if let Some(html) = find(&[HTML_MIME, "HTML Format"]) {
        keep(HTML_MIME, strip_cf_html(html).to_vec());
    }
    if let Some(rtf) = find(RTF_SOURCES) {
        keep(RTF_MIME, rtf.to_vec());
    }
    if let Some(uris) = find(&[URI_LIST_MIME]) {
        keep(URI_LIST_MIME, uris.to_vec());
    } else if let Some(gnome) = find(&[GNOME_COPIED_FILES]) {
        keep(URI_LIST_MIME, gnome_files_to_uri_list(gnome));
    }
    let mut decoded = None;
    if let Some(png) = find(&[PNG_MIME]) {
        keep(PNG_MIME, png.to_vec());
    } else if let Some(image) = find(IMAGE_SOURCES).and_then(decode) {
        if let Some(png) = to_png(&image) {
            keep(PNG_MIME, png);
            decoded = Some(image);
        }
    }
    kept.retain(|f| !f.data.iter().all(u8::is_ascii_whitespace));
    Normalised {
        formats: kept,
        decoded,
    }
}

fn total_bytes(formats: &[Format]) -> u64 {
    formats.iter().map(|f| f.data.len() as u64).sum()
}

/// X11 `STRING` and some apps' `text/plain` are Latin-1, not UTF-8.
fn to_utf8(data: &[u8]) -> String {
    match std::str::from_utf8(data) {
        Ok(text) => text.to_owned(),
        Err(_) => data.iter().map(|&b| char::from(b)).collect(),
    }
}

/// Windows "HTML Format" wraps the HTML in a header of byte offsets; keep only the HTML.
fn strip_cf_html(data: &[u8]) -> &[u8] {
    if !data.starts_with(b"Version:") {
        return data;
    }
    let header = String::from_utf8_lossy(&data[..data.len().min(1024)]);
    let offset = |key: &str| {
        header
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.trim().parse::<usize>().ok())
    };
    match (offset("StartHTML:"), offset("EndHTML:")) {
        (Some(start), Some(end)) if start < end && end <= data.len() => &data[start..end],
        _ => data,
    }
}

/// GNOME's "copy\nfile:///a\nfile:///b" (or "cut\n…") as a standard text/uri-list.
fn gnome_files_to_uri_list(data: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(data)
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .map(|uri| format!("{uri}\r\n"))
        .collect::<String>()
        .into_bytes()
}

fn decode(data: &[u8]) -> Option<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);
    reader.decode().ok()
}

fn to_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(png)
}
