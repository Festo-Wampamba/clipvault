//! GNOME Wayland backend.
//!
//! Mutter does not let background apps read the clipboard, so the ClipVault Bridge
//! shell extension reads it for us. Trust comes from the process tree, not from a socket
//! path anyone could reach: GNOME Shell starts ClipVault itself, opens a one-time socket in
//! a private folder, accepts only the child it started, then deletes the socket. Clipboard
//! data never touches the shared session bus, which any process of this user can monitor.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use rustix::process::{DumpableBehavior, Pid};
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type as MessageType;

use crate::{ClipboardBackend, RawClip, Result};

/// Set by the extension when it starts ClipVault: path of the one-time bridge socket.
pub const SOCKET_ENV: &str = "CLIPVAULT_BRIDGE_SOCKET";
const OBJECT_PATH: &str = "/org/clipvault/Bridge";
const INTERFACE: &str = "org.clipvault.Bridge1";
const GNOME_SHELL_EXE: &str = "/usr/bin/gnome-shell";

pub struct GnomeBridge {
    conn: Connection,
}

impl GnomeBridge {
    /// Connects to the extension that started this process.
    pub fn connect(socket: &Path) -> Result<Self> {
        // First thing: from here on, other processes of this user cannot attach a debugger
        // to ClipVault or read its memory through /proc.
        rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)?;
        let stream = UnixStream::connect(socket).map_err(|e| {
            format!(
                "cannot reach the ClipVault Bridge socket {}: {e}",
                socket.display()
            )
        })?;
        let parent = rustix::process::getppid().ok_or("ClipVault has no parent process")?;
        verify_peer(&stream, parent, Path::new(GNOME_SHELL_EXE))?;
        let conn = zbus::blocking::connection::Builder::unix_stream(stream)
            .p2p()
            .build()?;
        Ok(Self { conn })
    }
}

/// Accepts only `expected_pid` running `expected_exe` as us, with no debugger attached.
/// Another process of this user can swap the socket for its own, but it cannot be our
/// parent, so a fake bridge never sees the clips ClipVault writes.
fn verify_peer(stream: &UnixStream, expected_pid: Pid, expected_exe: &Path) -> Result<()> {
    let peer = rustix::net::sockopt::socket_peercred(stream)?;
    if peer.pid != expected_pid || peer.uid != rustix::process::getuid() {
        return Err(format!(
            "refusing bridge socket: peer is pid {} (uid {}), expected pid {}",
            peer.pid.as_raw_nonzero(),
            peer.uid.as_raw(),
            expected_pid.as_raw_nonzero()
        )
        .into());
    }
    let proc_dir = PathBuf::from(format!("/proc/{}", peer.pid.as_raw_nonzero()));
    let exe = fs::read_link(proc_dir.join("exe"))?;
    if exe != expected_exe {
        return Err(format!(
            "refusing bridge socket: peer runs {}, expected {}",
            exe.display(),
            expected_exe.display()
        )
        .into());
    }
    let status = fs::read_to_string(proc_dir.join("status"))?;
    let tracer = status
        .lines()
        .find_map(|line| line.strip_prefix("TracerPid:"));
    if tracer.map(str::trim) != Some("0") {
        return Err("refusing bridge socket: its owner is under a debugger".into());
    }
    Ok(())
}

impl ClipboardBackend for GnomeBridge {
    fn watch(&mut self, tx: Sender<RawClip>) -> Result<()> {
        let messages = MessageIterator::from(&self.conn);
        thread::spawn(move || {
            for message in messages {
                let message = match message {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("clipvault: bridge connection failed: {e}");
                        break;
                    }
                };
                let header = message.header();
                let is_clip = header.message_type() == MessageType::Signal
                    && header
                        .member()
                        .is_some_and(|m| m.as_str() == "ClipboardChanged");
                if !is_clip {
                    continue;
                }
                match message.body().deserialize::<Vec<(String, Vec<u8>)>>() {
                    Ok(formats) => {
                        if tx.send(RawClip { formats }).is_err() {
                            break;
                        }
                    }
                    Err(e) => eprintln!("clipvault: ignoring malformed ClipboardChanged: {e}"),
                }
            }
            // Dropping `tx` here tells the receiver the bridge is gone.
        });
        Ok(())
    }

    fn write(&mut self, mime: &str, data: &[u8]) -> Result<()> {
        self.conn.call_method(
            None::<&str>,
            OBJECT_PATH,
            Some(INTERFACE),
            "SetClipboard",
            &(mime, data),
        )?;
        Ok(())
    }

    fn paste(&mut self) -> Result<()> {
        self.conn
            .call_method(None::<&str>, OBJECT_PATH, Some(INTERFACE), "Paste", &())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_peer_that_is_not_the_expected_process() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let parent = rustix::process::getppid().unwrap();

        let result = verify_peer(&ours, parent, &std::env::current_exe().unwrap());

        assert!(result.is_err());
    }

    #[test]
    fn rejects_expected_process_running_another_program() {
        let (ours, _theirs) = UnixStream::pair().unwrap();

        let result = verify_peer(&ours, rustix::process::getpid(), Path::new(GNOME_SHELL_EXE));

        assert!(result.is_err());
    }

    #[test]
    fn accepts_expected_process_running_expected_program() {
        let (ours, _theirs) = UnixStream::pair().unwrap();

        let result = verify_peer(
            &ours,
            rustix::process::getpid(),
            &std::env::current_exe().unwrap(),
        );

        assert!(result.is_ok());
    }
}
