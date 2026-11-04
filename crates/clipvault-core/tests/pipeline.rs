use std::io::Cursor;

use clipvault_core::{
    Added, ClipId, Format, MAX_CLIP_BYTES, NewClip, Outcome, PNG_MIME, Skip, Store, TEXT_MIME,
    app_key, capture,
};
use image::{DynamicImage, ImageFormat, RgbImage};
use tempfile::TempDir;

fn open() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

fn offered(formats: &[(&str, &[u8])]) -> NewClip {
    NewClip {
        formats: formats
            .iter()
            .map(|(mime, data)| Format {
                mime: (*mime).to_owned(),
                data: data.to_vec(),
            })
            .collect(),
        source_app: None,
    }
}

fn from_app(app: &str, text: &str) -> NewClip {
    NewClip {
        source_app: Some(app.to_owned()),
        ..offered(&[(TEXT_MIME, text.as_bytes())])
    }
}

fn encode(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(RgbImage::new(width, height))
        .write_to(&mut Cursor::new(&mut bytes), format)
        .unwrap();
    bytes
}

fn stored_id(outcome: Outcome) -> ClipId {
    match outcome {
        Outcome::Stored(Added::Inserted(id) | Added::Bumped(id)) => id,
        Outcome::Skipped(skip) => panic!("expected the clip to be stored, got {skip:?}"),
    }
}

fn stored_mimes(store: &Store, id: ClipId) -> Vec<String> {
    store
        .formats(id)
        .unwrap()
        .into_iter()
        .map(|f| f.mime)
        .collect()
}

fn capture_and_get(store: &mut Store, clip: NewClip, mime: &str) -> Vec<u8> {
    let id = stored_id(capture(store, clip, 1).unwrap());
    store
        .formats(id)
        .unwrap()
        .into_iter()
        .find(|f| f.mime == mime)
        .unwrap()
        .data
}

#[test]
fn a_normal_copy_is_stored() {
    let (_dir, mut store) = open();

    let outcome = capture(&mut store, offered(&[(TEXT_MIME, b"hello")]), 1).unwrap();

    assert!(matches!(outcome, Outcome::Stored(Added::Inserted(_))));
}

#[test]
fn private_mode_stores_nothing() {
    let (_dir, mut store) = open();
    store.set_private_mode(true).unwrap();

    let outcome = capture(&mut store, offered(&[(TEXT_MIME, b"hello")]), 1).unwrap();

    assert_eq!(outcome, Outcome::Skipped(Skip::PrivateMode));
}

#[test]
fn turning_private_mode_off_resumes_capture() {
    let (_dir, mut store) = open();
    store.set_private_mode(true).unwrap();
    store.set_private_mode(false).unwrap();

    let outcome = capture(&mut store, offered(&[(TEXT_MIME, b"hello")]), 1).unwrap();

    assert!(matches!(outcome, Outcome::Stored(_)));
}

#[test]
fn copies_from_a_built_in_password_manager_are_skipped() {
    let (_dir, mut store) = open();

    let outcome = capture(
        &mut store,
        from_app(r"C:\Program Files\KeePassXC\KeePassXC.exe", "s3cret"),
        1,
    )
    .unwrap();

    assert_eq!(outcome, Outcome::Skipped(Skip::ExcludedApp));
}

#[test]
fn copies_from_an_app_the_user_excluded_are_skipped() {
    let (_dir, mut store) = open();
    store.add_excluded_app("/usr/bin/Signal").unwrap();

    let outcome = capture(&mut store, from_app("signal", "chat"), 1).unwrap();

    assert_eq!(outcome, Outcome::Skipped(Skip::ExcludedApp));
}

#[test]
fn removing_an_excluded_app_reports_it_was_listed() {
    let (_dir, mut store) = open();
    store.add_excluded_app("signal").unwrap();

    let removed = store.remove_excluded_app("Signal.exe").unwrap();

    assert!(removed);
}

#[test]
fn app_names_compare_without_folder_case_or_exe() {
    assert_eq!(app_key(r"C:\Apps\Telegram.EXE"), "telegram");
}

#[test]
fn a_copy_over_the_size_limit_is_skipped() {
    let (_dir, mut store) = open();
    let huge = vec![b'a'; MAX_CLIP_BYTES as usize + 1];

    let outcome = capture(&mut store, offered(&[(TEXT_MIME, &huge)]), 1).unwrap();

    assert!(matches!(outcome, Outcome::Skipped(Skip::TooLarge { .. })));
}

#[test]
fn blank_text_is_skipped() {
    let (_dir, mut store) = open();

    let outcome = capture(&mut store, offered(&[(TEXT_MIME, b"  \n\t ")]), 1).unwrap();

    assert_eq!(outcome, Outcome::Skipped(Skip::Empty));
}

#[test]
fn a_copy_with_only_app_private_formats_is_skipped() {
    let (_dir, mut store) = open();

    let outcome = capture(
        &mut store,
        offered(&[("application/x-qt-image", b"\x01\x02")]),
        1,
    )
    .unwrap();

    assert_eq!(outcome, Outcome::Skipped(Skip::Empty));
}

#[test]
fn text_offered_under_several_names_is_stored_once() {
    let (_dir, mut store) = open();
    let clip = offered(&[
        ("UTF8_STRING", b"hi"),
        ("text/plain", b"hi"),
        ("TEXT", b"hi"),
    ]);

    let id = stored_id(capture(&mut store, clip, 1).unwrap());

    assert_eq!(stored_mimes(&store, id), vec![TEXT_MIME]);
}

#[test]
fn latin1_text_is_converted_to_utf8() {
    let (_dir, mut store) = open();

    let text = capture_and_get(&mut store, offered(&[("STRING", b"caf\xe9")]), TEXT_MIME);

    assert_eq!(text, "café".as_bytes());
}

#[test]
fn app_private_formats_are_dropped_alongside_text() {
    let (_dir, mut store) = open();
    let clip = offered(&[(TEXT_MIME, b"hi"), ("application/x-qt-image", b"\x01")]);

    let id = stored_id(capture(&mut store, clip, 1).unwrap());

    assert_eq!(stored_mimes(&store, id), vec![TEXT_MIME]);
}

#[test]
fn the_windows_html_header_is_stripped() {
    let (_dir, mut store) = open();
    let html = "<html><body>Hi</body></html>";
    let header = "Version:0.9\r\nStartHTML:0000000071\r\nEndHTML:0000000099\r\n";
    let filler = " ".repeat(71 - header.len());
    let cf_html = format!("{header}{filler}{html}");

    let stored = capture_and_get(
        &mut store,
        offered(&[("HTML Format", cf_html.as_bytes())]),
        "text/html",
    );

    assert_eq!(stored, html.as_bytes());
}

#[test]
fn gnome_copied_files_become_a_standard_file_list() {
    let (_dir, mut store) = open();
    let gnome = b"copy\nfile:///home/u/a.pdf\nfile:///home/u/b.png";

    let stored = capture_and_get(
        &mut store,
        offered(&[("x-special/gnome-copied-files", gnome)]),
        "text/uri-list",
    );

    assert_eq!(stored, b"file:///home/u/a.pdf\r\nfile:///home/u/b.png\r\n");
}

#[test]
fn a_jpeg_is_stored_as_png() {
    let (_dir, mut store) = open();
    let jpeg = encode(4, 4, ImageFormat::Jpeg);

    let png = capture_and_get(&mut store, offered(&[("image/jpeg", &jpeg)]), PNG_MIME);

    assert_eq!(image::guess_format(&png).unwrap(), ImageFormat::Png);
}

#[test]
fn a_corrupt_image_alone_is_skipped() {
    let (_dir, mut store) = open();

    let outcome = capture(&mut store, offered(&[("image/jpeg", b"not a jpeg")]), 1).unwrap();

    assert_eq!(outcome, Outcome::Skipped(Skip::Empty));
}

#[test]
fn a_new_image_gets_a_thumbnail_that_keeps_its_shape() {
    let (_dir, mut store) = open();
    let png = encode(1024, 512, ImageFormat::Png);
    let id = stored_id(capture(&mut store, offered(&[(PNG_MIME, &png)]), 1).unwrap());

    let thumbnail = image::load_from_memory(&store.thumbnail(id).unwrap().unwrap()).unwrap();

    assert_eq!((thumbnail.width(), thumbnail.height()), (256, 128));
}

#[test]
fn an_image_wider_than_the_decode_limit_is_kept_without_decoding() {
    let (_dir, mut store) = open();
    // A real, tiny 20 000 x 1 image: only the width cap stops it being decoded.
    let wide = encode(20_000, 1, ImageFormat::Png);
    let id = stored_id(capture(&mut store, offered(&[(PNG_MIME, &wide)]), 1).unwrap());

    let thumbnail = store.thumbnail(id).unwrap();

    assert_eq!(thumbnail, None);
}

#[test]
fn an_empty_file_list_alone_is_skipped() {
    let (_dir, mut store) = open();

    let outcome = capture(
        &mut store,
        offered(&[("x-special/gnome-copied-files", b"cut\n")]),
        1,
    )
    .unwrap();

    assert_eq!(outcome, Outcome::Skipped(Skip::Empty));
}

#[test]
fn text_offered_under_many_names_is_not_counted_several_times() {
    let (_dir, mut store) = open();
    let text = vec![b'a'; (MAX_CLIP_BYTES / 2) as usize];
    let clip = offered(&[
        (TEXT_MIME, &text),
        ("UTF8_STRING", &text),
        ("text/plain", &text),
    ]);

    let outcome = capture(&mut store, clip, 1).unwrap();

    assert!(matches!(outcome, Outcome::Stored(_)));
}

#[test]
fn re_copying_an_image_without_a_thumbnail_adds_one() {
    let (_dir, mut store) = open();
    let png = encode(64, 64, ImageFormat::Png);
    let clip = offered(&[(PNG_MIME, &png)]);
    // Stored without the pipeline, as clips saved before thumbnails existed were.
    let id = stored_id(Outcome::Stored(store.add(&clip, 1).unwrap()));

    capture(&mut store, clip, 2).unwrap();

    assert!(store.thumbnail(id).unwrap().is_some());
}

#[test]
fn text_has_no_thumbnail() {
    let (_dir, mut store) = open();
    let id = stored_id(capture(&mut store, offered(&[(TEXT_MIME, b"hi")]), 1).unwrap());

    let thumbnail = store.thumbnail(id).unwrap();

    assert_eq!(thumbnail, None);
}
