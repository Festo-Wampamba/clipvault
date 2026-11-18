//! Clipboard capture and paste-back backends.
//!
//! At start-up [`detect`] inspects the current session (README §5) and returns
//! the matching [`ClipboardBackend`]: Windows (`win32`), GNOME Wayland (`gnome`),
//! Wayland data-control (`data_control`, for KDE, Sway and Hyprland), or X11 (`x11`).

#[cfg(target_os = "linux")]
mod data_control;
#[cfg(target_os = "linux")]
mod gnome;
#[cfg(windows)]
mod win32;
#[cfg(target_os = "linux")]
mod x11;

use std::sync::mpsc::Sender;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// MIME type used for UTF-8 text on every platform.
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";
/// MIME type used for images on every platform.
pub const PNG_MIME: &str = "image/png";

/// Copies larger than this per format are skipped (README §7).
pub(crate) const MAX_FORMAT_BYTES: usize = 20 * 1024 * 1024;
/// KDE convention, also set by KeePassXC and other password managers.
#[cfg(target_os = "linux")]
pub(crate) const SENSITIVE_HINT: &str = "x-kde-passwordManagerHint";
/// Text formats, best first; the first one offered is read as [`TEXT_MIME`].
#[cfg(target_os = "linux")]
pub(crate) const TEXT_SOURCES: &[&str] =
    &[TEXT_MIME, "UTF8_STRING", "text/plain", "STRING", "TEXT"];
/// Non-text, non-image formats forwarded under their own MIME name.
#[cfg(target_os = "linux")]
pub(crate) const OTHER_FORWARDED: &[&str] = &[
    "text/html",
    "text/rtf",
    "application/rtf",
    "text/uri-list",
    "x-special/gnome-copied-files",
];
/// Image formats, PNG preferred; others are converted to PNG by the capture pipeline.
#[cfg(target_os = "linux")]
pub(crate) const IMAGE_SOURCES: &[&str] = &[
    PNG_MIME,
    "image/jpeg",
    "image/bmp",
    "image/gif",
    "image/webp",
    "image/tiff",
];
/// MIME names offered when placing [`TEXT_MIME`] on a Wayland or X11 clipboard.
#[cfg(target_os = "linux")]
pub(crate) const TEXT_OFFERED: &[&str] =
    &[TEXT_MIME, "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// One clipboard change: every format the source application offered, plus the
/// application name or path when the platform exposes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawClip {
    pub formats: Vec<(String, Vec<u8>)>,
    pub source_app: Option<String>,
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

/// Picks the backend for the current session (README §5).
pub fn detect() -> Result<Box<dyn ClipboardBackend>> {
    #[cfg(windows)]
    {
        Ok(Box::new(win32::Win32))
    }
    #[cfg(target_os = "linux")]
    {
        detect_linux(|key| std::env::var_os(key))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err("unsupported operating system".into())
    }
}

/// Chooses which offered formats to read: `(offered_name, stored_mime)`.
/// Returns `None` when the copy carries [`SENSITIVE_HINT`].
#[cfg(target_os = "linux")]
pub(crate) fn select_formats(offered: &[impl AsRef<str>]) -> Option<Vec<(&str, &'static str)>> {
    let has = |target: &str| {
        offered
            .iter()
            .find_map(|m| (m.as_ref() == target).then_some(m.as_ref()))
    };
    if has(SENSITIVE_HINT).is_some() {
        return None;
    }
    let mut picked = Vec::new();
    if let Some(source) = TEXT_SOURCES.iter().find_map(|m| has(m)) {
        picked.push((source, TEXT_MIME));
    }
    for &mime in OTHER_FORWARDED {
        if let Some(source) = has(mime) {
            picked.push((source, mime));
        }
    }
    if let Some((source, mime)) = IMAGE_SOURCES
        .iter()
        .find_map(|&m| has(m).map(|source| (source, m)))
    {
        picked.push((source, mime));
    }
    Some(picked)
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum LinuxSession {
    GnomeBridge(std::path::PathBuf),
    GnomeWithoutBridge,
    Wayland { x11_fallback: bool },
    X11,
    None,
}

#[cfg(target_os = "linux")]
fn choose_linux_session(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> LinuxSession {
    let non_empty = |key: &str| var(key).filter(|v| !v.is_empty());
    if let Some(socket) = non_empty(gnome::SOCKET_ENV) {
        return LinuxSession::GnomeBridge(socket.into());
    }
    if non_empty("WAYLAND_DISPLAY").is_some() {
        let is_gnome = non_empty("XDG_CURRENT_DESKTOP").is_some_and(|desktop| {
            desktop
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("gnome")
        });
        if is_gnome {
            return LinuxSession::GnomeWithoutBridge;
        }
        return LinuxSession::Wayland {
            x11_fallback: non_empty("DISPLAY").is_some(),
        };
    }
    if non_empty("DISPLAY").is_some() {
        return LinuxSession::X11;
    }
    LinuxSession::None
}

#[cfg(target_os = "linux")]
fn detect_linux(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Box<dyn ClipboardBackend>> {
    match choose_linux_session(var) {
        LinuxSession::GnomeBridge(socket) => Ok(Box::new(gnome::GnomeBridge::connect(&socket)?)),
        LinuxSession::GnomeWithoutBridge => Err(
            "on GNOME Wayland, the ClipVault Bridge shell extension starts ClipVault itself \
             (see extensions/gnome-shell/README.md)"
                .into(),
        ),
        LinuxSession::Wayland { x11_fallback } => match data_control::DataControl::connect() {
            Ok(backend) => Ok(Box::new(backend)),
            Err(data_control::ConnectError::UnsupportedCompositor) if x11_fallback => {
                eprintln!(
                    "clipvault: Wayland compositor does not offer data-control; \
                     falling back to X11 (captures X11 apps only)"
                );
                Ok(Box::new(x11::X11::connect()?))
            }
            Err(e) => Err(e.into()),
        },
        LinuxSession::X11 => Ok(Box::new(x11::X11::connect()?)),
        LinuxSession::None => {
            Err("no graphical session found: neither WAYLAND_DISPLAY nor DISPLAY is set".into())
        }
    }
}

/// Formats file paths (such as Windows `CF_HDROP` entries) as a `text/uri-list`.
#[cfg(any(windows, test))]
pub(crate) fn paths_to_uri_list(paths: &[impl AsRef<str>]) -> Vec<u8> {
    let mut out = String::new();
    for raw in paths {
        let path = raw.as_ref().trim();
        if path.is_empty() {
            continue;
        }
        let normalised = path.replace('\\', "/");
        if let Some(unc) = normalised.strip_prefix("//") {
            out.push_str("file://");
            out.push_str(unc);
        } else if let Some(abs) = normalised.strip_prefix('/') {
            out.push_str("file:///");
            out.push_str(abs);
        } else {
            out.push_str("file:///");
            out.push_str(&normalised);
        }
        out.push_str("\r\n");
    }
    out.into_bytes()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::*;

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
    fn picks_gnome_bridge_when_its_socket_is_set() {
        let session = choose_linux_session(env_of(&[(
            "CLIPVAULT_BRIDGE_SOCKET",
            "/run/user/1000/clipvault/bridge.sock",
        )]));

        assert_eq!(
            session,
            LinuxSession::GnomeBridge(PathBuf::from("/run/user/1000/clipvault/bridge.sock"))
        );
    }

    #[test]
    fn refuses_gnome_wayland_when_the_extension_did_not_start_clipvault() {
        let session = choose_linux_session(env_of(&[
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME"),
        ]));

        assert_eq!(session, LinuxSession::GnomeWithoutBridge);
    }

    #[test]
    fn picks_wayland_data_control_on_non_gnome_wayland() {
        let session = choose_linux_session(env_of(&[
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("XDG_CURRENT_DESKTOP", "sway"),
            ("DISPLAY", ":0"),
        ]));

        assert_eq!(session, LinuxSession::Wayland { x11_fallback: true });
    }

    #[test]
    fn picks_x11_when_only_display_is_set() {
        let session = choose_linux_session(env_of(&[
            ("DISPLAY", ":99"),
            ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME"),
        ]));

        assert_eq!(session, LinuxSession::X11);
    }

    #[test]
    fn reports_no_session_when_neither_display_is_set() {
        let session = choose_linux_session(env_of(&[]));

        assert_eq!(session, LinuxSession::None);
    }

    #[test]
    fn skips_copies_marked_sensitive_by_password_managers() {
        let offered = [TEXT_MIME, SENSITIVE_HINT];

        assert_eq!(select_formats(&offered), None);
    }

    #[test]
    fn prefers_utf8_text_and_png_over_fallbacks() {
        let offered = ["STRING", "UTF8_STRING", "image/jpeg", PNG_MIME, "text/html"];

        assert_eq!(
            select_formats(&offered),
            Some(vec![
                ("UTF8_STRING", TEXT_MIME),
                ("text/html", "text/html"),
                (PNG_MIME, PNG_MIME),
            ])
        );
    }

    #[test]
    fn converts_windows_drive_and_unc_paths_to_a_uri_list() {
        let paths = [r"C:\Users\u\report.pdf", r"\\nas\share\photo.png"];

        assert_eq!(
            paths_to_uri_list(&paths),
            b"file:///C:/Users/u/report.pdf\r\nfile://nas/share/photo.png\r\n"
        );
    }
}
