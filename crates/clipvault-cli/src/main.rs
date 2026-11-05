//! `clipvault`: records clipboard history (`clipvault run`) and manages it from the terminal.
//!
//! Every command opens the same history database; there is no socket or other channel
//! between them. `run` holds a lock file so only one recorder runs at a time.

mod daemon;

use std::env;
use std::ffi::OsString;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clipvault_core::{ClipId, ClipSummary, Store};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const USAGE: &str = "usage: clipvault <command>

  run               record the clipboard (on GNOME, the ClipVault Bridge extension starts this)
  list [N]          show the N most recent clips (default 20), newest first
  search QUERY      find clips containing every word; /pattern/ for a regex
  pin ID            keep a clip forever
  unpin ID          let a clip expire again
  delete ID         remove a clip and wipe it from disk
  clear             remove every clip that is not pinned
  private [on|off]  pause or resume recording; without on/off, show which";

const DEFAULT_LIMIT: usize = 20;
const PREVIEW_CHARS: usize = 70;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args.iter().map(String::as_str).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("clipvault: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[&str]) -> Result<()> {
    if matches!(args, ["help" | "--help" | "-h"]) {
        println!("{USAGE}");
        return Ok(());
    }
    let dir = data_dir()?;
    match args {
        ["run"] => {
            // Opening the history first creates its folder private (0700) before the lock
            // file goes in it.
            let mut store = Store::open(&dir)?;
            let _lock = lock(&dir)?;
            daemon::run(&mut store)
        }
        ["list"] => print_clips(&Store::open(&dir)?.recent(DEFAULT_LIMIT)?),
        ["list", count] => {
            let count = count
                .parse()
                .map_err(|_| format!("not a number: {count}"))?;
            print_clips(&Store::open(&dir)?.recent(count)?)
        }
        ["search", query @ ..] if !query.is_empty() => {
            print_clips(&Store::open(&dir)?.search(&query.join(" "), DEFAULT_LIMIT)?)
        }
        ["pin", id] => Ok(Store::open(&dir)?.set_pinned(parse_id(id)?, true)?),
        ["unpin", id] => Ok(Store::open(&dir)?.set_pinned(parse_id(id)?, false)?),
        ["delete", id] => Ok(Store::open(&dir)?.delete(parse_id(id)?)?),
        ["clear"] => {
            let removed = Store::open(&dir)?.clear_unpinned()?;
            println!("removed {removed} clips");
            Ok(())
        }
        ["private"] => {
            let on = Store::open(&dir)?.private_mode()?;
            println!("private mode is {}", if on { "on" } else { "off" });
            Ok(())
        }
        ["private", "on"] => Ok(Store::open(&dir)?.set_private_mode(true)?),
        ["private", "off"] => Ok(Store::open(&dir)?.set_private_mode(false)?),
        _ => Err(USAGE.into()),
    }
}

fn data_dir() -> Result<PathBuf> {
    data_dir_from(|key| env::var_os(key))
}

/// `CLIPVAULT_DATA_DIR`, else README §8: `$XDG_DATA_HOME/clipvault` (normally
/// `~/.local/share/clipvault`) on Linux, `%APPDATA%\ClipVault` on Windows. Only absolute
/// paths are used, so an empty or relative variable can never put the history in whatever
/// folder the command happens to run in.
fn data_dir_from(var: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let absolute = |key: &str| var(key).map(PathBuf::from).filter(|p| p.is_absolute());
    if var("CLIPVAULT_DATA_DIR").is_some() {
        return absolute("CLIPVAULT_DATA_DIR")
            .ok_or_else(|| "CLIPVAULT_DATA_DIR must be an absolute path".into());
    }
    #[cfg(windows)]
    {
        let appdata = absolute("APPDATA").ok_or("APPDATA is not an absolute path")?;
        Ok(appdata.join("ClipVault"))
    }
    #[cfg(not(windows))]
    {
        let data_home = absolute("XDG_DATA_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".local/share")))
            .ok_or("neither XDG_DATA_HOME nor HOME is an absolute path")?;
        Ok(data_home.join("clipvault"))
    }
}

/// Holds an exclusive lock on `clipvault.lock` in the (existing) history folder for as long
/// as the returned file lives. The OS releases it if ClipVault exits or crashes.
fn lock(dir: &Path) -> Result<File> {
    let file = File::create(dir.join("clipvault.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err("ClipVault is already running".into()),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

fn parse_id(text: &str) -> Result<ClipId> {
    text.parse()
        .map_err(|_| format!("not a clip id: {text}").into())
}

fn print_clips(clips: &[ClipSummary]) -> Result<()> {
    for clip in clips {
        let pin = if clip.pinned { "📌" } else { "  " };
        println!(
            "{:>6} {pin} {:<5}  {}",
            clip.id,
            clip.kind.as_str(),
            one_line(&clip.preview)
        );
    }
    Ok(())
}

/// The preview on one line, shortened to fit a terminal row.
fn one_line(preview: &str) -> String {
    let flat: String = preview
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut chars = flat.chars();
    let short: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_recorder_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let _first = lock(dir.path()).unwrap();

        let second = lock(dir.path());

        assert_eq!(
            second.unwrap_err().to_string(),
            "ClipVault is already running"
        );
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn a_relative_clipvault_data_dir_is_refused() {
        let dir = data_dir_from(env_of(&[("CLIPVAULT_DATA_DIR", "history")]));

        assert!(dir.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn xdg_data_home_is_used_when_absolute() {
        let dir = data_dir_from(env_of(&[("XDG_DATA_HOME", "/data"), ("HOME", "/home/u")]));

        assert_eq!(dir.unwrap(), PathBuf::from("/data/clipvault"));
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_xdg_data_home_falls_back_to_home() {
        let dir = data_dir_from(env_of(&[("XDG_DATA_HOME", "data"), ("HOME", "/home/u")]));

        assert_eq!(
            dir.unwrap(),
            PathBuf::from("/home/u/.local/share/clipvault")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_home_is_refused() {
        let dir = data_dir_from(env_of(&[("HOME", "")]));

        assert!(dir.is_err());
    }

    #[test]
    fn previews_are_flattened_to_one_line() {
        assert_eq!(one_line("a\nb\tc"), "a b c");
    }

    #[test]
    fn long_previews_are_shortened() {
        assert_eq!(
            one_line(&"x".repeat(100)).chars().count(),
            PREVIEW_CHARS + 1
        );
    }
}
