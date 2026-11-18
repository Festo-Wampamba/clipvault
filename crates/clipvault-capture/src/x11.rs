//! X11 backend (`x11rb`): XFixes `SelectionNotify` for capture, ICCCM selection
//! ownership (including `INCR` for large payloads) for write-back, and XTest for paste.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::pipe::{PipeFlags, pipe_with};
use x11rb::connection::Connection;
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, PropMode, Property, SELECTION_NOTIFY_EVENT,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::{
    ClipboardBackend, Error, Event, IMAGE_SOURCES, MAX_FORMAT_BYTES, OTHER_FORWARDED, PNG_MIME,
    RawClip, Result, SENSITIVE_HINT, TEXT_MIME, TEXT_OFFERED, TEXT_SOURCES, select_formats,
};

const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Payloads above this size are transferred in chunks via ICCCM `INCR`.
const INCR_CHUNK_BYTES: usize = 256 * 1024;
const MAX_INCR_TRANSFERS: usize = 8;
const RELEASE_POLL: Duration = Duration::from_millis(20);
const RELEASE_TIMEOUT: Duration = Duration::from_secs(1);

const XK_CONTROL_L: u32 = 0xffe3;
const XK_CONTROL_R: u32 = 0xffe4;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_SHIFT_R: u32 = 0xffe2;
const XK_ALT_L: u32 = 0xffe9;
const XK_ALT_R: u32 = 0xffea;
const XK_SUPER_L: u32 = 0xffeb;
const XK_SUPER_R: u32 = 0xffec;
const XK_V: u32 = 0x0076;

const HELD_MODIFIER_KEYSYMS: &[u32] = &[
    XK_SHIFT_L,
    XK_SHIFT_R,
    XK_CONTROL_L,
    XK_CONTROL_R,
    XK_ALT_L,
    XK_ALT_R,
    XK_SUPER_L,
    XK_SUPER_R,
];

enum OwnerCommand {
    Write {
        mime: String,
        data: Vec<u8>,
        reply: Sender<Result<()>>,
    },
}

pub struct X11 {
    display: Option<String>,
    conn: RustConnection,
    owner_win: Window,
    owner_tx: Sender<OwnerCommand>,
    owner_wake: File,
    io_timeout: Duration,
}

impl X11 {
    pub fn connect() -> Result<Self> {
        Self::connect_with_timeout(None, IO_TIMEOUT)
    }

    pub(crate) fn connect_with_timeout(
        display: Option<&str>,
        io_timeout: Duration,
    ) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(display)?;
        conn.xfixes_query_version(5, 0)?.reply()?;
        conn.xtest_get_version(2, 1)?.reply()?;

        let (owner_conn, owner_screen) = x11rb::connect(display)?;
        let owner_atoms = Atoms::intern(&owner_conn)?;
        let owner_win = create_hidden_window(&owner_conn, owner_screen, EventMask::NO_EVENT)?;
        owner_conn.flush()?;

        let (wake_rx, wake_tx) = pipe_with(PipeFlags::CLOEXEC)?;
        set_nonblocking(&wake_rx)?;
        let (owner_tx, owner_rx) = mpsc::channel();
        thread::spawn(move || {
            run_owner_loop(owner_conn, owner_win, owner_atoms, owner_rx, wake_rx);
        });

        let _ = screen_num;
        Ok(Self {
            display: display.map(str::to_owned),
            conn,
            owner_win,
            owner_tx,
            owner_wake: File::from(wake_tx),
            io_timeout,
        })
    }
}

impl ClipboardBackend for X11 {
    fn watch(&mut self, tx: Sender<Event>) -> Result<()> {
        let display = self.display.clone();
        let owner_win = self.owner_win;
        let timeout = self.io_timeout;
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::spawn(move || {
            let setup = (|| -> Result<(RustConnection, Window, Atoms)> {
                let (conn, screen_num) = x11rb::connect(display.as_deref())?;
                conn.xfixes_query_version(5, 0)?.reply()?;
                let atoms = Atoms::intern(&conn)?;
                let win = create_hidden_window(&conn, screen_num, EventMask::PROPERTY_CHANGE)?;
                conn.xfixes_select_selection_input(
                    win,
                    atoms.clipboard,
                    SelectionEventMask::SET_SELECTION_OWNER,
                )?;
                conn.sync()?;
                Ok((conn, win, atoms))
            })();
            let (conn, win, atoms) = match setup {
                Ok(v) => {
                    let _ = ready_tx.send(Ok(()));
                    v
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            run_watch_loop(conn, win, owner_win, atoms, tx, timeout);
        });
        ready_rx
            .recv()
            .map_err(|_| Error::from("the X11 watcher thread stopped before starting"))??;
        Ok(())
    }

    fn write(&mut self, mime: &str, data: &[u8]) -> Result<()> {
        if mime != TEXT_MIME && mime != PNG_MIME {
            return Err(format!("unsupported clipboard format {mime}").into());
        }
        if data.len() > MAX_FORMAT_BYTES {
            return Err(format!("clip larger than {MAX_FORMAT_BYTES} bytes").into());
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.owner_tx
            .send(OwnerCommand::Write {
                mime: mime.to_owned(),
                data: data.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| "the X11 selection owner thread stopped")?;
        let _ = self.owner_wake.write(&[1]);
        reply_rx
            .recv()
            .map_err(|_| Error::from("the X11 selection owner thread stopped"))?
    }

    fn paste(&mut self) -> Result<()> {
        let keymap = KeycodeMap::query(&self.conn)?;
        if !wait_for_modifiers_released(&self.conn, &keymap)? {
            eprintln!(
                "clipvault: shortcut keys still held, not pasting; the clip is on the clipboard"
            );
            return Ok(());
        }
        let ctrl = keymap
            .keycode_for(XK_CONTROL_L)
            .ok_or("X11 keymap has no keycode for Control_L")?;
        let v = keymap
            .keycode_for(XK_V)
            .ok_or("X11 keymap has no keycode for 'v'")?;
        for (event_type, keycode) in [
            (KEY_PRESS_EVENT, ctrl),
            (KEY_PRESS_EVENT, v),
            (KEY_RELEASE_EVENT, v),
            (KEY_RELEASE_EVENT, ctrl),
        ] {
            self.conn.xtest_fake_input(
                event_type,
                keycode,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            )?;
        }
        self.conn.sync()?;
        Ok(())
    }
}

#[derive(Clone)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    multiple: Atom,
    incr: Atom,
    property: Atom,
    sensitive: Atom,
    known: Vec<(Atom, &'static str)>,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self> {
        let clipboard = conn.intern_atom(false, b"CLIPBOARD")?;
        let targets = conn.intern_atom(false, b"TARGETS")?;
        let timestamp = conn.intern_atom(false, b"TIMESTAMP")?;
        let multiple = conn.intern_atom(false, b"MULTIPLE")?;
        let incr = conn.intern_atom(false, b"INCR")?;
        let property = conn.intern_atom(false, b"CLIPVAULT_SELECTION")?;
        let sensitive = conn.intern_atom(false, SENSITIVE_HINT.as_bytes())?;

        let mut names: Vec<&'static str> = Vec::new();
        for &name in TEXT_SOURCES
            .iter()
            .chain(OTHER_FORWARDED.iter())
            .chain(IMAGE_SOURCES.iter())
        {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let cookies: Vec<_> = names
            .iter()
            .map(|name| conn.intern_atom(false, name.as_bytes()))
            .collect::<std::result::Result<_, _>>()?;

        let mut known = Vec::with_capacity(names.len());
        for (name, cookie) in names.into_iter().zip(cookies) {
            known.push((cookie.reply()?.atom, name));
        }

        Ok(Self {
            clipboard: clipboard.reply()?.atom,
            targets: targets.reply()?.atom,
            timestamp: timestamp.reply()?.atom,
            multiple: multiple.reply()?.atom,
            incr: incr.reply()?.atom,
            property: property.reply()?.atom,
            sensitive: sensitive.reply()?.atom,
            known,
        })
    }

    fn name_of(&self, atom: Atom) -> Option<&'static str> {
        if atom == self.sensitive {
            return Some(SENSITIVE_HINT);
        }
        self.known
            .iter()
            .find_map(|&(a, name)| (a == atom).then_some(name))
    }

    fn atom_for(&self, name: &str) -> Option<Atom> {
        self.known
            .iter()
            .find_map(|&(a, n)| (n == name).then_some(a))
    }
}

fn create_hidden_window(
    conn: &RustConnection,
    screen_num: usize,
    event_mask: EventMask,
) -> Result<Window> {
    let screen = &conn.setup().roots[screen_num];
    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_FROM_PARENT as u8,
        win,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(event_mask),
    )?;
    Ok(win)
}

enum ReadStep<T> {
    Done(T),
    Superseded(Window),
}

fn run_watch_loop(
    conn: RustConnection,
    win: Window,
    owner_win: Window,
    atoms: Atoms,
    tx: Sender<Event>,
    timeout: Duration,
) {
    let mut pending_owner: Option<Window> = None;
    loop {
        let selection_owner = match pending_owner.take() {
            Some(owner) => owner,
            None => {
                let event = match conn.wait_for_event() {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!("clipvault: X11 watcher disconnected: {e}");
                        break;
                    }
                };
                let XEvent::XfixesSelectionNotify(notify) = event else {
                    continue;
                };
                if notify.selection != atoms.clipboard || notify.owner == x11rb::NONE {
                    continue;
                }
                notify.owner
            }
        };
        let source_app = (selection_owner != owner_win)
            .then(|| source_app_of(&conn, selection_owner))
            .flatten();
        match read_selection(&conn, win, &atoms, source_app, timeout) {
            Ok(ReadStep::Done(Some(clip))) => {
                if tx.send(Event::Clip(clip)).is_err() {
                    break;
                }
            }
            Ok(ReadStep::Done(None)) => {}
            Ok(ReadStep::Superseded(next_owner)) => pending_owner = Some(next_owner),
            Err(e) => eprintln!("clipvault: could not read X11 clipboard: {e}"),
        }
    }
}

fn source_app_of(conn: &RustConnection, selection_owner: Window) -> Option<String> {
    if let Some(class) = wm_class_of(conn, selection_owner) {
        return Some(class);
    }
    let focus = conn.get_input_focus().ok()?.reply().ok()?.focus;
    if focus == x11rb::NONE || focus == 1 {
        return None;
    }
    wm_class_of(conn, focus)
}

fn wm_class_of(conn: &RustConnection, window: Window) -> Option<String> {
    let reply = conn
        .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()?
        .reply()
        .ok()?;
    let parts: Vec<&[u8]> = reply
        .value
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .collect();
    let chosen = parts.get(1).or_else(|| parts.first())?;
    Some(String::from_utf8_lossy(chosen).into_owned())
}

fn read_selection(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    source_app: Option<String>,
    timeout: Duration,
) -> Result<ReadStep<Option<RawClip>>> {
    let target_atoms = match request_targets(conn, win, atoms, timeout)? {
        ReadStep::Done(Some(list)) => list,
        ReadStep::Done(None) => return Ok(ReadStep::Done(None)),
        ReadStep::Superseded(owner) => return Ok(ReadStep::Superseded(owner)),
    };
    let offered: Vec<&str> = target_atoms
        .into_iter()
        .filter_map(|atom| atoms.name_of(atom))
        .collect();
    let Some(selected) = select_formats(&offered) else {
        return Ok(ReadStep::Done(None));
    };
    if selected.is_empty() {
        return Ok(ReadStep::Done(None));
    }

    let mut formats = Vec::with_capacity(selected.len());
    for (source_mime, target_mime) in selected {
        let Some(target_atom) = atoms.atom_for(source_mime) else {
            continue;
        };
        match request_format(conn, win, atoms, target_atom, timeout) {
            Ok(ReadStep::Done(Some(bytes))) if !bytes.is_empty() => {
                formats.push((target_mime.to_owned(), bytes));
            }
            Ok(ReadStep::Done(_)) => {}
            Ok(ReadStep::Superseded(owner)) => return Ok(ReadStep::Superseded(owner)),
            Err(e) => eprintln!("clipvault: could not read X11 target {source_mime}: {e}"),
        }
    }
    Ok(ReadStep::Done((!formats.is_empty()).then_some(RawClip {
        formats,
        source_app,
    })))
}

fn request_targets(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    timeout: Duration,
) -> Result<ReadStep<Option<Vec<Atom>>>> {
    let _ = conn.delete_property(win, atoms.property);
    conn.convert_selection(
        win,
        atoms.clipboard,
        atoms.targets,
        atoms.property,
        x11rb::CURRENT_TIME,
    )?;
    conn.flush()?;

    let deadline = Instant::now() + timeout;
    loop {
        match wait_event_until(conn, atoms.clipboard, deadline)? {
            ReadStep::Superseded(owner) => return Ok(ReadStep::Superseded(owner)),
            ReadStep::Done(XEvent::SelectionNotify(ev))
                if ev.selection == atoms.clipboard && ev.target == atoms.targets =>
            {
                if ev.property == x11rb::NONE {
                    return Ok(ReadStep::Done(None));
                }
                let reply = conn
                    .get_property(
                        true,
                        win,
                        atoms.property,
                        AtomEnum::ANY,
                        0,
                        (MAX_FORMAT_BYTES / 4) as u32,
                    )?
                    .reply()?;
                let list = reply.value32().map(|it| it.collect()).unwrap_or_default();
                return Ok(ReadStep::Done(Some(list)));
            }
            ReadStep::Done(_) => continue,
        }
    }
}

fn request_format(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    target_atom: Atom,
    timeout: Duration,
) -> Result<ReadStep<Option<Vec<u8>>>> {
    let _ = conn.delete_property(win, atoms.property);
    conn.convert_selection(
        win,
        atoms.clipboard,
        target_atom,
        atoms.property,
        x11rb::CURRENT_TIME,
    )?;
    conn.flush()?;

    let deadline = Instant::now() + timeout;
    loop {
        match wait_event_until(conn, atoms.clipboard, deadline)? {
            ReadStep::Superseded(owner) => return Ok(ReadStep::Superseded(owner)),
            ReadStep::Done(XEvent::SelectionNotify(ev))
                if ev.selection == atoms.clipboard && ev.target == target_atom =>
            {
                if ev.property == x11rb::NONE {
                    return Ok(ReadStep::Done(None));
                }
                let reply = conn
                    .get_property(
                        true,
                        win,
                        atoms.property,
                        AtomEnum::ANY,
                        0,
                        (MAX_FORMAT_BYTES / 4) as u32 + 1,
                    )?
                    .reply()?;
                if reply.type_ == atoms.incr {
                    return read_incr(conn, win, atoms.clipboard, atoms.property, deadline);
                }
                if reply.value.len() > MAX_FORMAT_BYTES {
                    eprintln!("clipvault: skipped X11 format larger than {MAX_FORMAT_BYTES} bytes");
                    return Ok(ReadStep::Done(None));
                }
                return Ok(ReadStep::Done(Some(reply.value)));
            }
            ReadStep::Done(_) => continue,
        }
    }
}

fn read_incr(
    conn: &RustConnection,
    win: Window,
    clipboard: Atom,
    property: Atom,
    deadline: Instant,
) -> Result<ReadStep<Option<Vec<u8>>>> {
    let mut accumulated = Vec::new();
    loop {
        match wait_event_until(conn, clipboard, deadline)? {
            ReadStep::Superseded(owner) => return Ok(ReadStep::Superseded(owner)),
            ReadStep::Done(XEvent::PropertyNotify(ev))
                if ev.window == win && ev.atom == property && ev.state == Property::NEW_VALUE =>
            {
                let chunk = conn
                    .get_property(
                        true,
                        win,
                        property,
                        AtomEnum::ANY,
                        0,
                        (MAX_FORMAT_BYTES / 4) as u32 + 1,
                    )?
                    .reply()?;
                if chunk.value.is_empty() {
                    return Ok(ReadStep::Done(Some(accumulated)));
                }
                if accumulated.len() + chunk.value.len() > MAX_FORMAT_BYTES {
                    eprintln!(
                        "clipvault: skipped X11 INCR format larger than {MAX_FORMAT_BYTES} bytes"
                    );
                    return Ok(ReadStep::Done(None));
                }
                accumulated.extend_from_slice(&chunk.value);
            }
            ReadStep::Done(_) => continue,
        }
    }
}

fn wait_event_until(
    conn: &RustConnection,
    clipboard: Atom,
    deadline: Instant,
) -> Result<ReadStep<XEvent>> {
    loop {
        if let Some(ev) = conn.poll_for_event()? {
            if let XEvent::XfixesSelectionNotify(notify) = &ev
                && notify.selection == clipboard
                && notify.owner != x11rb::NONE
            {
                return Ok(ReadStep::Superseded(notify.owner));
            }
            return Ok(ReadStep::Done(ev));
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("X11 selection transfer timed out")?;
        let ts = Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: i64::from(remaining.subsec_nanos()),
        };
        let stream = conn.stream();
        let mut fds = [PollFd::new(&stream, PollFlags::IN)];
        let ready = poll(&mut fds, Some(&ts))?;
        if ready == 0 {
            return Err("X11 selection transfer timed out".into());
        }
    }
}

struct OwnedClip {
    mime: String,
    data: Arc<[u8]>,
}

struct IncrSend {
    requestor: Window,
    property: Atom,
    target: Atom,
    data: Arc<[u8]>,
    offset: usize,
    started_at: Instant,
}

fn run_owner_loop(
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    cmd_rx: Receiver<OwnerCommand>,
    wake_rx: OwnedFd,
) {
    let mut wake_rx = File::from(wake_rx);
    let mut current: Option<OwnedClip> = None;
    let mut transfers: Vec<IncrSend> = Vec::new();

    loop {
        while let Ok(Some(ev)) = conn.poll_for_event() {
            handle_owner_event(&conn, &atoms, &current, &mut transfers, ev);
        }
        let _ = conn.flush();

        let (x11_ready, wake_ready) = {
            let stream = conn.stream();
            let mut fds = [
                PollFd::new(&stream, PollFlags::IN),
                PollFd::new(&wake_rx, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) => (!fds[0].revents().is_empty(), !fds[1].revents().is_empty()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => break,
            }
        };

        if x11_ready {
            while let Ok(Some(ev)) = conn.poll_for_event() {
                handle_owner_event(&conn, &atoms, &current, &mut transfers, ev);
            }
        }
        if wake_ready {
            drain_file(&mut wake_rx);
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    OwnerCommand::Write { mime, data, reply } => {
                        let res = set_x11_owner(&conn, win, &atoms, &mut current, mime, data);
                        let _ = reply.send(res);
                    }
                }
            }
        }
    }
}

fn set_x11_owner(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    current: &mut Option<OwnedClip>,
    mime: String,
    data: Vec<u8>,
) -> Result<()> {
    conn.set_selection_owner(win, atoms.clipboard, x11rb::CURRENT_TIME)?;
    let owner = conn.get_selection_owner(atoms.clipboard)?.reply()?.owner;
    if owner != win {
        return Err("could not take ownership of the X11 CLIPBOARD selection".into());
    }
    *current = Some(OwnedClip {
        mime,
        data: Arc::from(data),
    });
    Ok(())
}

fn handle_owner_event(
    conn: &RustConnection,
    atoms: &Atoms,
    current: &Option<OwnedClip>,
    transfers: &mut Vec<IncrSend>,
    event: XEvent,
) {
    match event {
        XEvent::SelectionRequest(req) => {
            let prop = if req.property == x11rb::NONE {
                req.target
            } else {
                req.property
            };
            let ok = if req.selection == atoms.clipboard
                && let Some(clip) = current
            {
                answer_selection_request(conn, atoms, clip, transfers, &req, prop).is_ok()
            } else {
                false
            };
            let notify = SelectionNotifyEvent {
                response_type: SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: req.time,
                requestor: req.requestor,
                selection: req.selection,
                target: req.target,
                property: if ok { prop } else { x11rb::NONE },
            };
            let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
            let _ = conn.flush();
        }
        XEvent::PropertyNotify(ev) if ev.state == Property::DELETE => {
            if let Some(idx) = transfers
                .iter()
                .position(|t| t.requestor == ev.window && t.property == ev.atom)
            {
                let transfer = &mut transfers[idx];
                let remaining = &transfer.data[transfer.offset..];
                let chunk_len = remaining.len().min(INCR_CHUNK_BYTES);
                let _ = conn.change_property8(
                    PropMode::REPLACE,
                    transfer.requestor,
                    transfer.property,
                    transfer.target,
                    &remaining[..chunk_len],
                );
                if chunk_len == 0 {
                    let _ = conn.change_window_attributes(
                        transfer.requestor,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
                    );
                    transfers.swap_remove(idx);
                } else {
                    transfer.offset += chunk_len;
                }
                let _ = conn.flush();
            }
        }
        _ => {}
    }
}

fn answer_selection_request(
    conn: &RustConnection,
    atoms: &Atoms,
    clip: &OwnedClip,
    transfers: &mut Vec<IncrSend>,
    req: &SelectionRequestEvent,
    prop: Atom,
) -> Result<()> {
    let offered_atoms = offered_atoms_for(atoms, &clip.mime);
    if req.target == atoms.targets {
        let mut all = vec![atoms.targets, atoms.timestamp, atoms.multiple];
        all.extend_from_slice(&offered_atoms);
        conn.change_property32(PropMode::REPLACE, req.requestor, prop, AtomEnum::ATOM, &all)?;
        return Ok(());
    }
    if req.target == atoms.timestamp {
        conn.change_property32(
            PropMode::REPLACE,
            req.requestor,
            prop,
            AtomEnum::INTEGER,
            &[0u32],
        )?;
        return Ok(());
    }
    if offered_atoms.contains(&req.target) {
        if clip.data.len() <= INCR_CHUNK_BYTES {
            conn.change_property8(
                PropMode::REPLACE,
                req.requestor,
                prop,
                req.target,
                &clip.data,
            )?;
        } else {
            transfers.retain(|t| t.started_at.elapsed() < IO_TIMEOUT);
            if transfers.len() >= MAX_INCR_TRANSFERS {
                transfers.remove(0);
            }
            conn.change_window_attributes(
                req.requestor,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?;
            conn.change_property32(
                PropMode::REPLACE,
                req.requestor,
                prop,
                atoms.incr,
                &[clip.data.len() as u32],
            )?;
            transfers.push(IncrSend {
                requestor: req.requestor,
                property: prop,
                target: req.target,
                data: Arc::clone(&clip.data),
                offset: 0,
                started_at: Instant::now(),
            });
        }
        return Ok(());
    }
    Err("unsupported target".into())
}

fn offered_atoms_for(atoms: &Atoms, mime: &str) -> Vec<Atom> {
    match mime {
        TEXT_MIME => TEXT_OFFERED
            .iter()
            .filter_map(|name| atoms.atom_for(name))
            .collect(),
        PNG_MIME => atoms.atom_for(PNG_MIME).into_iter().collect(),
        _ => Vec::new(),
    }
}

struct KeycodeMap {
    min_keycode: u8,
    keysyms_per_keycode: usize,
    keysyms: Vec<u32>,
}

impl KeycodeMap {
    fn query(conn: &RustConnection) -> Result<Self> {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let count = setup.max_keycode.saturating_sub(min).saturating_add(1);
        let reply = conn.get_keyboard_mapping(min, count)?.reply()?;
        Ok(Self {
            min_keycode: min,
            keysyms_per_keycode: usize::from(reply.keysyms_per_keycode),
            keysyms: reply.keysyms,
        })
    }

    fn keycode_for(&self, target_keysym: u32) -> Option<u8> {
        if self.keysyms_per_keycode == 0 {
            return None;
        }
        for (idx, chunk) in self.keysyms.chunks(self.keysyms_per_keycode).enumerate() {
            if chunk.contains(&target_keysym) {
                return u8::try_from(idx)
                    .ok()
                    .and_then(|offset| self.min_keycode.checked_add(offset));
            }
        }
        None
    }

    fn modifier_keycodes(&self) -> HashSet<u8> {
        HELD_MODIFIER_KEYSYMS
            .iter()
            .filter_map(|&ks| self.keycode_for(ks))
            .collect()
    }
}

fn wait_for_modifiers_released(conn: &RustConnection, map: &KeycodeMap) -> Result<bool> {
    let modifiers = map.modifier_keycodes();
    let deadline = Instant::now() + RELEASE_TIMEOUT;
    loop {
        let state = conn.query_keymap()?.reply()?.keys;
        let any_held = modifiers.iter().any(|&code| {
            let byte = usize::from(code / 8);
            let bit = code % 8;
            state.get(byte).is_some_and(|b| (b & (1 << bit)) != 0)
        });
        if !any_held {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(RELEASE_POLL);
    }
}

fn set_nonblocking(fd: impl AsFd) -> std::result::Result<(), rustix::io::Errno> {
    let flags = fcntl_getfl(&fd)?;
    fcntl_setfl(&fd, flags | OFlags::NONBLOCK)
}

fn drain_file(file: &mut File) {
    let mut buf = [0u8; 64];
    while file.read(&mut buf).is_ok_and(|n| n > 0) {}
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command as ProcessCommand, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};

    use x11rb::protocol::xproto::InputFocus;

    use super::*;

    static NEXT_DISPLAY: AtomicU32 = AtomicU32::new(115);

    struct XvfbServer {
        display: String,
        child: Child,
    }

    impl XvfbServer {
        fn start() -> Self {
            for _ in 0..20 {
                let num = NEXT_DISPLAY.fetch_add(1, Ordering::Relaxed);
                let display = format!(":{num}");
                let Ok(mut child) = ProcessCommand::new("Xvfb")
                    .arg(&display)
                    .args(["-screen", "0", "640x480x24", "-nolisten", "tcp", "-noreset"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                else {
                    continue;
                };
                for _ in 0..40 {
                    thread::sleep(Duration::from_millis(25));
                    if x11rb::connect(Some(&display)).is_ok() {
                        return Self { display, child };
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
            }
            panic!("could not start Xvfb on any test display");
        }
    }

    impl Drop for XvfbServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn next_clip(rx: &Receiver<Event>) -> Vec<(String, Vec<u8>)> {
        match rx.recv_timeout(Duration::from_secs(3)).unwrap() {
            Event::Clip(clip) => clip.formats,
            Event::PasteRequested => panic!("expected a Clip event"),
        }
    }

    fn spawn_hung_owner(display: &str) -> RustConnection {
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let atoms = Atoms::intern(&conn).unwrap();
        let win = create_hidden_window(&conn, screen_num, EventMask::NO_EVENT).unwrap();
        conn.set_selection_owner(win, atoms.clipboard, x11rb::CURRENT_TIME)
            .unwrap();
        conn.sync().unwrap();
        conn
    }

    fn focused_key_window(display: &str) -> (RustConnection, u8, u8) {
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let screen = &conn.setup().roots[screen_num];
        let win = conn.generate_id().unwrap();
        conn.create_window(
            screen.root_depth,
            win,
            screen.root,
            0,
            0,
            100,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE),
        )
        .unwrap();
        conn.map_window(win).unwrap();
        conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
            .unwrap();
        conn.sync().unwrap();
        let keymap = KeycodeMap::query(&conn).unwrap();
        let ctrl = keymap.keycode_for(XK_CONTROL_L).unwrap();
        let v = keymap.keycode_for(XK_V).unwrap();
        (conn, ctrl, v)
    }

    fn drain_key_events(conn: &RustConnection) -> Vec<(u8, u8)> {
        let mut events = Vec::new();
        while let Some(ev) = conn.poll_for_event().unwrap() {
            match ev {
                XEvent::KeyPress(k) => events.push((KEY_PRESS_EVENT, k.detail)),
                XEvent::KeyRelease(k) => events.push((KEY_RELEASE_EVENT, k.detail)),
                _ => {}
            }
        }
        events
    }

    /// Spawns a test X11 selection owner offering `formats` on `display`.
    fn spawn_x11_owner(
        display: &str,
        wm_class: Option<&'static [u8]>,
        formats: Vec<(&'static str, Vec<u8>)>,
    ) {
        let display = display.to_owned();
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::spawn(move || {
            let (conn, screen_num) = x11rb::connect(Some(&display)).unwrap();
            let atoms = Atoms::intern(&conn).unwrap();
            let win = create_hidden_window(&conn, screen_num, EventMask::NO_EVENT).unwrap();
            if let Some(class) = wm_class {
                conn.change_property8(
                    PropMode::REPLACE,
                    win,
                    AtomEnum::WM_CLASS,
                    AtomEnum::STRING,
                    class,
                )
                .unwrap();
            }
            conn.set_selection_owner(win, atoms.clipboard, x11rb::CURRENT_TIME)
                .unwrap();
            conn.sync().unwrap();
            let _ = ready_tx.send(());

            while let Ok(ev) = conn.wait_for_event() {
                if let XEvent::SelectionRequest(req) = ev {
                    let prop = if req.property == x11rb::NONE {
                        req.target
                    } else {
                        req.property
                    };
                    if req.target == atoms.targets {
                        let mut list = vec![atoms.targets, atoms.timestamp];
                        for (mime, _) in &formats {
                            if *mime == SENSITIVE_HINT {
                                list.push(atoms.sensitive);
                            } else if let Some(a) = atoms.atom_for(mime) {
                                list.push(a);
                            }
                        }
                        conn.change_property32(
                            PropMode::REPLACE,
                            req.requestor,
                            prop,
                            AtomEnum::ATOM,
                            &list,
                        )
                        .unwrap();
                    } else if let Some((_, data)) = formats.iter().find(|(m, _)| {
                        (*m == SENSITIVE_HINT && req.target == atoms.sensitive)
                            || atoms.atom_for(m) == Some(req.target)
                    }) {
                        conn.change_property8(
                            PropMode::REPLACE,
                            req.requestor,
                            prop,
                            req.target,
                            data,
                        )
                        .unwrap();
                    }
                    let notify = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: req.time,
                        requestor: req.requestor,
                        selection: req.selection,
                        target: req.target,
                        property: prop,
                    };
                    conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)
                        .unwrap();
                    conn.flush().unwrap();
                }
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn captures_text_and_png_copied_on_x11() {
        let xvfb = XvfbServer::start();
        let mut backend = X11::connect_with_timeout(Some(&xvfb.display), IO_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();

        spawn_x11_owner(
            &xvfb.display,
            None,
            vec![
                ("UTF8_STRING", b"hello x11".to_vec()),
                (PNG_MIME, b"\x89PNG".to_vec()),
            ],
        );

        assert_eq!(
            next_clip(&rx),
            vec![
                (TEXT_MIME.to_owned(), b"hello x11".to_vec()),
                (PNG_MIME.to_owned(), b"\x89PNG".to_vec()),
            ]
        );
    }

    #[test]
    fn captures_the_source_apps_wm_class_on_x11() {
        let xvfb = XvfbServer::start();
        let mut backend = X11::connect_with_timeout(Some(&xvfb.display), IO_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();

        spawn_x11_owner(
            &xvfb.display,
            Some(b"keepassxc\0KeePassXC\0"),
            vec![("UTF8_STRING", b"secret".to_vec())],
        );

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Clip(RawClip {
                formats: vec![(TEXT_MIME.to_owned(), b"secret".to_vec())],
                source_app: Some("KeePassXC".to_owned()),
            })
        );
    }

    #[test]
    fn skips_x11_copies_marked_with_the_password_manager_hint() {
        let xvfb = XvfbServer::start();
        let mut backend = X11::connect_with_timeout(Some(&xvfb.display), IO_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();

        spawn_x11_owner(
            &xvfb.display,
            None,
            vec![
                ("UTF8_STRING", b"hunter2".to_vec()),
                (SENSITIVE_HINT, b"secret".to_vec()),
            ],
        );
        spawn_x11_owner(
            &xvfb.display,
            None,
            vec![("UTF8_STRING", b"visible".to_vec())],
        );

        assert_eq!(
            next_clip(&rx),
            vec![(TEXT_MIME.to_owned(), b"visible".to_vec())]
        );
    }

    #[test]
    fn recovers_when_an_x11_owner_never_answers_convert_selection() {
        let xvfb = XvfbServer::start();
        let mut backend =
            X11::connect_with_timeout(Some(&xvfb.display), Duration::from_secs(5)).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();

        let _hung = spawn_hung_owner(&xvfb.display);
        spawn_x11_owner(
            &xvfb.display,
            None,
            vec![("UTF8_STRING", b"rescued".to_vec())],
        );

        assert_eq!(
            next_clip(&rx),
            vec![(TEXT_MIME.to_owned(), b"rescued".to_vec())]
        );
    }

    #[test]
    fn write_and_watch_round_trip_large_payloads_via_incr() {
        let xvfb = XvfbServer::start();
        let mut backend = X11::connect_with_timeout(Some(&xvfb.display), IO_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();
        let large_png = vec![0x42; INCR_CHUNK_BYTES * 2 + 123];

        backend.write(PNG_MIME, &large_png).unwrap();

        assert_eq!(next_clip(&rx), vec![(PNG_MIME.to_owned(), large_png)]);
    }

    #[test]
    fn paste_sends_ctrl_v_through_xtest_to_the_focused_window() {
        let xvfb = XvfbServer::start();
        let (conn, ctrl, v) = focused_key_window(&xvfb.display);
        let mut backend = X11::connect_with_timeout(Some(&xvfb.display), IO_TIMEOUT).unwrap();

        backend.paste().unwrap();

        assert_eq!(
            drain_key_events(&conn),
            vec![
                (KEY_PRESS_EVENT, ctrl),
                (KEY_PRESS_EVENT, v),
                (KEY_RELEASE_EVENT, v),
                (KEY_RELEASE_EVENT, ctrl),
            ]
        );
    }
}
