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
//!
//! [`watch_dirs`] is the same machinery over a set of directories
//! (§5.10.2's "watched for changes", Spec R10-T6): each dir subscribed
//! recursively, every non-`Access` event under any of them debouncing
//! into one `()` — "rescan now" — for consumers like `ait`'s live
//! skills catalog whose reload is a rescan, not a file read.

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
/// `--init`, the config dir (`~/.config/ait/`, `%APPDATA%\ait\` on
/// Windows) may be absent; the caller surfaces the error
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

    let (raw_tx, raw_rx) = mpsc::channel();
    let mut watcher = raw_watcher(&raw_tx, &dir.display().to_string())?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|err| ConfigError::unlocated(format!("cannot watch {}: {err}", dir.display())))?;

    let (tx, rx) = mpsc::channel();
    let reload_file = file.clone();
    let thread = thread::spawn(move || {
        pump(
            raw_rx,
            tx,
            move |event| touches_file(event, &file),
            move || load_from(&reload_file),
            watcher,
        )
    });
    Ok(SettingsWatcher {
        receiver: rx,
        pump: PumpHandle::new(raw_tx, thread),
    })
}

/// [`watch`] generalized to a set of directories (REQUIREMENTS.md
/// §5.10.2 "watched for changes", Spec R10-T6): every existing dir is
/// subscribed RECURSIVELY, and any non-`Access` event under ANY of
/// them — a `SKILL.md` created, edited or deleted at any depth, a
/// `skills/` directory appearing under a watched `.ait` — settles
/// into one `()` event after the same [`DEBOUNCE`] quiet window. The
/// payload is deliberately just "rescan now": what changed is the
/// consumer's reload question, not the watcher's to answer.
///
/// Directories that do not exist are skipped — a skills dir that has
/// not been created yet, or that vanished between the caller's
/// resolution and this call, is simply nothing to watch; a set with
/// no watchable dirs still yields a live watcher that never fires, so
/// callers need no empty-set special case.
pub fn watch_dirs(dirs: &[PathBuf]) -> Result<DirsWatcher, ConfigError> {
    let (raw_tx, raw_rx) = mpsc::channel();
    let mut watcher = raw_watcher(&raw_tx, "the watched directories")?;
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        watcher
            .watch(dir, RecursiveMode::Recursive)
            .map_err(|err| {
                ConfigError::unlocated(format!("cannot watch {}: {err}", dir.display()))
            })?;
    }

    let (tx, rx) = mpsc::channel();
    let thread = thread::spawn(move || {
        pump(
            raw_rx,
            tx,
            |event| !matches!(event.kind, EventKind::Access(_)),
            || (),
            watcher,
        )
    });
    Ok(DirsWatcher {
        receiver: rx,
        pump: PumpHandle::new(raw_tx, thread),
    })
}

/// The consumer half of [`watch`]: holds the event channel and keeps the
/// OS watcher + pump thread alive. Drop it to stop watching — the thread
/// is joined so no watcher outlives its handle.
pub struct SettingsWatcher {
    receiver: Receiver<Result<Settings, ConfigError>>,
    pump: PumpHandle,
}

impl Drop for SettingsWatcher {
    fn drop(&mut self) {
        self.pump.stop();
    }
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

/// The consumer half of [`watch_dirs`]: the `()` event channel plus
/// the pump/OS-watcher ownership. Drop it to stop watching — the
/// thread is joined so no watcher outlives its handle.
pub struct DirsWatcher {
    receiver: Receiver<()>,
    pump: PumpHandle,
}

impl Drop for DirsWatcher {
    fn drop(&mut self) {
        self.pump.stop();
    }
}

impl DirsWatcher {
    /// The channel of "rescan now" events — drain it with
    /// [`Receiver::try_recv`] once per app-loop tick, or block on
    /// [`Receiver::recv`]/[`Receiver::recv_timeout`] where that fits
    /// better (tests).
    pub fn receiver(&self) -> &Receiver<()> {
        &self.receiver
    }
}

impl std::fmt::Debug for DirsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirsWatcher")
            .field("receiver", &self.receiver)
            .finish_non_exhaustive()
    }
}

/// The shared pump-thread ownership of both watcher types: holds the
/// Shutdown end of the raw channel and the thread handle so a watcher
/// Drop can wake the pump (it exits on Shutdown) and join it — no
/// watcher outlives its handle.
struct PumpHandle {
    shutdown: Sender<RawEvent>,
    thread: Option<JoinHandle<()>>,
}

impl PumpHandle {
    fn new(shutdown: Sender<RawEvent>, thread: JoinHandle<()>) -> Self {
        Self {
            shutdown,
            thread: Some(thread),
        }
    }

    /// Wake the pump thread (it exits on Shutdown) and join it so the
    /// OS watcher is dropped before the owning watcher returns.
    fn stop(&mut self) {
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

/// The shared `notify` watcher half of [`watch_from`]/[`watch_dirs`]:
/// filesystem events are pushed onto `raw_tx` as [`RawEvent::Fs`] so
/// the pump thread multiplexes them with Shutdown on one channel.
/// `context` names what was being watched when the OS watcher itself
/// cannot be created.
fn raw_watcher(
    raw_tx: &Sender<RawEvent>,
    context: &str,
) -> Result<RecommendedWatcher, ConfigError> {
    notify::recommended_watcher({
        let raw_tx = raw_tx.clone();
        move |event: notify::Result<Event>| {
            // A dead receiver only means the pump thread has exited.
            let _ = raw_tx.send(RawEvent::Fs(event));
        }
    })
    .map_err(|err| ConfigError::unlocated(format!("cannot watch {context}: {err}")))
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

/// The pump thread both watchers share: own the `notify` watcher
/// (dropping it stops OS delivery) and turn filesystem-event batches
/// into debounced outcomes on `tx` — `touches` decides whether an
/// event counts (the settings file's exact-path filter, the
/// directory watcher's any-non-access rule) and `reload` computes the
/// payload (`load_from` for settings, `()` for directories). Exits
/// when the consumer signals Shutdown, the send side dies, or the
/// consumer's receiver is gone — never panics on a bad save: reload
/// errors are channel payloads, not exceptional.
fn pump<T: Send + 'static>(
    raw_rx: Receiver<RawEvent>,
    tx: Sender<T>,
    touches: impl Fn(&Event) -> bool,
    reload: impl Fn() -> T,
    _watcher: RecommendedWatcher,
) {
    loop {
        match raw_rx.recv() {
            Err(_) | Ok(RawEvent::Shutdown) => return,
            // A notify error (e.g. event-queue overflow) may mean dropped
            // events — resync by treating it as a change.
            Ok(RawEvent::Fs(Err(_))) => {}
            Ok(RawEvent::Fs(Ok(event))) if !touches(&event) => continue,
            Ok(RawEvent::Fs(Ok(_))) => {}
        }
        // A touching event: drain the burst until the watched paths have
        // been quiet for DEBOUNCE — each *touching* event slides the
        // window, so a multi-write save reloads once, from the settled
        // state. Events the filter rejects are drained without sliding:
        // churn outside the watched surface must not starve the reload.
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
                    if touches(&event) {
                        deadline = Instant::now() + DEBOUNCE;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        if tx.send(reload()).is_err() {
            // The consumer dropped its receiver — nothing left to do.
            return;
        }
    }
}
