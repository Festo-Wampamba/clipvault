//! Windows backend: `AddClipboardFormatListener` (via clipboard-win's `Monitor`),
//! clipboard read/write, and `SendInput` for paste.

use std::io::Cursor;
use std::sync::mpsc::{self, Sender};
use std::thread;

use clipboard_win::{Clipboard, Getter, Monitor, formats, raw};
use image::ImageFormat;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
    VK_CONTROL, VK_V,
};

use crate::{ClipboardBackend, PNG_MIME, RawClip, Result, TEXT_MIME};

const OPEN_ATTEMPTS: usize = 10;
/// Formats password managers add so clipboard tools skip the copy.
const SENSITIVE_FORMATS: [&str; 2] = [
    "ExcludeClipboardContentFromMonitorProcessing",
    "Clipboard Viewer Ignore",
];

pub struct Win32;

impl ClipboardBackend for Win32 {
    fn watch(&mut self, tx: Sender<RawClip>) -> Result<()> {
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::spawn(move || {
            // The monitor's hidden window must live on the thread that pumps its messages.
            let mut monitor = match Monitor::new() {
                Ok(m) => m,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            loop {
                match monitor.recv() {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => {
                        eprintln!("clipvault: clipboard listener failed: {e}");
                        break;
                    }
                }
                match read_clip() {
                    Ok(Some(clip)) => {
                        if tx.send(clip).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("clipvault: could not read the clipboard: {e}"),
                }
            }
        });
        ready_rx.recv()??;
        Ok(())
    }

    fn write(&mut self, mime: &str, data: &[u8]) -> Result<()> {
        let _clipboard = Clipboard::new_attempts(OPEN_ATTEMPTS)?;
        match mime {
            TEXT_MIME => raw::set_string(std::str::from_utf8(data)?)?,
            PNG_MIME => {
                // PNG keeps transparency for apps that read it; the bitmap is for everything else.
                let png_format = raw::register_format("PNG")
                    .ok_or("cannot register the PNG clipboard format")?;
                raw::set(png_format.get(), data)?;
                raw::set_bitmap(&png_to_bmp(data)?)?;
            }
            other => return Err(format!("unsupported clipboard format {other}").into()),
        }
        Ok(())
    }

    fn paste(&mut self) -> Result<()> {
        let inputs = [
            key(VK_CONTROL, 0),
            key(VK_V, 0),
            key(VK_V, KEYEVENTF_KEYUP),
            key(VK_CONTROL, KEYEVENTF_KEYUP),
        ];
        // SAFETY: `inputs` is a live array of fully initialised INPUT structs for the whole call.
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        };
        if sent as usize != inputs.len() {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

fn read_clip() -> Result<Option<RawClip>> {
    let _clipboard = Clipboard::new_attempts(OPEN_ATTEMPTS)?;
    if is_sensitive() {
        return Ok(None);
    }
    let mut found = Vec::new();
    if raw::is_format_avail(formats::CF_UNICODETEXT) {
        let mut text = String::new();
        formats::Unicode.read_clipboard(&mut text)?;
        found.push((TEXT_MIME.to_owned(), text.into_bytes()));
    }
    if let Some(png) = read_png()? {
        found.push((PNG_MIME.to_owned(), png));
    }
    Ok((!found.is_empty()).then_some(RawClip { formats: found }))
}

fn is_sensitive() -> bool {
    let has_marker = SENSITIVE_FORMATS
        .iter()
        .any(|name| raw::register_format(name).is_some_and(|id| raw::is_format_avail(id.get())));
    // Windows clipboard-history convention: a DWORD 0 here means "do not keep this copy".
    let history_opt_out = raw::register_format("CanIncludeInClipboardHistory").is_some_and(|id| {
        let mut value = Vec::new();
        raw::is_format_avail(id.get())
            && formats::RawData(id.get())
                .read_clipboard(&mut value)
                .is_ok()
            && value.get(..4) == Some(&[0, 0, 0, 0])
    });
    has_marker || history_opt_out
}

fn read_png() -> Result<Option<Vec<u8>>> {
    // Browsers and Office offer a "PNG" format that keeps transparency; prefer it.
    if let Some(id) = raw::register_format("PNG").filter(|id| raw::is_format_avail(id.get())) {
        let mut png = Vec::new();
        formats::RawData(id.get()).read_clipboard(&mut png)?;
        return Ok(Some(png));
    }
    if !raw::is_format_avail(formats::CF_BITMAP) {
        return Ok(None);
    }
    let mut bmp = Vec::new();
    formats::Bitmap.read_clipboard(&mut bmp)?;
    let mut png = Vec::new();
    image::load_from_memory_with_format(&bmp, ImageFormat::Bmp)?
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    Ok(Some(png))
}

/// clipboard-win copies only a plain BITMAPINFOHEADER, so encode 24-bit RGB:
/// a 32-bit encode would need the extra colour-mask header that gets dropped.
fn png_to_bmp(png: &[u8]) -> Result<Vec<u8>> {
    let rgb = image::load_from_memory_with_format(png, ImageFormat::Png)?.into_rgb8();
    let mut bmp = Vec::new();
    rgb.write_to(&mut Cursor::new(&mut bmp), ImageFormat::Bmp)?;
    Ok(bmp)
}

fn key(vk: VIRTUAL_KEY, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
