use std::fs;
use std::path::Path;
use std::time::Duration;

use clipvault_core::{
    Added, ClipId, Error, Format, Kind, Limits, NewClip, PNG_MIME, Store, TEXT_MIME,
};
use tempfile::TempDir;

const BIG: usize = 100 * 1024;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn open() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

fn clip(mime: &str, data: &[u8]) -> NewClip {
    NewClip {
        formats: vec![Format {
            mime: mime.to_owned(),
            data: data.to_vec(),
        }],
        source_app: None,
    }
}

fn text(s: &str) -> NewClip {
    clip(TEXT_MIME, s.as_bytes())
}

fn id(added: Added) -> ClipId {
    match added {
        Added::Inserted(id) | Added::Bumped(id) => id,
    }
}

fn add(store: &mut Store, new: &NewClip, now: i64) -> ClipId {
    id(store.add(new, now).unwrap())
}

fn recent_ids(store: &Store) -> Vec<ClipId> {
    store.recent(100).unwrap().iter().map(|c| c.id).collect()
}

fn search_ids(store: &Store, query: &str) -> Vec<ClipId> {
    store
        .search(query, 100)
        .unwrap()
        .iter()
        .map(|c| c.id)
        .collect()
}

/// Whether `needle` appears anywhere in the database, its log, or its shared-memory file.
fn database_files_contain(dir: &Path, needle: &[u8]) -> bool {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .any(|path| {
            fs::read(path)
                .unwrap()
                .windows(needle.len())
                .any(|window| window == needle)
        })
}

/// Replaces one blob file with a folder, so removing it fails.
fn make_a_blob_unremovable(dir: &Path) {
    let shard = fs::read_dir(dir.join("blobs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let blob = fs::read_dir(&shard)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&blob).unwrap();
    fs::create_dir(&blob).unwrap();
}

fn blob_file_count(dir: &Path) -> usize {
    fs::read_dir(dir.join("blobs"))
        .unwrap()
        .map(|shard| fs::read_dir(shard.unwrap().path()).unwrap().count())
        .sum()
}

/// A PNG signature and IHDR chunk header: enough for the preview to read the size.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend_from_slice(&width.to_be_bytes());
    png.extend_from_slice(&height.to_be_bytes());
    png
}

#[test]
fn new_content_is_inserted() {
    let (_dir, mut store) = open();

    let added = store.add(&text("hello"), 1).unwrap();

    assert!(matches!(added, Added::Inserted(_)));
}

#[test]
fn re_copying_the_same_content_bumps_the_existing_clip() {
    let (_dir, mut store) = open();
    let first = add(&mut store, &text("hello"), 1);

    let again = store.add(&text("hello"), 2).unwrap();

    assert_eq!(again, Added::Bumped(first));
}

#[test]
fn a_bumped_clip_moves_to_the_top() {
    let (_dir, mut store) = open();
    let a = add(&mut store, &text("a"), 1);
    let b = add(&mut store, &text("b"), 2);

    add(&mut store, &text("a"), 3);

    assert_eq!(recent_ids(&store), vec![a, b]);
}

#[test]
fn a_clip_without_formats_is_rejected() {
    let (_dir, mut store) = open();
    let empty = NewClip {
        formats: vec![],
        source_app: None,
    };

    let result = store.add(&empty, 1);

    assert!(matches!(result, Err(Error::EmptyClip)));
}

#[test]
fn small_formats_round_trip() {
    let (_dir, mut store) = open();
    let new = text("hello ✓ 🙂");
    let id = add(&mut store, &new, 1);

    let formats = store.formats(id).unwrap();

    assert_eq!(formats, new.formats);
}

#[test]
fn large_formats_round_trip_through_the_blob_store() {
    let (_dir, mut store) = open();
    let new = clip(PNG_MIME, &vec![7; BIG]);
    let id = add(&mut store, &new, 1);

    let formats = store.formats(id).unwrap();

    assert_eq!(formats, new.formats);
}

#[test]
fn small_payloads_create_no_blob_files() {
    let (dir, mut store) = open();

    add(&mut store, &text("hello"), 1);

    assert_eq!(blob_file_count(dir.path()), 0);
}

#[test]
fn formats_of_an_unknown_clip_is_not_found() {
    let (_dir, store) = open();

    let result = store.formats(42);

    assert!(matches!(result, Err(Error::NotFound(42))));
}

#[test]
fn search_matches_every_word_as_a_prefix() {
    let (_dir, mut store) = open();
    let invoice = add(&mut store, &text("Invoice 2025 for Akolet"), 1);
    add(&mut store, &text("invoice draft"), 2);

    let found = search_ids(&store, "inv 20");

    assert_eq!(found, vec![invoice]);
}

#[test]
fn search_treats_query_syntax_as_literal_text() {
    let (_dir, mut store) = open();
    let quoted = add(&mut store, &text(r#"say "hi" OR leave"#), 1);

    let found = search_ids(&store, r#""hi" OR"#);

    assert_eq!(found, vec![quoted]);
}

#[test]
fn an_empty_search_lists_recent_clips() {
    let (_dir, mut store) = open();
    let a = add(&mut store, &text("a"), 1);

    let found = search_ids(&store, "   ");

    assert_eq!(found, vec![a]);
}

#[test]
fn regex_search_matches_case_insensitively() {
    let (_dir, mut store) = open();
    let match_id = add(&mut store, &text("ref INV-42 paid"), 1);
    add(&mut store, &text("ref inv-x"), 2);

    let found = search_ids(&store, r"/inv-\d+/");

    assert_eq!(found, vec![match_id]);
}

#[test]
fn an_invalid_regex_is_an_error() {
    let (_dir, store) = open();

    let result = store.search("/(/", 10);

    assert!(matches!(result, Err(Error::Regex(_))));
}

#[test]
fn a_deleted_clip_is_no_longer_found() {
    let (_dir, mut store) = open();
    let id = add(&mut store, &text("secret token"), 1);

    store.delete(id).unwrap();

    assert!(search_ids(&store, "secret").is_empty());
}

#[test]
fn deleting_a_clip_removes_its_blob_file() {
    let (dir, mut store) = open();
    let id = add(&mut store, &clip(PNG_MIME, &vec![1; BIG]), 1);

    store.delete(id).unwrap();

    assert_eq!(blob_file_count(dir.path()), 0);
}

#[test]
fn a_blob_shared_by_two_clips_survives_deleting_one() {
    let (_dir, mut store) = open();
    let image = Format {
        mime: PNG_MIME.to_owned(),
        data: vec![2; BIG],
    };
    let plain = clip(PNG_MIME, &image.data);
    let labelled = NewClip {
        formats: vec![image.clone(), text("label").formats[0].clone()],
        source_app: None,
    };
    let first = add(&mut store, &plain, 1);
    let second = add(&mut store, &labelled, 2);

    store.delete(first).unwrap();

    assert_eq!(store.formats(second).unwrap()[0], image);
}

#[test]
fn deleting_an_unknown_clip_is_not_found() {
    let (_dir, mut store) = open();

    let result = store.delete(42);

    assert!(matches!(result, Err(Error::NotFound(42))));
}

#[test]
fn pinning_an_unknown_clip_is_not_found() {
    let (_dir, mut store) = open();

    let result = store.set_pinned(42, true);

    assert!(matches!(result, Err(Error::NotFound(42))));
}

#[test]
fn prune_by_count_keeps_the_newest_unpinned_and_every_pinned_clip() {
    let (_dir, mut store) = open();
    let pinned_old = add(&mut store, &text("pinned"), 1);
    store.set_pinned(pinned_old, true).unwrap();
    add(&mut store, &text("old"), 2);
    let newer = add(&mut store, &text("newer"), 3);
    let newest = add(&mut store, &text("newest"), 4);
    let limits = Limits {
        max_items: Some(2),
        max_age: None,
        max_total_bytes: None,
    };

    store.prune(&limits, 5).unwrap();

    assert_eq!(recent_ids(&store), vec![newest, newer, pinned_old]);
}

#[test]
fn prune_by_age_removes_old_unpinned_clips() {
    let (_dir, mut store) = open();
    add(&mut store, &text("old"), 0);
    let fresh = add(&mut store, &text("fresh"), 29 * DAY_MS);
    let limits = Limits {
        max_items: None,
        max_age: Some(Duration::from_secs(7 * 24 * 60 * 60)),
        max_total_bytes: None,
    };

    store.prune(&limits, 30 * DAY_MS).unwrap();

    assert_eq!(recent_ids(&store), vec![fresh]);
}

#[test]
fn prune_by_size_removes_the_oldest_clips_first() {
    let (_dir, mut store) = open();
    add(&mut store, &clip(PNG_MIME, &vec![1; 1000]), 1);
    let middle = add(&mut store, &clip(PNG_MIME, &vec![2; 1000]), 2);
    let newest = add(&mut store, &clip(PNG_MIME, &vec![3; 1000]), 3);
    let limits = Limits {
        max_items: None,
        max_age: None,
        max_total_bytes: Some(2500),
    };

    store.prune(&limits, 4).unwrap();

    assert_eq!(recent_ids(&store), vec![newest, middle]);
}

#[test]
fn prune_removes_orphaned_blob_files() {
    let (dir, mut store) = open();
    let orphan_dir = dir.path().join("blobs").join("ab");
    fs::create_dir_all(&orphan_dir).unwrap();
    fs::write(orphan_dir.join("ab".repeat(32)), b"left by a crash").unwrap();

    store.prune(&Limits::default(), 1).unwrap();

    assert_eq!(blob_file_count(dir.path()), 0);
}

#[test]
fn prune_keeps_blob_files_that_clips_still_use() {
    let (dir, mut store) = open();
    add(&mut store, &clip(PNG_MIME, &vec![4; BIG]), 1);

    store.prune(&Limits::default(), 2).unwrap();

    assert_eq!(blob_file_count(dir.path()), 1);
}

#[test]
fn deleted_text_leaves_no_trace_in_the_database_files() {
    let (dir, mut store) = open();
    let secret = b"hunter2supersecret";
    let id = add(&mut store, &clip(TEXT_MIME, secret), 1);

    store.delete(id).unwrap();

    assert!(!database_files_contain(dir.path(), secret));
}

#[test]
fn a_failed_blob_cleanup_still_wipes_deleted_text_from_the_log() {
    let (dir, mut store) = open();
    let secret = b"correcthorsebatterystaple";
    add(&mut store, &clip(TEXT_MIME, secret), 1);
    add(&mut store, &clip(PNG_MIME, &vec![8; BIG]), 2);
    make_a_blob_unremovable(dir.path());

    let _ = store.clear_unpinned();

    assert!(!database_files_contain(dir.path(), secret));
}

#[test]
fn reopening_the_history_removes_blob_files_a_crash_left_behind() {
    let dir = TempDir::new().unwrap();
    drop(Store::open(dir.path()).unwrap());
    let orphan_dir = dir.path().join("blobs").join("ef");
    fs::create_dir_all(&orphan_dir).unwrap();
    fs::write(orphan_dir.join("ef".repeat(32)), b"image of a deleted clip").unwrap();

    let _reopened = Store::open(dir.path()).unwrap();

    assert_eq!(blob_file_count(dir.path()), 0);
}

#[test]
fn delete_reports_when_another_reader_keeps_old_content_in_the_log() {
    let (dir, mut store) = open();
    let id = add(&mut store, &text("hunter2"), 1);
    let reader = rusqlite::Connection::open(dir.path().join("clipvault.db")).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM clip", [], |r| r.get(0))
        .unwrap();

    let result = store.delete(id);

    assert!(matches!(result, Err(Error::DeletedButNotFlushed)));
}

#[test]
fn a_failed_blob_cleanup_does_not_bring_deleted_clips_back() {
    let (dir, mut store) = open();
    add(&mut store, &clip(PNG_MIME, &vec![5; BIG]), 1);
    add(&mut store, &clip(PNG_MIME, &vec![6; BIG]), 2);
    make_a_blob_unremovable(dir.path());

    let _ = store.clear_unpinned();

    assert!(recent_ids(&store).is_empty());
}

#[test]
fn a_deleted_clips_id_is_never_reused() {
    let (_dir, mut store) = open();
    let deleted = add(&mut store, &text("password"), 1);
    store.delete(deleted).unwrap();

    let next = add(&mut store, &text("something else"), 2);

    assert_ne!(next, deleted);
}

#[test]
fn clear_unpinned_keeps_pinned_clips() {
    let (_dir, mut store) = open();
    let pinned = add(&mut store, &text("keep"), 1);
    store.set_pinned(pinned, true).unwrap();
    add(&mut store, &text("drop"), 2);

    store.clear_unpinned().unwrap();

    assert_eq!(recent_ids(&store), vec![pinned]);
}

#[test]
fn an_image_preview_shows_its_dimensions() {
    let (_dir, mut store) = open();

    add(&mut store, &clip(PNG_MIME, &png_header(1920, 1080)), 1);

    assert_eq!(store.recent(1).unwrap()[0].preview, "Image 1920 × 1080");
}

#[test]
fn a_copied_file_list_is_a_files_clip() {
    let (_dir, mut store) = open();

    add(
        &mut store,
        &clip("text/uri-list", b"file:///home/u/report.pdf\r\n"),
        1,
    );

    assert_eq!(store.recent(1).unwrap()[0].kind, Kind::Files);
}

#[test]
fn a_file_list_preview_shows_file_names() {
    let (_dir, mut store) = open();

    add(
        &mut store,
        &clip(
            "text/uri-list",
            b"file:///a/report.pdf\r\nfile:///b/photo.png\r\n",
        ),
        1,
    );

    assert_eq!(store.recent(1).unwrap()[0].preview, "report.pdf, photo.png");
}

#[test]
fn settings_round_trip() {
    let (_dir, mut store) = open();

    store.set_setting("private_mode", "on").unwrap();

    assert_eq!(
        store.setting("private_mode").unwrap().as_deref(),
        Some("on")
    );
}

#[test]
fn history_survives_reopening() {
    let dir = TempDir::new().unwrap();
    let id = add(&mut Store::open(dir.path()).unwrap(), &text("kept"), 1);

    let reopened = Store::open(dir.path()).unwrap();

    assert_eq!(recent_ids(&reopened), vec![id]);
}

#[cfg(unix)]
#[test]
fn the_history_folder_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, _store) = open();

    let mode = fs::metadata(dir.path()).unwrap().permissions().mode();

    assert_eq!(mode & 0o777, 0o700);
}
