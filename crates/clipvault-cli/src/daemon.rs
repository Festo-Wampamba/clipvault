//! `clipvault run`: records every copy and answers the paste-back shortcut.

use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use clipvault_capture::{ClipboardBackend, Event};
use clipvault_core::{
    Added, Format, Limits, NewClip, Outcome, PNG_MIME, Store, TEXT_MIME, capture,
};

use crate::Result;

/// Pruning scans the history, so it runs after a new clip at most this often.
const PRUNE_INTERVAL_MS: i64 = 60 * 1000;

/// What the daemon remembers between events.
#[derive(Default)]
struct State {
    last_prune: Option<i64>,
    /// The latest copy was not stored (private mode, an excluded app, too large, empty),
    /// so what is on the clipboard now is not the newest clip in history.
    current_skipped: bool,
}

pub fn run(store: &mut Store) -> Result<()> {
    let mut backend = clipvault_capture::detect()?;
    let (tx, events) = mpsc::channel();
    backend.watch(tx)?;
    eprintln!("clipvault: recording the clipboard");
    let mut state = State::default();
    for event in events {
        // One failed copy or paste must not stop recording; report it and carry on.
        if let Err(e) = handle(store, backend.as_mut(), event, now_ms(), &mut state) {
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
            let outcome = capture(store, clip, now)?;
            state.current_skipped = matches!(outcome, Outcome::Skipped(_));
            let is_new = matches!(outcome, Outcome::Stored(Added::Inserted(_)));
            let prune_due = state
                .last_prune
                .is_none_or(|last| now - last >= PRUNE_INTERVAL_MS);
            if is_new && prune_due {
                state.last_prune = Some(now);
                store.prune(&Limits::default(), now)?;
            }
            Ok(())
        }
        Event::PasteRequested => paste_previous(store, backend, state.current_skipped),
    }
}

/// Puts the clip before the current clipboard content back on the clipboard and pastes it,
/// so the shortcut recalls your previous copy until the popup UI arrives. When the current
/// content was skipped it is not in history, so the newest stored clip is the previous one.
fn paste_previous(
    store: &mut Store,
    backend: &mut dyn ClipboardBackend,
    current_skipped: bool,
) -> Result<()> {
    let position = if current_skipped { 0 } else { 1 };
    let Some(previous) = store.recent(position + 1)?.into_iter().nth(position) else {
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

    /// A daemon with a temporary history and a fake clipboard, fed events in order.
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
        }

        fn copy(&mut self, mime: &str, data: &[u8]) {
            self.send(Event::Clip(RawClip {
                formats: vec![(mime.to_owned(), data.to_vec())],
            }));
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

        assert_eq!(
            daemon.backend.written,
            vec![(TEXT_MIME.to_owned(), b"first".to_vec())]
        );
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

        assert_eq!(
            daemon.backend.written,
            vec![(TEXT_MIME.to_owned(), b"newest stored".to_vec())]
        );
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
}
