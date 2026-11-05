//! Runs the real `clipvault` binary against a temporary history.

use std::path::Path;
use std::process::{Command, Output};

use clipvault_core::{Added, ClipId, Format, NewClip, Store, TEXT_MIME};
use tempfile::TempDir;

fn clipvault(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clipvault"))
        .args(args)
        .env("CLIPVAULT_DATA_DIR", dir)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// A history holding `texts`, copied in order (the last is the newest).
fn history(texts: &[&str]) -> (TempDir, Vec<ClipId>) {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let ids = texts
        .iter()
        .zip(1..)
        .map(|(text, now)| {
            let clip = NewClip {
                formats: vec![Format {
                    mime: TEXT_MIME.to_owned(),
                    data: text.as_bytes().to_vec(),
                }],
                source_app: None,
            };
            match store.add(&clip, now).unwrap() {
                Added::Inserted(id) | Added::Bumped(id) => id,
            }
        })
        .collect();
    (dir, ids)
}

fn previews(dir: &Path) -> Vec<String> {
    Store::open(dir)
        .unwrap()
        .recent(100)
        .unwrap()
        .into_iter()
        .map(|clip| clip.preview)
        .collect()
}

#[test]
fn list_shows_the_newest_clip_first() {
    let (dir, _) = history(&["older", "newer"]);

    let output = clipvault(dir.path(), &["list"]);

    let text = stdout(&output);
    assert!(text.find("newer").unwrap() < text.find("older").unwrap());
}

#[test]
fn list_with_a_count_shows_that_many() {
    let (dir, _) = history(&["a", "b", "c"]);

    let output = clipvault(dir.path(), &["list", "2"]);

    assert_eq!(stdout(&output).lines().count(), 2);
}

#[test]
fn search_shows_only_matching_clips() {
    let (dir, _) = history(&["invoice 2025", "shopping list"]);

    let output = clipvault(dir.path(), &["search", "inv"]);

    let text = stdout(&output);
    assert!(text.contains("invoice") && !text.contains("shopping"));
}

#[test]
fn pin_keeps_a_clip_through_clear() {
    let (dir, ids) = history(&["keep me", "drop me"]);
    clipvault(dir.path(), &["pin", &ids[0].to_string()]);

    clipvault(dir.path(), &["clear"]);

    assert_eq!(previews(dir.path()), vec!["keep me"]);
}

#[test]
fn delete_removes_one_clip() {
    let (dir, ids) = history(&["password", "note"]);

    clipvault(dir.path(), &["delete", &ids[0].to_string()]);

    assert_eq!(previews(dir.path()), vec!["note"]);
}

#[test]
fn private_on_pauses_recording() {
    let (dir, _) = history(&[]);

    clipvault(dir.path(), &["private", "on"]);

    assert!(Store::open(dir.path()).unwrap().private_mode().unwrap());
}

#[test]
fn private_without_an_argument_reports_the_state() {
    let (dir, _) = history(&[]);

    let output = clipvault(dir.path(), &["private"]);

    assert_eq!(stdout(&output), "private mode is off\n");
}

#[test]
fn deleting_an_unknown_clip_fails() {
    let (dir, _) = history(&[]);

    let output = clipvault(dir.path(), &["delete", "42"]);

    assert!(!output.status.success());
}

#[test]
fn list_with_a_count_that_is_not_a_number_fails() {
    let (dir, _) = history(&[]);

    let output = clipvault(dir.path(), &["list", "abc"]);

    assert!(!output.status.success());
}

#[test]
fn an_unknown_command_fails() {
    let (dir, _) = history(&[]);

    let output = clipvault(dir.path(), &["frobnicate"]);

    assert!(!output.status.success());
}
