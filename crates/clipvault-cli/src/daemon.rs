//! `clipvault run`: records every copy, answers the paste-back shortcut and expires old
//! history on a schedule.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clipvault_capture::{ClipboardBackend, Event};
use clipvault_core::{
    Added, ClipId, Format, Limits, NewClip, Outcome, PNG_MIME, Store, TEXT_MIME, capture,
};

use crate::Result;

/// History is pruned at start-up, then at most this often: after copies, and on a timer
/// while idle, so old clips expire even when nothing new is copied.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// What the daemon remembers between events.
#[derive(Default)]
struct State {
    last_prune: Option<i64>,
    /// The clip on the clipboard right now, or `None` when the latest copy was not stored
    /// (private mode, an excluded app, too large, empty). It may since have been deleted.
    current: Option<ClipId>,
}

pub fn run(store: &mut Store) -> Result<()> {
    let mut backend = clipvault_capture::detect()?;
    let (tx, events) = mpsc::channel();
    backend.watch(tx)?;
    eprintln!("clipvault: recording the clipboard");
    // Until a copy arrives, assume the clipboard holds the newest stored clip.
    let mut state = State {
        current: store.recent(1)?.first().map(|clip| clip.id),
        ..State::default()
    };
    loop {
        let event = match events.recv_timeout(PRUNE_INTERVAL) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let now = now_ms();
        // One failed copy, paste or prune must not stop recording; report it and carry on.
        if let Some(event) = event
            && let Err(e) = handle(store, backend.as_mut(), event, now, &mut state)
        {
            eprintln!("clipvault: {e}");
        }
        if let Err(e) = prune_if_due(store, now, &mut state) {
            eprintln!("clipvault: {e}");
        }
    }
    Err("the clipboard backend stopped".into())
}

fn handle(
    store: &mut Store,
    backend: &mut dyn ClipboardBackend,
    event: Event,
    now: i64,
    state: &mut State,
) -> Result<()> {
    match event {
        Event::Clip(raw) => {
            let clip = NewClip {
                formats: raw
                    .formats
                    .into_iter()
                    .map(|(mime, data)| Format { mime, data })
                    .collect(),
                source_app: None,
            };
            state.current = match capture(store, clip, now)? {
                Outcome::Stored(Added::Inserted(id) | Added::Bumped(id)) => Some(id),
                Outcome::Skipped(_) => None,
            };
            Ok(())
        }
        Event::PasteRequested => paste_previous(store, backend, state.current),
    }
}

fn prune_if_due(store: &mut Store, now: i64, state: &mut State) -> Result<()> {
    let interval_ms = i64::try_from(PRUNE_INTERVAL.as_millis()).unwrap_or(i64::MAX);
    if state
        .last_prune
        .is_some_and(|last| now - last < interval_ms)
    {
        return Ok(());
    }
    state.last_prune = Some(now);
    store.prune(&Limits::default(), now)?;
    Ok(())
}

/// Puts the newest clip that is not on the clipboard right now back on it and pastes it,
/// so the shortcut recalls your previous copy until the popup UI arrives. Works whether
/// the current content was skipped, deleted from history, or is the newest clip.
fn paste_previous(
    store: &mut Store,
    backend: &mut dyn ClipboardBackend,
    current: Option<ClipId>,
) -> Result<()> {
    let previous = store
        .recent(2)?
        .into_iter()
        .find(|clip| Some(clip.id) != current);
    let Some(previous) = previous else {
        return Ok(());
    };
    let formats = store.formats(previous.id)?;
    // Backends write one format; an image clip pastes as the image.
    let chosen = [PNG_MIME, TEXT_MIME]
        .into_iter()
        .find_map(|mime| formats.iter().find(|f| f.mime == mime));
    if let Some(format) = chosen {
        backend.write(&format.mime, &format.data)?;
        backend.paste()?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Sender;

    use clipvault_capture::RawClip;
    use tempfile::TempDir;

    use super::*;

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    /// Records what the daemon asks of the clipboard instead of touching a real one.
    #[derive(Default)]
    struct FakeBackend {
        written: Vec<(String, Vec<u8>)>,
        pastes: usize,
    }

    impl ClipboardBackend for FakeBackend {
        fn watch(&mut self, _tx: Sender<Event>) -> clipvault_capture::Result<()> {
            Ok(())
        }

        fn write(&mut self, mime: &str, data: &[u8]) -> clipvault_capture::Result<()> {
            self.written.push((mime.to_owned(), data.to_vec()));
            Ok(())
        }

        fn paste(&mut self) -> clipvault_capture::Result<()> {
            self.pastes += 1;
            Ok(())
        }
    }

    /// A daemon with a temporary history and a fake clipboard, fed events in order the way
    /// `run` does: handle the event, then prune if due.
    struct Daemon {
        _dir: TempDir,
        store: Store,
        backend: FakeBackend,
        state: State,
        now: i64,
    }

    impl Daemon {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let store = Store::open(dir.path()).unwrap();
            Self {
                _dir: dir,
                store,
                backend: FakeBackend::default(),
                state: State::default(),
                now: 0,
            }
        }

        fn send(&mut self, event: Event) {
            self.now += 1;
            handle(
                &mut self.store,
                &mut self.backend,
                event,
                self.now,
                &mut self.state,
            )
            .unwrap();
            prune_if_due(&mut self.store, self.now, &mut self.state).unwrap();
        }

        fn copy(&mut self, mime: &str, data: &[u8]) {
            self.send(Event::Clip(RawClip {
                formats: vec![(mime.to_owned(), data.to_vec())],
            }));
        }

        fn written_text(&self) -> Vec<&[u8]> {
            self.backend
                .written
                .iter()
                .map(|(_, data)| data.as_slice())
                .collect()
        }
    }

    #[test]
    fn a_copy_is_recorded() {
        let mut daemon = Daemon::new();

        daemon.copy(TEXT_MIME, b"hello");

        assert_eq!(daemon.store.recent(1).unwrap()[0].preview, "hello");
    }

    #[test]
    fn the_shortcut_puts_back_the_clip_before_the_current_one() {
        let mut daemon = Daemon::new();
        daemon.copy(TEXT_MIME, b"first");
        daemon.copy(TEXT_MIME, b"second");

        daemon.send(Event::PasteRequested);

        assert_eq!(daemon.written_text(), vec![b"first".as_slice()]);
    }

    #[test]
    fn the_shortcut_presses_paste() {
        let mut daemon = Daemon::new();
        daemon.copy(TEXT_MIME, b"first");
        daemon.copy(TEXT_MIME, b"second");

        daemon.send(Event::PasteRequested);

        assert_eq!(daemon.backend.pastes, 1);
    }

    #[test]
    fn the_shortcut_does_nothing_with_a_single_clip() {
        let mut daemon = Daemon::new();
        daemon.copy(TEXT_MIME, b"only");

        daemon.send(Event::PasteRequested);

        assert!(daemon.backend.written.is_empty());
    }

    #[test]
    fn after_a_skipped_copy_the_shortcut_puts_back_the_newest_stored_clip() {
        let mut daemon = Daemon::new();
        daemon.copy(TEXT_MIME, b"older");
        daemon.copy(TEXT_MIME, b"newest stored");
        daemon.store.set_private_mode(true).unwrap();
        daemon.copy(TEXT_MIME, b"secret, not stored");

        daemon.send(Event::PasteRequested);

        assert_eq!(daemon.written_text(), vec![b"newest stored".as_slice()]);
    }

    #[test]
    fn after_the_current_clip_is_deleted_the_shortcut_puts_back_the_newest_left() {
        let mut daemon = Daemon::new();
        daemon.copy(TEXT_MIME, b"a");
        daemon.copy(TEXT_MIME, b"b");
        daemon.copy(TEXT_MIME, b"c");
        let current = daemon.store.recent(1).unwrap()[0].id;
        daemon.store.delete(current).unwrap();

        daemon.send(Event::PasteRequested);

        assert_eq!(daemon.written_text(), vec![b"b".as_slice()]);
    }

    #[test]
    fn an_image_clip_is_pasted_as_png() {
        let mut daemon = Daemon::new();
        daemon.copy(PNG_MIME, b"png bytes");
        daemon.copy(TEXT_MIME, b"later");

        daemon.send(Event::PasteRequested);

        assert_eq!(daemon.backend.written[0].0, PNG_MIME);
    }

    #[test]
    fn nothing_is_recorded_in_private_mode() {
        let mut daemon = Daemon::new();
        daemon.store.set_private_mode(true).unwrap();

        daemon.copy(TEXT_MIME, b"secret");

        assert!(daemon.store.recent(1).unwrap().is_empty());
    }

    #[test]
    fn old_clips_expire_even_when_nothing_new_is_copied() {
        let mut daemon = Daemon::new();
        daemon.copy(TEXT_MIME, b"from last month");

        prune_if_due(&mut daemon.store, 31 * DAY_MS, &mut daemon.state).unwrap();

        assert!(daemon.store.recent(1).unwrap().is_empty());
    }

    #[test]
    fn pruning_waits_for_the_interval() {
        let mut daemon = Daemon::new();
        daemon.copy(TEXT_MIME, b"kept");
        let last = daemon.state.last_prune.unwrap();

        prune_if_due(&mut daemon.store, last + 1, &mut daemon.state).unwrap();

        assert_eq!(daemon.state.last_prune, Some(last));
    }
}
