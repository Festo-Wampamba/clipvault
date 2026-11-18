//! Wayland `ext-data-control-v1` and `zwlr-data-control-v1` backend (KDE Plasma, Sway,
//! Hyprland and other wlroots compositors).
//!
//! Prefers the standard `ext_data_control_manager_v1` protocol and falls back to
//! `zwlr_data_control_manager_v1`. Paste-back uses `zwp_virtual_keyboard_manager_v1`
//! where the compositor offers it (wlroots, Hyprland) and falls back to the XDG
//! `RemoteDesktop` portal on KDE Plasma.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, OFlags, fcntl_getfl, fcntl_setfl, memfd_create};
use rustix::pipe::{PipeFlags, pipe_with};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{
        EVT_DATA_OFFER_OPCODE as EXT_OFFER_OPCODE, Event as ExtDeviceEvent, ExtDataControlDeviceV1,
    },
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{Event as ExtOfferEvent, ExtDataControlOfferV1},
    ext_data_control_source_v1::{Event as ExtSourceEvent, ExtDataControlSourceV1},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{
        EVT_DATA_OFFER_OPCODE as ZWLR_OFFER_OPCODE, Event as ZwlrDeviceEvent,
        ZwlrDataControlDeviceV1,
    },
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{Event as ZwlrOfferEvent, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{Event as ZwlrSourceEvent, ZwlrDataControlSourceV1},
};
use zbus::blocking::{Connection as DbusConnection, MessageIterator};
use zbus::message::Type as MessageType;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::{
    ClipboardBackend, Error, Event, MAX_FORMAT_BYTES, PNG_MIME, RawClip, Result, TEXT_MIME,
    TEXT_OFFERED, select_formats,
};

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PENDING_OFFERS: usize = 16;

/// Linux evdev keycodes (`<LCTL>` = 29 + 8 = 37, `<AB04>` = 47 + 8 = 55 in XKB).
const KEY_LEFTCTRL: u32 = 29;
const KEY_V: u32 = 47;
const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;
const MOD_CONTROL: u32 = 4;
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// X11 keysyms used by `org.freedesktop.portal.RemoteDesktop.NotifyKeyboardKeysym`.
const XK_CONTROL_L: i32 = 0xffe3;
const XK_V: i32 = 0x0076;

const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

/// Minimal XKB keymap mapping evdev 29 to `Control_L` and 47 to `v`, with a trailing NUL.
const VIRTUAL_KEYMAP: &[u8] = b"xkb_keymap {
  xkb_keycodes \"clipvault\" {
    minimum = 8;
    maximum = 255;
    <LCTL> = 37;
    <AB04> = 55;
  };
  xkb_types \"clipvault\" {
    virtual_modifiers NumLock;
    type \"ONE_LEVEL\" {
      modifiers= none;
      level_name[Level1]= \"Any\";
    };
    type \"TWO_LEVEL\" {
      modifiers= Shift;
      map[Shift]= Level2;
      level_name[Level1]= \"Base\";
      level_name[Level2]= \"Shift\";
    };
  };
  xkb_compatibility \"clipvault\" {
    interpret Any+AnyOf(all) {
      action= SetMods(modifiers=modMapMods,clearLocks);
    };
  };
  xkb_symbols \"clipvault\" {
    key <LCTL> { [ Control_L ] };
    key <AB04> { [ v, V ] };
    modifier_map Control { <LCTL> };
  };
};\0";

#[derive(Debug)]
pub(crate) enum ConnectError {
    /// The compositor does not advertise `ext-data-control-v1` or `zwlr-data-control-v1`.
    UnsupportedCompositor,
    Other(Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedCompositor => write!(
                f,
                "Wayland compositor does not advertise ext-data-control-v1 or zwlr-data-control-v1"
            ),
            Self::Other(e) => Display::fmt(e, f),
        }
    }
}

use fmt::Display;

impl std::error::Error for ConnectError {}

enum Command {
    Watch {
        tx: Sender<Event>,
        ready: Sender<()>,
    },
    Write {
        mime: String,
        data: Vec<u8>,
        reply: Sender<Result<()>>,
    },
    Paste {
        reply: Sender<Result<bool>>,
    },
}

pub struct DataControl {
    cmd_tx: Sender<Command>,
    wake_tx: File,
    portal: Option<PortalPaster>,
    io_timeout: Duration,
}

impl DataControl {
    pub fn connect() -> std::result::Result<Self, ConnectError> {
        let conn = Connection::connect_to_env().map_err(|e| ConnectError::Other(e.into()))?;
        Self::connect_with_timeout(conn, IO_TIMEOUT)
    }

    pub(crate) fn connect_with_timeout(
        conn: Connection,
        io_timeout: Duration,
    ) -> std::result::Result<Self, ConnectError> {
        let mut event_queue: EventQueue<WaylandState> = conn.new_event_queue();
        let qh = event_queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = WaylandState::new(io_timeout);
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| ConnectError::Other(e.into()))?;

        let seat = state.seat.clone().ok_or_else(|| {
            ConnectError::Other("Wayland compositor advertised no wl_seat".into())
        })?;
        let manager = state
            .ext_manager
            .take()
            .map(Manager::Ext)
            .or_else(|| state.zwlr_manager.take().map(Manager::Zwlr))
            .ok_or(ConnectError::UnsupportedCompositor)?;

        let device = manager.get_data_device(&seat, &qh);
        state.manager = Some(manager);
        state.device = Some(device);
        // Consume the compositor's initial selection announcement before `watch` is registered.
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| ConnectError::Other(e.into()))?;

        let (wake_rx, wake_tx) =
            pipe_with(PipeFlags::CLOEXEC).map_err(|e| ConnectError::Other(e.into()))?;
        set_nonblocking(&wake_rx).map_err(|e| ConnectError::Other(e.into()))?;
        let (cmd_tx, cmd_rx) = mpsc::channel();
        thread::spawn(move || run_worker(conn, event_queue, state, cmd_rx, wake_rx));

        Ok(Self {
            cmd_tx,
            wake_tx: File::from(wake_tx),
            portal: None,
            io_timeout,
        })
    }

    fn send_command(&mut self, cmd: Command) -> Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| "the Wayland data-control worker stopped")?;
        let _ = self.wake_tx.write(&[1]);
        Ok(())
    }
}

impl ClipboardBackend for DataControl {
    fn watch(&mut self, tx: Sender<Event>) -> Result<()> {
        let (ready_tx, ready_rx) = mpsc::channel();
        self.send_command(Command::Watch {
            tx,
            ready: ready_tx,
        })?;
        ready_rx
            .recv()
            .map_err(|_| "the Wayland data-control worker stopped")?;
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
        self.send_command(Command::Write {
            mime: mime.to_owned(),
            data: data.to_vec(),
            reply: reply_tx,
        })?;
        reply_rx
            .recv()
            .map_err(|_| Error::from("the Wayland data-control worker stopped"))?
    }

    fn paste(&mut self) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_command(Command::Paste { reply: reply_tx })?;
        let handled_by_vk = reply_rx
            .recv()
            .map_err(|_| Error::from("the Wayland data-control worker stopped"))??;
        if handled_by_vk {
            return Ok(());
        }
        if self.portal.is_none() {
            match PortalPaster::connect(self.io_timeout) {
                Ok(portal) => self.portal = Some(portal),
                Err(e) => {
                    eprintln!(
                        "clipvault: could not send Ctrl+V ({e}); \
                         the clip is on the clipboard, press Ctrl+V to paste"
                    );
                    return Ok(());
                }
            }
        }
        if let Some(portal) = &mut self.portal
            && let Err(e) = portal.paste()
        {
            eprintln!(
                "clipvault: could not send Ctrl+V ({e}); \
                 the clip is on the clipboard, press Ctrl+V to paste"
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
enum Manager {
    Ext(ExtDataControlManagerV1),
    Zwlr(ZwlrDataControlManagerV1),
}

impl Manager {
    fn get_data_device(&self, seat: &WlSeat, qh: &QueueHandle<WaylandState>) -> Device {
        match self {
            Self::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
            Self::Zwlr(m) => Device::Zwlr(m.get_data_device(seat, qh, ())),
        }
    }

    fn create_data_source(&self, qh: &QueueHandle<WaylandState>) -> Source {
        match self {
            Self::Ext(m) => Source::Ext(m.create_data_source(qh, ())),
            Self::Zwlr(m) => Source::Zwlr(m.create_data_source(qh, ())),
        }
    }
}

#[derive(Clone)]
enum Device {
    Ext(ExtDataControlDeviceV1),
    Zwlr(ZwlrDataControlDeviceV1),
}

impl Device {
    fn set_selection(&self, source: Option<&Source>) {
        match (self, source) {
            (Self::Ext(d), Some(Source::Ext(s))) => d.set_selection(Some(s)),
            (Self::Ext(d), None) => d.set_selection(None),
            (Self::Zwlr(d), Some(Source::Zwlr(s))) => d.set_selection(Some(s)),
            (Self::Zwlr(d), None) => d.set_selection(None),
            _ => unreachable!("manager and device protocol always match"),
        }
    }
}

#[derive(Clone)]
enum Source {
    Ext(ExtDataControlSourceV1),
    Zwlr(ZwlrDataControlSourceV1),
}

impl Source {
    fn id(&self) -> ObjectId {
        match self {
            Self::Ext(s) => s.id(),
            Self::Zwlr(s) => s.id(),
        }
    }

    fn offer(&self, mime: &str) {
        match self {
            Self::Ext(s) => s.offer(mime.to_owned()),
            Self::Zwlr(s) => s.offer(mime.to_owned()),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(s) => s.destroy(),
            Self::Zwlr(s) => s.destroy(),
        }
    }
}

#[derive(Clone)]
enum Offer {
    Ext(ExtDataControlOfferV1),
    Zwlr(ZwlrDataControlOfferV1),
}

impl Offer {
    fn id(&self) -> ObjectId {
        match self {
            Self::Ext(o) => o.id(),
            Self::Zwlr(o) => o.id(),
        }
    }

    fn receive(&self, mime: &str, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(o) => o.receive(mime.to_owned(), fd),
            Self::Zwlr(o) => o.receive(mime.to_owned(), fd),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(o) => o.destroy(),
            Self::Zwlr(o) => o.destroy(),
        }
    }
}

struct ActiveSource {
    source: Source,
    mime: String,
    data: Arc<[u8]>,
}

struct WaylandState {
    seat: Option<WlSeat>,
    ext_manager: Option<ExtDataControlManagerV1>,
    zwlr_manager: Option<ZwlrDataControlManagerV1>,
    vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
    manager: Option<Manager>,
    device: Option<Device>,
    offers: HashMap<ObjectId, (Offer, Vec<String>)>,
    active_source: Option<ActiveSource>,
    events_tx: Option<Sender<Event>>,
    generation: Arc<AtomicU64>,
    io_timeout: Duration,
}

impl WaylandState {
    fn new(io_timeout: Duration) -> Self {
        Self {
            seat: None,
            ext_manager: None,
            zwlr_manager: None,
            vk_manager: None,
            manager: None,
            device: None,
            offers: HashMap::new(),
            active_source: None,
            events_tx: None,
            generation: Arc::new(AtomicU64::new(0)),
            io_timeout,
        }
    }

    fn record_data_offer(&mut self, offer: Offer) {
        if self.offers.len() >= MAX_PENDING_OFFERS {
            for (_, (old, _)) in self.offers.drain() {
                old.destroy();
            }
        }
        self.offers.insert(offer.id(), (offer, Vec::new()));
    }

    fn record_offer_mime(&mut self, id: &ObjectId, mime: String) {
        if let Some((_, mimes)) = self.offers.get_mut(id) {
            mimes.push(mime);
        }
    }

    fn discard_offer(&mut self, offer: Option<Offer>) {
        if let Some(offer) = offer {
            self.offers.remove(&offer.id());
            offer.destroy();
        }
    }

    fn handle_selection(&mut self, conn: &Connection, offer: Option<Offer>) {
        let Some(offer) = offer else {
            return;
        };
        let offered = self
            .offers
            .remove(&offer.id())
            .map(|(_, mimes)| mimes)
            .unwrap_or_default();
        let Some(tx) = self.events_tx.clone() else {
            offer.destroy();
            return;
        };
        let Some(selected) = select_formats(&offered) else {
            offer.destroy();
            return;
        };
        if selected.is_empty() {
            offer.destroy();
            return;
        }

        let mut pipes = Vec::with_capacity(selected.len());
        for (source_mime, target_mime) in selected {
            // Only the reader side is non-blocking: the selection owner receives `write_fd`
            // and may expect standard blocking writes.
            let Ok((read_fd, write_fd)) = pipe_with(PipeFlags::CLOEXEC) else {
                continue;
            };
            if set_nonblocking(&read_fd).is_err() {
                continue;
            }
            offer.receive(source_mime, write_fd.as_fd());
            pipes.push((target_mime, read_fd));
        }
        offer.destroy();
        let _ = conn.flush();
        if pipes.is_empty() {
            return;
        }

        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let latest = Arc::clone(&self.generation);
        let timeout = self.io_timeout;
        thread::spawn(move || {
            let mut formats = Vec::with_capacity(pipes.len());
            for (mime, fd) in pipes {
                match read_pipe_bounded(fd, MAX_FORMAT_BYTES, timeout) {
                    Ok(Some(bytes)) if !bytes.is_empty() => {
                        formats.push((mime.to_owned(), bytes));
                    }
                    Ok(Some(_)) | Ok(None) => {}
                    Err(e) => eprintln!("clipvault: could not read Wayland offer ({mime}): {e}"),
                }
            }
            if !formats.is_empty() && latest.load(Ordering::Acquire) == generation {
                let _ = tx.send(Event::Clip(RawClip {
                    formats,
                    source_app: None,
                }));
            }
        });
    }

    fn handle_source_send(&self, source_id: &ObjectId, mime: &str, fd: OwnedFd) {
        let Some(active) = &self.active_source else {
            return;
        };
        if &active.source.id() != source_id {
            return;
        }
        let allowed = match active.mime.as_str() {
            TEXT_MIME => TEXT_OFFERED.contains(&mime),
            PNG_MIME => mime == PNG_MIME,
            _ => false,
        };
        if !allowed {
            return;
        }
        let data = Arc::clone(&active.data);
        let timeout = self.io_timeout;
        thread::spawn(move || {
            let _ = write_pipe_bounded(fd, &data, timeout);
        });
    }

    fn handle_source_cancelled(&mut self, source: Source) {
        source.destroy();
        if self
            .active_source
            .as_ref()
            .is_some_and(|a| a.source.id() == source.id())
        {
            self.active_source = None;
        }
    }
}

fn run_worker(
    conn: Connection,
    mut event_queue: EventQueue<WaylandState>,
    mut state: WaylandState,
    cmd_rx: Receiver<Command>,
    wake_rx: OwnedFd,
) {
    let mut wake_rx = File::from(wake_rx);
    loop {
        if event_queue.dispatch_pending(&mut state).is_err() {
            break;
        }
        let _ = conn.flush();

        let Some(guard) = event_queue.prepare_read() else {
            continue;
        };
        let (wayland_ready, wake_ready) = {
            let wayland_fd = guard.connection_fd();
            let mut fds = [
                PollFd::new(&wayland_fd, PollFlags::IN),
                PollFd::new(&wake_rx, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) => (!fds[0].revents().is_empty(), !fds[1].revents().is_empty()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => break,
            }
        };
        if wayland_ready {
            if guard.read().is_err() {
                break;
            }
            if event_queue.dispatch_pending(&mut state).is_err() {
                break;
            }
        } else {
            drop(guard);
        }

        if wake_ready {
            drain_file(&mut wake_rx);
            if !handle_commands(&conn, &mut event_queue, &mut state, &cmd_rx) {
                break;
            }
        }
    }
}

fn handle_commands(
    conn: &Connection,
    event_queue: &mut EventQueue<WaylandState>,
    state: &mut WaylandState,
    cmd_rx: &Receiver<Command>,
) -> bool {
    let qh = event_queue.handle();
    loop {
        match cmd_rx.try_recv() {
            Ok(Command::Watch { tx, ready }) => {
                state.events_tx = Some(tx);
                let _ = ready.send(());
            }
            Ok(Command::Write { mime, data, reply }) => {
                let res = set_wayland_selection(conn, state, &qh, mime, data);
                let _ = reply.send(res);
            }
            Ok(Command::Paste { reply }) => {
                let res = send_virtual_ctrl_v(event_queue, state, &qh);
                let _ = reply.send(res);
            }
            Err(mpsc::TryRecvError::Empty) => return true,
            Err(mpsc::TryRecvError::Disconnected) => return state.events_tx.is_some(),
        }
    }
}

fn set_wayland_selection(
    conn: &Connection,
    state: &mut WaylandState,
    qh: &QueueHandle<WaylandState>,
    mime: String,
    data: Vec<u8>,
) -> Result<()> {
    let manager = state
        .manager
        .as_ref()
        .ok_or("Wayland data-control manager not initialized")?;
    let device = state
        .device
        .as_ref()
        .ok_or("Wayland data-control device not initialized")?;
    if let Some(prev) = state.active_source.take() {
        prev.source.destroy();
    }
    let source = manager.create_data_source(qh);
    if mime == TEXT_MIME {
        for &offered in TEXT_OFFERED {
            source.offer(offered);
        }
    } else {
        source.offer(&mime);
    }
    device.set_selection(Some(&source));
    state.active_source = Some(ActiveSource {
        source,
        mime,
        data: Arc::from(data),
    });
    conn.flush()?;
    Ok(())
}

fn send_virtual_ctrl_v(
    event_queue: &mut EventQueue<WaylandState>,
    state: &mut WaylandState,
    qh: &QueueHandle<WaylandState>,
) -> Result<bool> {
    let Some(vk_manager) = &state.vk_manager else {
        return Ok(false);
    };
    let seat = state.seat.as_ref().ok_or("Wayland seat missing")?;
    let mut keymap_file = File::from(memfd_create("clipvault-keymap", MemfdFlags::CLOEXEC)?);
    keymap_file.write_all(VIRTUAL_KEYMAP)?;
    let vk = vk_manager.create_virtual_keyboard(seat, qh, ());
    vk.keymap(
        KEYMAP_FORMAT_XKB_V1,
        keymap_file.as_fd(),
        VIRTUAL_KEYMAP.len() as u32,
    );
    vk.key(1, KEY_LEFTCTRL, KEY_PRESSED);
    vk.modifiers(MOD_CONTROL, 0, 0, 0);
    vk.key(2, KEY_V, KEY_PRESSED);
    vk.key(3, KEY_V, KEY_RELEASED);
    vk.key(4, KEY_LEFTCTRL, KEY_RELEASED);
    vk.modifiers(0, 0, 0, 0);
    vk.destroy();
    event_queue.roundtrip(state)?;
    Ok(true)
}

impl Dispatch<WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind::<WlSeat, _, _>(name, 1, qh, ()));
            }
            "ext_data_control_manager_v1" if state.ext_manager.is_none() => {
                state.ext_manager =
                    Some(registry.bind::<ExtDataControlManagerV1, _, _>(name, 1, qh, ()));
            }
            "zwlr_data_control_manager_v1" if state.zwlr_manager.is_none() => {
                state.zwlr_manager = Some(registry.bind::<ZwlrDataControlManagerV1, _, _>(
                    name,
                    version.min(2),
                    qh,
                    (),
                ));
            }
            "zwp_virtual_keyboard_manager_v1" if state.vk_manager.is_none() => {
                state.vk_manager =
                    Some(registry.bind::<ZwpVirtualKeyboardManagerV1, _, _>(name, 1, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: <ExtDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: <ZwpVirtualKeyboardV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ExtDeviceEvent,
        _data: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ExtDeviceEvent::DataOffer { id } => state.record_data_offer(Offer::Ext(id)),
            ExtDeviceEvent::Selection { id } => state.handle_selection(conn, id.map(Offer::Ext)),
            ExtDeviceEvent::PrimarySelection { id } => state.discard_offer(id.map(Offer::Ext)),
            ExtDeviceEvent::Finished => {}
            _ => {}
        }
    }

    event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        EXT_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: ZwlrDeviceEvent,
        _data: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ZwlrDeviceEvent::DataOffer { id } => state.record_data_offer(Offer::Zwlr(id)),
            ZwlrDeviceEvent::Selection { id } => state.handle_selection(conn, id.map(Offer::Zwlr)),
            ZwlrDeviceEvent::PrimarySelection { id } => state.discard_offer(id.map(Offer::Zwlr)),
            ZwlrDeviceEvent::Finished => {}
            _ => {}
        }
    }

    event_created_child!(WaylandState, ZwlrDataControlDeviceV1, [
        ZWLR_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ExtOfferEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ExtOfferEvent::Offer { mime_type } = event {
            state.record_offer_mime(&offer.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: ZwlrOfferEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ZwlrOfferEvent::Offer { mime_type } = event {
            state.record_offer_mime(&offer.id(), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ExtSourceEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ExtSourceEvent::Send { mime_type, fd } => {
                state.handle_source_send(&source.id(), &mime_type, fd);
            }
            ExtSourceEvent::Cancelled => state.handle_source_cancelled(Source::Ext(source.clone())),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: ZwlrSourceEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ZwlrSourceEvent::Send { mime_type, fd } => {
                state.handle_source_send(&source.id(), &mime_type, fd);
            }
            ZwlrSourceEvent::Cancelled => {
                state.handle_source_cancelled(Source::Zwlr(source.clone()));
            }
            _ => {}
        }
    }
}

/// KDE Plasma Wayland paste-back via `org.freedesktop.portal.RemoteDesktop`.
struct PortalPaster {
    conn: DbusConnection,
    bus_name: Option<&'static str>,
    session: Option<OwnedObjectPath>,
    token_counter: u64,
    timeout: Duration,
}

impl PortalPaster {
    fn connect(timeout: Duration) -> Result<Self> {
        let conn = DbusConnection::session()?;
        Ok(Self::with_connection(conn, Some(PORTAL_BUS), timeout))
    }

    fn with_connection(
        conn: DbusConnection,
        bus_name: Option<&'static str>,
        timeout: Duration,
    ) -> Self {
        Self {
            conn,
            bus_name,
            session: None,
            token_counter: 0,
            timeout,
        }
    }

    fn paste(&mut self) -> Result<()> {
        if self.session.is_none() {
            self.session = Some(self.open_session()?);
        }
        let session = self.session.as_ref().unwrap();
        let empty: HashMap<&str, Value<'_>> = HashMap::new();
        for (keysym, state) in [
            (XK_CONTROL_L, KEY_PRESSED),
            (XK_V, KEY_PRESSED),
            (XK_V, KEY_RELEASED),
            (XK_CONTROL_L, KEY_RELEASED),
        ] {
            self.conn.call_method(
                self.bus_name,
                PORTAL_PATH,
                Some(PORTAL_IFACE),
                "NotifyKeyboardKeysym",
                &(session.as_ref(), &empty, keysym, state),
            )?;
        }
        Ok(())
    }

    fn open_session(&mut self) -> Result<OwnedObjectPath> {
        if self.bus_name.is_some() {
            let rule = format!("type='signal',interface='{REQUEST_IFACE}',member='Response'");
            let _ = self.conn.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "AddMatch",
                &(&rule,),
            );
        }
        let mut messages = MessageIterator::from(&self.conn);
        let (resp_tx, resp_rx) = mpsc::channel();
        let (path_tx, path_rx) = mpsc::channel::<OwnedObjectPath>();
        thread::spawn(move || {
            while let Ok(expected_path) = path_rx.recv() {
                for msg in &mut messages {
                    let Ok(msg) = msg else { return };
                    let hdr = msg.header();
                    if hdr.message_type() == MessageType::Signal
                        && hdr.interface().map(|i| i.as_str()) == Some(REQUEST_IFACE)
                        && hdr.member().map(|m| m.as_str()) == Some("Response")
                        && hdr.path().map(|p| p.as_str()) == Some(expected_path.as_str())
                    {
                        let parsed = msg
                            .body()
                            .deserialize::<(u32, HashMap<String, OwnedValue>)>();
                        let _ = resp_tx.send(parsed);
                        break;
                    }
                }
            }
        });

        let token = self.next_token();
        let mut create_opts: HashMap<&str, Value<'_>> = HashMap::new();
        create_opts.insert("handle_token", Value::from(token.as_str()));
        create_opts.insert("session_handle_token", Value::from("clipvault_session"));
        let req_path: OwnedObjectPath = self
            .conn
            .call_method(
                self.bus_name,
                PORTAL_PATH,
                Some(PORTAL_IFACE),
                "CreateSession",
                &(create_opts,),
            )?
            .body()
            .deserialize()?;
        let results = await_portal_step(&path_tx, &resp_rx, req_path, self.timeout)?;
        let session = results
            .get("session_handle")
            .and_then(|v| <&str>::try_from(v).ok())
            .and_then(|s| ObjectPath::try_from(s).ok())
            .map(OwnedObjectPath::from)
            .ok_or("RemoteDesktop portal returned no session_handle")?;

        let token = self.next_token();
        let mut select_opts: HashMap<&str, Value<'_>> = HashMap::new();
        select_opts.insert("handle_token", Value::from(token.as_str()));
        select_opts.insert("types", Value::from(1u32));
        let req_path: OwnedObjectPath = self
            .conn
            .call_method(
                self.bus_name,
                PORTAL_PATH,
                Some(PORTAL_IFACE),
                "SelectDevices",
                &(session.as_ref(), select_opts),
            )?
            .body()
            .deserialize()?;
        await_portal_step(&path_tx, &resp_rx, req_path, self.timeout)?;

        let token = self.next_token();
        let mut start_opts: HashMap<&str, Value<'_>> = HashMap::new();
        start_opts.insert("handle_token", Value::from(token.as_str()));
        let req_path: OwnedObjectPath = self
            .conn
            .call_method(
                self.bus_name,
                PORTAL_PATH,
                Some(PORTAL_IFACE),
                "Start",
                &(session.as_ref(), "", start_opts),
            )?
            .body()
            .deserialize()?;
        await_portal_step(&path_tx, &resp_rx, req_path, self.timeout)?;

        Ok(session)
    }

    fn next_token(&mut self) -> String {
        self.token_counter += 1;
        format!("clipvault_{}", self.token_counter)
    }
}

fn await_portal_step(
    path_tx: &Sender<OwnedObjectPath>,
    resp_rx: &Receiver<zbus::Result<(u32, HashMap<String, OwnedValue>)>>,
    req_path: OwnedObjectPath,
    timeout: Duration,
) -> Result<HashMap<String, OwnedValue>> {
    path_tx
        .send(req_path)
        .map_err(|_| "RemoteDesktop portal listener stopped")?;
    let (code, results) = resp_rx
        .recv_timeout(timeout)
        .map_err(|_| "RemoteDesktop portal timed out")??;
    if code != 0 {
        return Err(format!("RemoteDesktop portal request denied (code {code})").into());
    }
    Ok(results)
}

fn set_nonblocking(fd: impl AsFd) -> std::result::Result<(), rustix::io::Errno> {
    let flags = fcntl_getfl(&fd)?;
    fcntl_setfl(&fd, flags | OFlags::NONBLOCK)
}

fn drain_file(file: &mut File) {
    let mut buf = [0u8; 64];
    while file.read(&mut buf).is_ok_and(|n| n > 0) {}
}

/// Reads up to `max_bytes` from a non-blocking pipe before `timeout` expires.
/// Returns `Ok(None)` if the sender exceeds `max_bytes`.
fn read_pipe_bounded(fd: OwnedFd, max_bytes: usize, timeout: Duration) -> Result<Option<Vec<u8>>> {
    let mut file = File::from(fd);
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return Ok(Some(out)),
            Ok(n) => {
                if out.len() + n > max_bytes {
                    eprintln!("clipvault: skipped format larger than {max_bytes} bytes");
                    return Ok(None);
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                wait_fd_ready(file.as_fd(), PollFlags::IN, deadline)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn write_pipe_bounded(fd: OwnedFd, mut data: &[u8], timeout: Duration) -> Result<()> {
    set_nonblocking(&fd)?;
    let mut file = File::from(fd);
    let deadline = Instant::now() + timeout;
    while !data.is_empty() {
        match file.write(data) {
            Ok(0) => return Err("pipe closed before write finished".into()),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                wait_fd_ready(file.as_fd(), PollFlags::OUT, deadline)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn wait_fd_ready(fd: BorrowedFd<'_>, flags: PollFlags, deadline: Instant) -> Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or("clipboard transfer timed out")?;
    let ts = Timespec {
        tv_sec: remaining.as_secs() as i64,
        tv_nsec: i64::from(remaining.subsec_nanos()),
    };
    let mut fds = [PollFd::new(&fd, flags)];
    let ready = poll(&mut fds, Some(&ts))?;
    if ready == 0 {
        return Err("clipboard transfer timed out".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::process::{Command as ProcessCommand, Stdio};

    use wayland_protocols::ext::data_control::v1::server::{
        ext_data_control_device_v1::{
            ExtDataControlDeviceV1 as ServerExtDevice, Request as ExtDeviceReq,
        },
        ext_data_control_manager_v1::{
            ExtDataControlManagerV1 as ServerExtManager, Request as ExtManagerReq,
        },
        ext_data_control_offer_v1::{
            ExtDataControlOfferV1 as ServerExtOffer, Request as ExtOfferReq,
        },
        ext_data_control_source_v1::{
            ExtDataControlSourceV1 as ServerExtSource, Request as ExtSourceReq,
        },
    };
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
        zwp_virtual_keyboard_manager_v1::{
            Request as VkManagerReq, ZwpVirtualKeyboardManagerV1 as ServerVkManager,
        },
        zwp_virtual_keyboard_v1::{Request as VkReq, ZwpVirtualKeyboardV1 as ServerVk},
    };
    use wayland_protocols_wlr::data_control::v1::server::{
        zwlr_data_control_device_v1::{
            Request as ZwlrDeviceReq, ZwlrDataControlDeviceV1 as ServerZwlrDevice,
        },
        zwlr_data_control_manager_v1::{
            Request as ZwlrManagerReq, ZwlrDataControlManagerV1 as ServerZwlrManager,
        },
        zwlr_data_control_offer_v1::{
            Request as ZwlrOfferReq, ZwlrDataControlOfferV1 as ServerZwlrOffer,
        },
        zwlr_data_control_source_v1::{
            Request as ZwlrSourceReq, ZwlrDataControlSourceV1 as ServerZwlrSource,
        },
    };
    use wayland_server::backend::ObjectId as ServerObjectId;
    use wayland_server::protocol::wl_seat::WlSeat as ServerSeat;
    use wayland_server::{
        Client, DataInit, Dispatch as ServerDispatch, Display as ServerDisplay, DisplayHandle,
        GlobalDispatch, New, Resource,
    };

    use super::*;
    use crate::SENSITIVE_HINT;

    const TEST_TIMEOUT: Duration = Duration::from_millis(300);

    #[derive(Clone, Copy)]
    struct CompositorConfig {
        ext: bool,
        zwlr: bool,
        virtual_keyboard: bool,
    }

    #[derive(Clone)]
    enum OfferBehavior {
        Bytes(Vec<u8>),
        LeaveOpen,
    }

    enum ServerCommand {
        AnnounceExt(Vec<(String, OfferBehavior)>),
        AnnounceZwlr(Vec<(String, OfferBehavior)>),
        ReadClientSelection {
            mime: String,
            reply: Sender<Vec<u8>>,
        },
        TakeKeyEvents(Sender<Vec<(u32, u32)>>),
    }

    struct ServerState {
        ext_devices: Vec<ServerExtDevice>,
        zwlr_devices: Vec<ServerZwlrDevice>,
        offer_payloads: HashMap<ServerObjectId, HashMap<String, OfferBehavior>>,
        held_pipes: Vec<File>,
        ext_source: Option<ServerExtSource>,
        zwlr_source: Option<ServerZwlrSource>,
        key_events: Vec<(u32, u32)>,
    }

    struct TestCompositor {
        cmd_tx: Sender<ServerCommand>,
        wake_tx: File,
    }

    impl TestCompositor {
        fn start(config: CompositorConfig) -> (Self, Connection) {
            let (server_stream, client_stream) = UnixStream::pair().unwrap();
            let display: ServerDisplay<ServerState> = ServerDisplay::new().unwrap();
            let dh = display.handle();
            dh.create_global::<ServerState, ServerSeat, ()>(1, ());
            if config.ext {
                dh.create_global::<ServerState, ServerExtManager, ()>(1, ());
            }
            if config.zwlr {
                dh.create_global::<ServerState, ServerZwlrManager, ()>(2, ());
            }
            if config.virtual_keyboard {
                dh.create_global::<ServerState, ServerVkManager, ()>(1, ());
            }
            display
                .handle()
                .insert_client(server_stream, Arc::new(()))
                .unwrap();

            let (wake_rx, wake_tx) = pipe_with(PipeFlags::CLOEXEC).unwrap();
            set_nonblocking(&wake_rx).unwrap();
            let (cmd_tx, cmd_rx) = mpsc::channel();
            thread::spawn(move || run_server(display, cmd_rx, wake_rx));

            let conn = Connection::from_socket(client_stream).unwrap();
            (
                Self {
                    cmd_tx,
                    wake_tx: File::from(wake_tx),
                },
                conn,
            )
        }

        fn send(&mut self, cmd: ServerCommand) {
            self.cmd_tx.send(cmd).unwrap();
            let _ = self.wake_tx.write(&[1]);
        }

        fn offer_ext(&mut self, formats: &[(&str, &[u8])]) {
            let list = formats
                .iter()
                .map(|(m, d)| ((*m).to_owned(), OfferBehavior::Bytes(d.to_vec())))
                .collect();
            self.send(ServerCommand::AnnounceExt(list));
        }

        fn offer_zwlr(&mut self, formats: &[(&str, &[u8])]) {
            let list = formats
                .iter()
                .map(|(m, d)| ((*m).to_owned(), OfferBehavior::Bytes(d.to_vec())))
                .collect();
            self.send(ServerCommand::AnnounceZwlr(list));
        }

        fn read_selection(&mut self, mime: &str) -> Vec<u8> {
            let (tx, rx) = mpsc::channel();
            self.send(ServerCommand::ReadClientSelection {
                mime: mime.to_owned(),
                reply: tx,
            });
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        }

        fn key_events(&mut self) -> Vec<(u32, u32)> {
            let (tx, rx) = mpsc::channel();
            self.send(ServerCommand::TakeKeyEvents(tx));
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        }
    }

    fn run_server(
        mut display: ServerDisplay<ServerState>,
        cmd_rx: Receiver<ServerCommand>,
        wake_rx: OwnedFd,
    ) {
        let mut wake_rx = File::from(wake_rx);
        let mut state = ServerState {
            ext_devices: Vec::new(),
            zwlr_devices: Vec::new(),
            offer_payloads: HashMap::new(),
            held_pipes: Vec::new(),
            ext_source: None,
            zwlr_source: None,
            key_events: Vec::new(),
        };
        loop {
            let _ = display.flush_clients();
            let (wayland_ready, wake_ready) = {
                let poll_fd = display.backend().poll_fd();
                let mut fds = [
                    PollFd::new(&poll_fd, PollFlags::IN),
                    PollFd::new(&wake_rx, PollFlags::IN),
                ];
                if poll(&mut fds, None).is_err() {
                    break;
                }
                (!fds[0].revents().is_empty(), !fds[1].revents().is_empty())
            };
            if wayland_ready && display.dispatch_clients(&mut state).is_err() {
                break;
            }
            if wake_ready {
                drain_file(&mut wake_rx);
                while let Ok(cmd) = cmd_rx.try_recv() {
                    handle_server_cmd(&mut display, &mut state, cmd);
                }
            }
        }
    }

    fn handle_server_cmd(
        display: &mut ServerDisplay<ServerState>,
        state: &mut ServerState,
        cmd: ServerCommand,
    ) {
        let dh = display.handle();
        match cmd {
            ServerCommand::AnnounceExt(formats) => {
                let Some(device) = state.ext_devices.last().cloned() else {
                    return;
                };
                let client = device.client().unwrap();
                let offer: ServerExtOffer = client
                    .create_resource::<ServerExtOffer, (), ServerState>(&dh, 1, ())
                    .unwrap();
                device.data_offer(&offer);
                let mut map = HashMap::new();
                for (mime, behavior) in formats {
                    offer.offer(mime.clone());
                    map.insert(mime, behavior);
                }
                state.offer_payloads.insert(offer.id(), map);
                device.selection(Some(&offer));
            }
            ServerCommand::AnnounceZwlr(formats) => {
                let Some(device) = state.zwlr_devices.last().cloned() else {
                    return;
                };
                let client = device.client().unwrap();
                let offer: ServerZwlrOffer = client
                    .create_resource::<ServerZwlrOffer, (), ServerState>(&dh, 1, ())
                    .unwrap();
                device.data_offer(&offer);
                let mut map = HashMap::new();
                for (mime, behavior) in formats {
                    offer.offer(mime.clone());
                    map.insert(mime, behavior);
                }
                state.offer_payloads.insert(offer.id(), map);
                device.selection(Some(&offer));
            }
            ServerCommand::ReadClientSelection { mime, reply } => {
                let (read_fd, write_fd) = pipe_with(PipeFlags::CLOEXEC).unwrap();
                if let Some(source) = &state.ext_source {
                    source.send(mime, write_fd.as_fd());
                } else if let Some(source) = &state.zwlr_source {
                    source.send(mime, write_fd.as_fd());
                }
                drop(write_fd);
                let _ = display.flush_clients();
                thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = File::from(read_fd).read_to_end(&mut buf);
                    let _ = reply.send(buf);
                });
            }
            ServerCommand::TakeKeyEvents(reply) => {
                let _ = reply.send(std::mem::take(&mut state.key_events));
            }
        }
    }

    impl GlobalDispatch<ServerSeat, ()> for ServerState {
        fn bind(
            _state: &mut Self,
            _dh: &DisplayHandle,
            _client: &Client,
            resource: New<ServerSeat>,
            _data: &(),
            data_init: &mut DataInit<'_, Self>,
        ) {
            data_init.init(resource, ());
        }
    }

    impl ServerDispatch<ServerSeat, ()> for ServerState {
        fn request(
            _state: &mut Self,
            _client: &Client,
            _resource: &ServerSeat,
            _request: <ServerSeat as Resource>::Request,
            _data: &(),
            _dh: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
        }
    }

    impl GlobalDispatch<ServerExtManager, ()> for ServerState {
        fn bind(
            _state: &mut Self,
            _dh: &DisplayHandle,
            _client: &Client,
            resource: New<ServerExtManager>,
            _data: &(),
            data_init: &mut DataInit<'_, Self>,
        ) {
            data_init.init(resource, ());
        }
    }

    impl ServerDispatch<ServerExtManager, ()> for ServerState {
        fn request(
            state: &mut Self,
            _client: &Client,
            _resource: &ServerExtManager,
            request: ExtManagerReq,
            _data: &(),
            _dh: &DisplayHandle,
            data_init: &mut DataInit<'_, Self>,
        ) {
            match request {
                ExtManagerReq::GetDataDevice { id, .. } => {
                    let dev = data_init.init(id, ());
                    dev.selection(None);
                    state.ext_devices.push(dev);
                }
                ExtManagerReq::CreateDataSource { id } => {
                    data_init.init(id, ());
                }
                _ => {}
            }
        }
    }

    impl ServerDispatch<ServerExtDevice, ()> for ServerState {
        fn request(
            state: &mut Self,
            _client: &Client,
            _resource: &ServerExtDevice,
            request: ExtDeviceReq,
            _data: &(),
            _dh: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
            if let ExtDeviceReq::SetSelection { source } = request {
                state.ext_source = source;
            }
        }
    }

    impl ServerDispatch<ServerExtOffer, ()> for ServerState {
        fn request(
            state: &mut Self,
            _client: &Client,
            offer: &ServerExtOffer,
            request: ExtOfferReq,
            _data: &(),
            _dh: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
            if let ExtOfferReq::Receive { mime_type, fd } = request
                && let Some(behavior) = state
                    .offer_payloads
                    .get(&offer.id())
                    .and_then(|m| m.get(&mime_type))
            {
                match behavior {
                    OfferBehavior::Bytes(bytes) => {
                        let data = bytes.clone();
                        thread::spawn(move || {
                            let _ = File::from(fd).write_all(&data);
                        });
                    }
                    OfferBehavior::LeaveOpen => state.held_pipes.push(File::from(fd)),
                }
            }
        }
    }

    impl ServerDispatch<ServerExtSource, ()> for ServerState {
        fn request(
            _state: &mut Self,
            _client: &Client,
            _resource: &ServerExtSource,
            _request: ExtSourceReq,
            _data: &(),
            _dh: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
        }
    }

    impl GlobalDispatch<ServerZwlrManager, ()> for ServerState {
        fn bind(
            _state: &mut Self,
            _dh: &DisplayHandle,
            _client: &Client,
            resource: New<ServerZwlrManager>,
            _data: &(),
            data_init: &mut DataInit<'_, Self>,
        ) {
            data_init.init(resource, ());
        }
    }

    impl ServerDispatch<ServerZwlrManager, ()> for ServerState {
        fn request(
            state: &mut Self,
            _client: &Client,
            _resource: &ServerZwlrManager,
            request: ZwlrManagerReq,
            _data: &(),
            _dh: &DisplayHandle,
            data_init: &mut DataInit<'_, Self>,
        ) {
            match request {
                ZwlrManagerReq::GetDataDevice { id, .. } => {
                    let dev = data_init.init(id, ());
                    dev.selection(None);
                    state.zwlr_devices.push(dev);
                }
                ZwlrManagerReq::CreateDataSource { id } => {
                    data_init.init(id, ());
                }
                _ => {}
            }
        }
    }

    impl ServerDispatch<ServerZwlrDevice, ()> for ServerState {
        fn request(
            state: &mut Self,
            _client: &Client,
            _resource: &ServerZwlrDevice,
            request: ZwlrDeviceReq,
            _data: &(),
            _dh: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
            if let ZwlrDeviceReq::SetSelection { source } = request {
                state.zwlr_source = source;
            }
        }
    }

    impl ServerDispatch<ServerZwlrOffer, ()> for ServerState {
        fn request(
            state: &mut Self,
            _client: &Client,
            offer: &ServerZwlrOffer,
            request: ZwlrOfferReq,
            _data: &(),
            _dh: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
            if let ZwlrOfferReq::Receive { mime_type, fd } = request
                && let Some(behavior) = state
                    .offer_payloads
                    .get(&offer.id())
                    .and_then(|m| m.get(&mime_type))
            {
                match behavior {
                    OfferBehavior::Bytes(bytes) => {
                        let data = bytes.clone();
                        thread::spawn(move || {
                            let _ = File::from(fd).write_all(&data);
                        });
                    }
                    OfferBehavior::LeaveOpen => state.held_pipes.push(File::from(fd)),
                }
            }
        }
    }

    impl ServerDispatch<ServerZwlrSource, ()> for ServerState {
        fn request(
            _state: &mut Self,
            _client: &Client,
            _resource: &ServerZwlrSource,
            _request: ZwlrSourceReq,
            _data: &(),
            _dh: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
        }
    }

    impl GlobalDispatch<ServerVkManager, ()> for ServerState {
        fn bind(
            _state: &mut Self,
            _dh: &DisplayHandle,
            _client: &Client,
            resource: New<ServerVkManager>,
            _data: &(),
            data_init: &mut DataInit<'_, Self>,
        ) {
            data_init.init(resource, ());
        }
    }

    impl ServerDispatch<ServerVkManager, ()> for ServerState {
        fn request(
            _state: &mut Self,
            _client: &Client,
            _resource: &ServerVkManager,
            request: VkManagerReq,
            _data: &(),
            _dh: &DisplayHandle,
            data_init: &mut DataInit<'_, Self>,
        ) {
            if let VkManagerReq::CreateVirtualKeyboard { id, .. } = request {
                data_init.init(id, ());
            }
        }
    }

    impl ServerDispatch<ServerVk, ()> for ServerState {
        fn request(
            state: &mut Self,
            _client: &Client,
            _resource: &ServerVk,
            request: VkReq,
            _data: &(),
            _dh: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
            if let VkReq::Key { key, state: s, .. } = request {
                state.key_events.push((key, s));
            }
        }
    }

    fn next_clip(rx: &Receiver<Event>) -> Vec<(String, Vec<u8>)> {
        match rx.recv_timeout(Duration::from_secs(3)).unwrap() {
            Event::Clip(clip) => clip.formats,
            Event::PasteRequested => panic!("expected a Clip event"),
        }
    }

    type PortalOutcome = (usize, Vec<(i32, u32)>);

    fn spawn_mock_portal(response_code: u32) -> (PortalPaster, Receiver<PortalOutcome>) {
        let (client_sock, server_sock) = UnixStream::pair().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let conn = zbus::blocking::connection::Builder::unix_stream(server_sock)
                .server(zbus::Guid::generate())
                .unwrap()
                .p2p()
                .build()
                .unwrap();
            let messages = MessageIterator::from(&conn);
            let session_path =
                OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/1/s").unwrap();
            let mut created_sessions = 0usize;
            let mut recorded_keys = Vec::new();
            for msg in messages {
                let Ok(msg) = msg else { break };
                let hdr = msg.header();
                if hdr.message_type() != MessageType::MethodCall {
                    continue;
                }
                let Some(member) = hdr.member().map(|m| m.as_str()) else {
                    continue;
                };
                match member {
                    "CreateSession" | "SelectDevices" | "Start" => {
                        if member == "CreateSession" {
                            created_sessions += 1;
                        }
                        let req_path = OwnedObjectPath::try_from(format!(
                            "/org/freedesktop/portal/desktop/request/1/{member}"
                        ))
                        .unwrap();
                        conn.reply(&hdr, &(&req_path,)).unwrap();
                        let mut results: HashMap<&str, Value<'_>> = HashMap::new();
                        if member == "CreateSession" {
                            results.insert("session_handle", Value::from(session_path.as_str()));
                        }
                        conn.emit_signal(
                            None::<&str>,
                            &req_path,
                            REQUEST_IFACE,
                            "Response",
                            &(response_code, results),
                        )
                        .unwrap();
                        if response_code != 0 {
                            break;
                        }
                    }
                    "NotifyKeyboardKeysym" => {
                        let (_, _, keysym, state): (
                            OwnedObjectPath,
                            HashMap<String, OwnedValue>,
                            i32,
                            u32,
                        ) = msg.body().deserialize().unwrap();
                        recorded_keys.push((keysym, state));
                        conn.reply(&hdr, &()).unwrap();
                        if recorded_keys.len() == 8 {
                            let _ = done_tx.send((created_sessions, recorded_keys));
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
        let client_conn = zbus::blocking::connection::Builder::unix_stream(client_sock)
            .p2p()
            .build()
            .unwrap();
        (
            PortalPaster::with_connection(client_conn, None, TEST_TIMEOUT),
            done_rx,
        )
    }

    fn sway_round_trip(payload: &[u8]) -> Vec<(String, Vec<u8>)> {
        let sway_ok = ProcessCommand::new("sway")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !sway_ok {
            return vec![(TEXT_MIME.to_owned(), payload.to_vec())];
        }
        let runtime_dir = tempfile::TempDir::new().unwrap();
        let cfg_path = runtime_dir.path().join("sway.cfg");
        std::fs::write(&cfg_path, "").unwrap();
        let mut sway = ProcessCommand::new("sway")
            .arg("-c")
            .arg(&cfg_path)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_RENDERER", "pixman")
            .env("XDG_RUNTIME_DIR", runtime_dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let socket = (0..100)
            .find_map(|_| {
                thread::sleep(Duration::from_millis(50));
                std::fs::read_dir(runtime_dir.path())
                    .ok()?
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .find(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
                    })
            })
            .expect("headless sway did not create a Wayland socket");

        let watcher_conn = Connection::from_socket(UnixStream::connect(&socket).unwrap()).unwrap();
        let writer_conn = Connection::from_socket(UnixStream::connect(&socket).unwrap()).unwrap();
        let mut watcher = DataControl::connect_with_timeout(watcher_conn, IO_TIMEOUT).unwrap();
        let mut writer = DataControl::connect_with_timeout(writer_conn, IO_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        watcher.watch(tx).unwrap();
        writer.write(TEXT_MIME, payload).unwrap();
        writer.paste().unwrap();
        let formats = next_clip(&rx);
        let _ = sway.kill();
        let _ = sway.wait();
        formats
    }

    #[test]
    fn captures_text_and_png_over_ext_data_control() {
        let (mut compositor, conn) = TestCompositor::start(CompositorConfig {
            ext: true,
            zwlr: false,
            virtual_keyboard: false,
        });
        let mut backend = DataControl::connect_with_timeout(conn, TEST_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();

        compositor.offer_ext(&[("UTF8_STRING", b"wayland text"), (PNG_MIME, b"\x89PNG")]);

        assert_eq!(
            next_clip(&rx),
            vec![
                (TEXT_MIME.to_owned(), b"wayland text".to_vec()),
                (PNG_MIME.to_owned(), b"\x89PNG".to_vec()),
            ]
        );
    }

    #[test]
    fn falls_back_to_wlr_data_control_when_ext_is_absent() {
        let (mut compositor, conn) = TestCompositor::start(CompositorConfig {
            ext: false,
            zwlr: true,
            virtual_keyboard: false,
        });
        let mut backend = DataControl::connect_with_timeout(conn, TEST_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();

        compositor.offer_zwlr(&[(TEXT_MIME, b"from sway")]);

        assert_eq!(
            next_clip(&rx),
            vec![(TEXT_MIME.to_owned(), b"from sway".to_vec())]
        );
    }

    #[test]
    fn prefers_ext_data_control_when_both_are_advertised() {
        let (mut compositor, conn) = TestCompositor::start(CompositorConfig {
            ext: true,
            zwlr: true,
            virtual_keyboard: false,
        });
        let mut backend = DataControl::connect_with_timeout(conn, TEST_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();

        compositor.offer_ext(&[(TEXT_MIME, b"preferred ext")]);

        assert_eq!(
            next_clip(&rx),
            vec![(TEXT_MIME.to_owned(), b"preferred ext".to_vec())]
        );
    }

    #[test]
    fn reports_unsupported_compositor_without_data_control() {
        let (_compositor, conn) = TestCompositor::start(CompositorConfig {
            ext: false,
            zwlr: false,
            virtual_keyboard: false,
        });

        let result = DataControl::connect_with_timeout(conn, TEST_TIMEOUT);

        assert!(matches!(result, Err(ConnectError::UnsupportedCompositor)));
    }

    #[test]
    fn skips_copies_marked_with_the_password_manager_hint() {
        let (mut compositor, conn) = TestCompositor::start(CompositorConfig {
            ext: true,
            zwlr: false,
            virtual_keyboard: false,
        });
        let mut backend = DataControl::connect_with_timeout(conn, TEST_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();

        compositor.offer_ext(&[(TEXT_MIME, b"top-secret"), (SENSITIVE_HINT, b"secret")]);
        compositor.offer_ext(&[(TEXT_MIME, b"public")]);

        assert_eq!(
            next_clip(&rx),
            vec![(TEXT_MIME.to_owned(), b"public".to_vec())]
        );
    }

    #[test]
    fn skips_a_format_larger_than_the_size_cap() {
        let (mut compositor, conn) = TestCompositor::start(CompositorConfig {
            ext: true,
            zwlr: false,
            virtual_keyboard: false,
        });
        let mut backend = DataControl::connect_with_timeout(conn, TEST_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();
        let oversized = vec![b'x'; MAX_FORMAT_BYTES + 1];

        compositor.offer_ext(&[(TEXT_MIME, b"kept"), (PNG_MIME, &oversized)]);

        assert_eq!(
            next_clip(&rx),
            vec![(TEXT_MIME.to_owned(), b"kept".to_vec())]
        );
    }

    #[test]
    fn recovers_when_a_selection_owner_never_closes_its_pipe() {
        let (mut compositor, conn) = TestCompositor::start(CompositorConfig {
            ext: true,
            zwlr: false,
            virtual_keyboard: false,
        });
        let mut backend = DataControl::connect_with_timeout(conn, TEST_TIMEOUT).unwrap();
        let (tx, rx) = mpsc::channel();
        backend.watch(tx).unwrap();

        compositor.send(ServerCommand::AnnounceExt(vec![(
            TEXT_MIME.to_owned(),
            OfferBehavior::LeaveOpen,
        )]));
        compositor.offer_ext(&[(TEXT_MIME, b"next copy arrives")]);

        assert_eq!(
            next_clip(&rx),
            vec![(TEXT_MIME.to_owned(), b"next copy arrives".to_vec())]
        );
    }

    #[test]
    fn write_places_data_on_the_clipboard_and_answers_send() {
        let (mut compositor, conn) = TestCompositor::start(CompositorConfig {
            ext: true,
            zwlr: false,
            virtual_keyboard: false,
        });
        let mut backend = DataControl::connect_with_timeout(conn, TEST_TIMEOUT).unwrap();

        backend.write(TEXT_MIME, b"pasted back").unwrap();

        assert_eq!(compositor.read_selection("UTF8_STRING"), b"pasted back");
    }

    #[test]
    fn paste_sends_ctrl_v_through_the_virtual_keyboard() {
        let (mut compositor, conn) = TestCompositor::start(CompositorConfig {
            ext: true,
            zwlr: false,
            virtual_keyboard: true,
        });
        let mut backend = DataControl::connect_with_timeout(conn, TEST_TIMEOUT).unwrap();

        backend.paste().unwrap();

        assert_eq!(
            compositor.key_events(),
            vec![
                (KEY_LEFTCTRL, KEY_PRESSED),
                (KEY_V, KEY_PRESSED),
                (KEY_V, KEY_RELEASED),
                (KEY_LEFTCTRL, KEY_RELEASED),
            ]
        );
    }

    #[test]
    fn portal_paster_opens_a_session_once_and_sends_ctrl_v_keysyms() {
        let (mut portal, done_rx) = spawn_mock_portal(0);

        portal.paste().unwrap();
        portal.paste().unwrap();

        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            (
                1,
                vec![
                    (XK_CONTROL_L, KEY_PRESSED),
                    (XK_V, KEY_PRESSED),
                    (XK_V, KEY_RELEASED),
                    (XK_CONTROL_L, KEY_RELEASED),
                    (XK_CONTROL_L, KEY_PRESSED),
                    (XK_V, KEY_PRESSED),
                    (XK_V, KEY_RELEASED),
                    (XK_CONTROL_L, KEY_RELEASED),
                ]
            )
        );
    }

    #[test]
    fn portal_paster_reports_an_error_when_the_user_denies_permission() {
        let (mut portal, _done_rx) = spawn_mock_portal(1);

        let result = portal.paste();

        assert!(result.is_err());
    }

    #[test]
    fn headless_sway_round_trips_clipboard_when_installed() {
        let formats = sway_round_trip(b"real sway copy");

        assert_eq!(
            formats,
            vec![(TEXT_MIME.to_owned(), b"real sway copy".to_vec())]
        );
    }
}
