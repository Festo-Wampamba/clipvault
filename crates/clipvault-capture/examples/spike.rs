//! Phase 0 spike: prints every copy and proves paste-back from history.
//!
//! Paste-back puts the clip *before* the current one into the focused window, and only
//! when you ask in a way no other program can fake. GNOME: press Super+Shift+V, a key
//! press GNOME Shell itself sees. Windows: press Enter in the spike's terminal, then focus
//! the target window within 3 seconds. Output goes to stdout: the terminal on Windows, the
//! journal on GNOME (where the extension starts the spike).

use std::sync::mpsc;

use clipvault_capture::{Event, PNG_MIME, RawClip, Result, TEXT_MIME, detect};

const PREVIEW_CHARS: usize = 60;

fn main() -> Result<()> {
    let mut backend = detect()?;
    let (tx, events) = mpsc::channel();
    #[cfg(windows)]
    listen_for_enter(tx.clone());
    #[cfg(not(windows))]
    println!("Press Super+Shift+V to paste the previous clip into the focused window.");
    backend.watch(tx)?;
    println!("ClipVault spike: watching the clipboard.");

    let mut current: Option<RawClip> = None;
    let mut previous: Option<RawClip> = None;
    for event in events {
        match event {
            Event::Clip(clip) => {
                print_clip(&clip);
                previous = current.replace(clip);
            }
            Event::PasteRequested => {
                let Some(clip) = &previous else {
                    println!("nothing to paste yet: copy two things first");
                    continue;
                };
                // Image first: an image copy often carries a text label as well.
                let Some((mime, data)) = [PNG_MIME, TEXT_MIME]
                    .into_iter()
                    .find_map(|mime| format_of(clip, mime).map(|data| (mime, data.to_vec())))
                else {
                    println!("the previous clip has no text or PNG to paste");
                    continue;
                };
                backend.write(mime, &data)?;
                backend.paste()?;
                println!("pasted the previous clip: {} bytes as {mime}", data.len());
            }
        }
    }
    Err("the clipboard watcher stopped".into())
}

/// Windows spike only: Enter in this terminal asks for a paste. Only you can type here.
#[cfg(windows)]
fn listen_for_enter(tx: mpsc::Sender<Event>) {
    use std::thread;
    use std::time::Duration;

    const FOCUS_DELAY: Duration = Duration::from_secs(3);
    println!(
        "Press Enter here, then focus the target window within 3 seconds, to paste the previous clip."
    );
    thread::spawn(move || {
        for line in std::io::stdin().lines() {
            if line.is_err() {
                break;
            }
            thread::sleep(FOCUS_DELAY);
            if tx.send(Event::PasteRequested).is_err() {
                break;
            }
        }
    });
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
