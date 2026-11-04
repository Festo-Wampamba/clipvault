//! Clipboard capture and paste-back backends.
//!
//! Phase 0 spike: proves capture and paste-back on GNOME Wayland (through the
//! ClipVault Bridge shell extension) and on Windows. The data-control and X11
//! backends land in Phase 1.

#[cfg(target_os = "linux")]
mod gnome;
#[cfg(windows)]
mod win32;

use std::sync::mpsc::Sender;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// MIME type used for UTF-8 text on every platform.
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";
/// MIME type used for images on every platform.
pub const PNG_MIME: &str = "image/png";

/// One clipboard change: every format the source application offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawClip {
    pub formats: Vec<(String, Vec<u8>)>,
}

/// Something a backend reports while watching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The clipboard changed.
    Clip(RawClip),
    /// The user pressed the paste-back shortcut. Only a key press the desktop itself saw
    /// produces this, never a message another program can send.
    PasteRequested,
}

/// Implemented once per platform, so the rest of the app never knows which OS it is on.
pub trait ClipboardBackend: Send {
    /// Starts watching in the background and sends every [`Event`] on `tx`.
    /// Copies flagged as sensitive by password managers are never sent.
    fn watch(&mut self, tx: Sender<Event>) -> Result<()>;
    /// Puts `data` on the system clipboard as `mime` (`TEXT_MIME` or `PNG_MIME`).
    fn write(&mut self, mime: &str, data: &[u8]) -> Result<()>;
    /// Sends Ctrl+V to the focused window.
    fn paste(&mut self) -> Result<()>;
}

/// Picks the backend for the current session.
pub fn detect() -> Result<Box<dyn ClipboardBackend>> {
    #[cfg(windows)]
    {
        Ok(Box::new(win32::Win32))
    }
    #[cfg(target_os = "linux")]
    {
        match std::env::var_os(gnome::SOCKET_ENV) {
            Some(socket) => Ok(Box::new(gnome::GnomeBridge::connect(socket.as_ref())?)),
            None => Err(
                "on Linux the Phase 0 spike covers GNOME Wayland only, where the \
                         ClipVault Bridge extension starts ClipVault itself \
                         (see extensions/gnome-shell/README.md)"
                    .into(),
            ),
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err("unsupported operating system".into())
    }
}
