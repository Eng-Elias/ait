//! Hot reload for `settings.toml` (Spec R03, user stories 10–11): a
//! `notify` watcher whose background thread re-reads the file through the
//! [`load_from`] seam and pushes `Result<Settings, ConfigError>` events on
//! a standard channel the consumer drains — `ait`'s synchronous poll loop
//! calls [`mpsc::Receiver::try_recv`] once per tick (ADR-0004: no async in
//! `ait`).
//!
//! The watcher subscribes to the settings file's **parent directory**, not
//! the file itself: editors that atomic-save (write temp + rename)
//! replace the inode, and an inotify watch on the file goes silent after
//! the first such save. Directory events are filtered down to the
//! settings path, so a file created (or re-created) after watching starts
//! still surfaces.
//!
//! What an event carries is pure outcome — `Ok(Settings)` on a good save,
//! `Err(ConfigError)` on a bad one, `Ok(defaults)` when the file is
//! deleted. The keep-last-good/new-sessions-only consumer policy is the
//! caller's (Spec R03 defers it to a later ADR); the watcher never panics
//! on a bad save.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{load_from, settings_path, ConfigError, Settings};

/// Quiet window before a batch of filesystem events becomes one reload —
/// Spec R03's "~300 ms" debounce: an editor's multi-write save collapses
/// to a single `Settings` event, and a mid-write read of a truncated file
/// never reaches the consumer.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Subscribe to [`settings_path`] — the hot-reload companion of [`load`].
///
/// Returns a [`ConfigError`] (never panics) when the OS watcher cannot be
/// created or the directory does not exist yet — on a first run without
/// `--init`, `~/.config/ait/` may be absent; the caller surfaces the error
/// and runs without hot reload.
///
/// [`load`]: crate::load
pub fn watch() -> Result<SettingsWatcher, ConfigError> {
    watch_from(settings_path())
}

/// [`watch`] at an explicit path — the seam callers with their own path
/// (tests, `--config` flags) use instead of the environment, mirroring
/// [`load_from`].
pub fn watch_from(path: impl AsRef<Path>) -> Result<SettingsWatcher, ConfigError> {
    let (dir, file) = watched_paths(path.as_ref())?;

    // Raw pump input: filesystem events from `notify`, plus a Shutdown the
    // `SettingsWatcher`'s Drop pushes so the thread exits promptly instead
    // of leaking parked on `recv`.
    let (raw_tx, raw_rx) = mpsc::channel::<RawEvent>();
    let mut watcher = notify::recommended_watcher({
        let raw_tx = raw_tx.clone();
        move |event: notify::Result<Event>| {
            // A dead receiver only means the pump thread has exited.
            let _ = raw_tx.send(RawEvent::Fs(event));
        }
    })
    .map_err(|err| ConfigError::unlocated(format!("cannot watch {}: {err}", dir.display())))?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|err| ConfigError::unlocated(format!("cannot watch {}: {err}", dir.display())))?;

    let (tx, rx) = mpsc::channel();
    let thread = thread::spawn(move || pump(raw_rx, tx, file, watcher));
    Ok(SettingsWatcher {
        receiver: rx,
        shutdown: raw_tx,
        thread: Some(thread),
    })
}

/// The consumer half of [`watch`]: holds the event channel and keeps the
/// OS watcher + pump thread alive. Drop it to stop watching — the thread
/// is joined so no watcher outlives its handle.
pub struct SettingsWatcher {
    receiver: Receiver<Result<Settings, ConfigError>>,
    shutdown: Sender<RawEvent>,
    thread: Option<JoinHandle<()>>,
}

impl SettingsWatcher {
    /// The channel of reload outcomes — drain it with
    /// [`Receiver::try_recv`] once per app-loop tick, or block on
    /// [`Receiver::recv`]/[`Receiver::recv_timeout`] where that fits
    /// better (tests).
    pub fn receiver(&self) -> &Receiver<Result<Settings, ConfigError>> {
        &self.receiver
    }
}

impl std::fmt::Debug for SettingsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsWatcher")
            .field("receiver", &self.receiver)
            .finish_non_exhaustive()
    }
}

impl Drop for SettingsWatcher {
    fn drop(&mut self) {
        // Wake the pump thread — it exits on Shutdown — then join it so
        // the OS watcher is dropped before this returns.
        let _ = self.shutdown.send(RawEvent::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Resolve `path` into `(directory to watch, exact file path to match)`
/// — both absolute so `notify`'s reported paths compare cleanly and a
/// later cwd change cannot redirect the re-read.
fn watched_paths(path: &Path) -> Result<(PathBuf, PathBuf), ConfigError> {
    let file_name = path.file_name().ok_or_else(|| {
        ConfigError::unlocated(format!("{}: not a settings file path", path.display()))
    })?;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        // A bare "settings.toml" means the working directory.
        _ => PathBuf::from("."),
    };
    let dir = std::path::absolute(&dir)
        .map_err(|err| ConfigError::unlocated(format!("cannot watch {}: {err}", dir.display())))?;
    Ok((dir.clone(), dir.join(file_name)))
}

/// Input to the pump thread — a `notify` event or a shutdown signal,
/// multiplexed on one channel so a blocking `recv` wakes for either.
enum RawEvent {
    Fs(notify::Result<Event>),
    Shutdown,
}

/// Whether `event` concerns the watched file: path-filtered (sibling temp
/// files in the same directory do not count) and kind-filtered (accesses
/// are not changes). `Any`/`Other` kinds count — an unrecognized
/// notification errs toward a reload over a missed one.
fn touches_file(event: &Event, file: &Path) -> bool {
    !matches!(event.kind, EventKind::Access(_)) && event.paths.iter().any(|path| path == file)
}

/// The pump thread: own the `notify` watcher (dropping it stops OS
/// delivery) and turn filesystem-event batches into debounced reload
/// outcomes on `tx`. Exits when the consumer signals Shutdown, the send
/// side dies, or the consumer's receiver is gone — never panics on a bad
/// save: `load_from` errors are channel payloads, not exceptional.
fn pump(
    raw_rx: Receiver<RawEvent>,
    tx: Sender<Result<Settings, ConfigError>>,
    file: PathBuf,
    _watcher: RecommendedWatcher,
) {
    loop {
        match raw_rx.recv() {
            Err(_) | Ok(RawEvent::Shutdown) => return,
            // A notify error (e.g. event-queue overflow) may mean dropped
            // events — resync by treating it as a change.
            Ok(RawEvent::Fs(Err(_))) => {}
            Ok(RawEvent::Fs(Ok(event))) if !touches_file(&event, &file) => continue,
            Ok(RawEvent::Fs(Ok(_))) => {}
        }
        // A touching event: drain the burst until the file has been quiet
        // for DEBOUNCE — each *touching* event slides the window, so a
        // multi-write save reloads once, from the settled file. Events
        // the path filter rejects are drained without sliding: churn on
        // sibling files in a busy config dir must not starve the reload.
        let mut deadline = Instant::now() + DEBOUNCE;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match raw_rx.recv_timeout(deadline - now) {
                Ok(RawEvent::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                // A notify error may mean dropped events — resync by
                // sliding the window like a touching event.
                Ok(RawEvent::Fs(Err(_))) => deadline = Instant::now() + DEBOUNCE,
                Ok(RawEvent::Fs(Ok(event))) => {
                    if touches_file(&event, &file) {
                        deadline = Instant::now() + DEBOUNCE;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        if tx.send(load_from(&file)).is_err() {
            // The consumer dropped its receiver — nothing left to do.
            return;
        }
    }
}
