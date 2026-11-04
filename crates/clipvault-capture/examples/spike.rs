//! Phase 0 spike: prints every copy and proves paste-back.
//!
//! Paste-back is triggered by you, never by clipboard contents, so no app can make the
//! spike paste your previous copy into it. Linux: send SIGUSR1 (last text) or SIGUSR2
//! (last image). Windows: type `t` or `i` and press Enter. Output goes to stdout: the
//! terminal on Windows, the journal on GNOME (where the extension starts the spike).

use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use clipvault_capture::{PNG_MIME, RawClip, Result, TEXT_MIME, detect};

/// Time to switch to the window you want to paste into.
const FOCUS_DELAY: Duration = Duration::from_secs(3);
const PREVIEW_CHARS: usize = 60;

enum Event {
    Clip(RawClip),
    Paste(&'static str),
}

fn main() -> Result<()> {
    let mut backend = detect()?;
    let (clip_tx, clips) = mpsc::channel();
    backend.watch(clip_tx)?;

    let (tx, events) = mpsc::channel();
    let clip_events = tx.clone();
    thread::spawn(move || {
        for clip in clips {
            if clip_events.send(Event::Clip(clip)).is_err() {
                break;
            }
        }
    });
    listen_for_paste_requests(tx)?;
    println!("ClipVault spike: watching the clipboard.");

    let mut last_text: Option<Vec<u8>> = None;
    let mut last_image: Option<Vec<u8>> = None;
    for event in events {
        match event {
            Event::Clip(clip) => {
                print_clip(&clip);
                if let Some(text) = format_of(&clip, TEXT_MIME) {
                    last_text = Some(text.to_vec());
                }
                if let Some(image) = format_of(&clip, PNG_MIME) {
                    last_image = Some(image.to_vec());
                }
            }
            Event::Paste(mime) => {
                let last = if mime == TEXT_MIME {
                    &last_text
                } else {
                    &last_image
                };
                let Some(data) = last.clone() else {
                    println!("nothing to paste yet: copy something as {mime} first");
                    continue;
                };
                println!(
                    "pasting in {} s: focus the target window",
                    FOCUS_DELAY.as_secs()
                );
                thread::sleep(FOCUS_DELAY);
                backend.write(mime, &data)?;
                backend.paste()?;
                println!("pasted {} bytes as {mime}", data.len());
            }
        }
    }
    Err("the clipboard watcher stopped".into())
}

#[cfg(unix)]
fn listen_for_paste_requests(tx: Sender<Event>) -> Result<()> {
    use signal_hook::consts::{SIGUSR1, SIGUSR2};

    let mut signals = signal_hook::iterator::Signals::new([SIGUSR1, SIGUSR2])?;
    thread::spawn(move || {
        for signal in signals.forever() {
            let mime = if signal == SIGUSR1 {
                TEXT_MIME
            } else {
                PNG_MIME
            };
            if tx.send(Event::Paste(mime)).is_err() {
                break;
            }
        }
    });
    println!(
        "Paste back with `pkill -USR1 -x spike` (last text) or `pkill -USR2 -x spike` (last image)."
    );
    Ok(())
}

#[cfg(windows)]
fn listen_for_paste_requests(tx: Sender<Event>) -> Result<()> {
    thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let mime = match line.as_deref().map(str::trim) {
                Ok("t") => TEXT_MIME,
                Ok("i") => PNG_MIME,
                Ok(_) => continue,
                Err(_) => break,
            };
            if tx.send(Event::Paste(mime)).is_err() {
                break;
            }
        }
    });
    println!("Paste back by typing t (last text) or i (last image) and pressing Enter.");
    Ok(())
}

fn format_of<'a>(clip: &'a RawClip, mime: &str) -> Option<&'a [u8]> {
    clip.formats
        .iter()
        .find(|(m, _)| m == mime)
        .map(|(_, data)| data.as_slice())
}

fn print_clip(clip: &RawClip) {
    for (mime, data) in &clip.formats {
        let preview = if mime.starts_with("text/") {
            let text: String = String::from_utf8_lossy(data)
                .chars()
                .take(PREVIEW_CHARS)
                .collect();
            format!("{text:?}")
        } else {
            String::from("(binary)")
        };
        println!("{mime:<30} {:>10} bytes  {preview}", data.len());
    }
    println!();
}
