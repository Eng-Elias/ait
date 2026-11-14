//! The live [`Session`] driver: a real shell in a real PTY.
//!
//! [`LiveSession`] wraps the pure, byte-fed [`Session`] core (Spec R01's
//! "`ait-term` internal shape" decision): `portable-pty` spawns the program,
//! a dedicated reader thread pumps PTY output into a bounded channel, and
//! [`drain_events`](LiveSession::drain_events) feeds it through
//! [`Session::feed_bytes`] on the caller's thread. Everything is
//! synchronous — no async runtime enters `ait-term`; `tokio` joins at the
//! `ait` binary in Round 2.
//!
//! Threads and shutdown
//! --------------------
//! Two worker threads per session:
//!
//! - **reader** — blocks in `read` on the PTY master and pushes chunks into
//!   a bounded `sync_channel`. Backpressure is the memory bound: when the
//!   channel is full the reader blocks, the PTY fills, and the child blocks
//!   on write — a flood (`cat` of a huge file, `yes`) can never outgrow the
//!   channel (REQUIREMENTS.md §6).
//! - **waiter** — polls `try_wait` on the child (so it can also service a
//!   kill request from `Drop`) and reports the exit status.
//!
//! Dropping the session requests a kill (`SIGHUP` escalating to `SIGKILL`
//! inside `portable-pty`), disconnects the output channel so a reader
//! blocked on `send` wakes up, and joins both threads. A process that
//! outlives its session leader (a backgrounded grandchild holding the PTY
//! slave) cannot hang `Drop`: the reader join is bounded, after which the
//! thread is simply detached.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use std::collections::VecDeque;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::events::SessionEvent;
use crate::grid::{Cursor, Damage, GridView};
use crate::key::KeyEvent;
use crate::session::{Session, SessionOptions, Size};

/// `TERM` advertised to the spawned program: the baseline the VT emulator
/// renders faithfully.
const DEFAULT_TERM: &str = "xterm-256color";

/// Unix fallback program when neither an explicit program nor `$SHELL`
/// is set (REQUIREMENTS.md §5.1). Windows has its own chain — see
/// [`windows_default_shell`].
#[cfg(not(windows))]
const FALLBACK_SHELL: &str = "/bin/sh";

/// Read size for the reader thread.
const READ_BUF: usize = 8 * 1024;

/// Bound on PTY-output chunks queued for `drain_events`. At most
/// `OUTPUT_CHANNEL_BOUND * READ_BUF` bytes (~512 KiB) of output can sit
/// unread before backpressure stalls the child.
const OUTPUT_CHANNEL_BOUND: usize = 64;

/// How often the waiter thread polls `try_wait` (also the granularity of
/// kill requests and of the exit-grace poll below).
const POLL: Duration = Duration::from_millis(10);

/// After the child exits, `drain_events` waits this long for the reader to
/// deliver trailing output before reporting `Exited`. Normally the reader
/// hits PTY EOF within milliseconds of `wait` returning; a grandchild that
/// inherited the slave can hold the PTY open longer, so the wait is bounded.
const READER_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Bound on the reader-thread join inside `Drop`.
const READER_JOIN_GRACE: Duration = Duration::from_millis(500);

/// Bound on [`LiveSession::output_tail`]: the most recent raw PTY output
/// retained for failure dumps (Spec E01 — the round's only product-code
/// change). 64 KiB covers several full screens of even escape-dense
/// output while staying trivially cheap to keep.
const OUTPUT_TAIL_BOUND: usize = 64 * 1024;

/// Constructor options for [`LiveSession`].
#[derive(Debug, Clone, Default)]
pub struct LiveSessionOptions {
    /// Core sizing and scrollback, forwarded to [`Session::new`]. Also the
    /// PTY's initial winsize.
    pub session: SessionOptions,
    /// Program to spawn in the PTY. `None` resolves `$SHELL`, falling back
    /// to the platform shell when unset or empty: `/bin/sh` on unix, the
    /// `pwsh.exe` → `powershell.exe` → `%ComSpec%` probe on Windows
    /// (Spec R16, §5.14).
    pub program: Option<OsString>,
    /// Arguments for the program (`argv[1..]`).
    pub args: Vec<OsString>,
    /// Working directory for the child; `None` inherits ours.
    pub cwd: Option<PathBuf>,
    /// Environment overrides applied on top of the inherited environment
    /// (may also override `TERM`).
    pub env: Vec<(OsString, OsString)>,
}

impl LiveSessionOptions {
    /// The program [`LiveSession::spawn`] will launch for these options:
    /// `program` when explicit, else `$SHELL`, else the platform default
    /// (`/bin/sh` on unix, the PowerShell probe on Windows — an empty
    /// `$SHELL` counts as unset on both). Frontends derive display
    /// names — a tab title is the basename — from the same source of
    /// truth the spawn uses, instead of re-resolving `$SHELL`
    /// themselves.
    pub fn resolved_program(&self) -> OsString {
        resolve_shell(self.program.clone(), std::env::var_os("SHELL"))
    }
}

/// Why [`LiveSession::spawn`] failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpawnError {
    /// `openpty` failed.
    #[error("failed to open a PTY: {0}")]
    OpenPty(String),
    /// The program could not be spawned in the PTY.
    #[error("failed to spawn {program:?} in the PTY: {reason}")]
    Spawn {
        /// The resolved program (see
        /// [`LiveSessionOptions::resolved_program`]).
        program: OsString,
        /// The underlying error.
        reason: String,
    },
    /// Cloning the PTY's I/O handles failed.
    #[error("failed to acquire PTY I/O handles: {0}")]
    Io(String),
    /// Spawning a worker thread failed.
    #[error("failed to spawn a session worker thread: {0}")]
    Thread(#[source] io::Error),
}

/// How the spawned program ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitStatus {
    /// The process exit code (`0` on a clean exit).
    pub code: u32,
    /// How the process was signaled, if it was (e.g. `"Terminated by
    /// SIGHUP"`), as reported by the platform.
    pub signal: Option<String>,
}

impl ExitStatus {
    /// `true` for exit code 0 with no signal.
    pub fn success(&self) -> bool {
        self.code == 0 && self.signal.is_none()
    }

    fn from_pty(status: portable_pty::ExitStatus) -> Self {
        Self {
            code: status.exit_code(),
            signal: status.signal().map(str::to_string),
        }
    }
}

/// Lifecycle of a [`LiveSession`] (Spec R01): `Running` once the child is
/// launched, `Exited` once the session's event path surfaces its death.
/// `Starting` is the transient phase inside [`LiveSession::spawn`] — a
/// returned session never reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// The PTY is open and the child is being spawned — transient inside
    /// [`LiveSession::spawn`].
    Starting,
    /// The child is (as far as the event stream knows) alive.
    Running,
    /// The child exited.
    Exited(ExitStatus),
}

/// A session event surfaced through [`LiveSession::drain_events`] or
/// [`LiveSession::try_recv_event`] — the live channel's unified stream:
/// output-ready hints, the core's observed [`SessionEvent`]s, and exit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LiveEvent {
    /// PTY output was fed into the grid; check [`LiveSession::damage`] and
    /// repaint.
    OutputReady,
    /// An event the core observed while feeding output — OSC 7/133/9/777
    /// and friends ([`SessionEvent`]).
    Osc(SessionEvent),
    /// The spawned program exited.
    ///
    /// `Exited` is withheld until the reader thread has delivered all
    /// output the kernel hands us (see [`READER_EXIT_GRACE`]), so by the
    /// time it surfaces the grid holds everything the program printed —
    /// unless a process that outlived the session leader is still holding
    /// the PTY open, in which case trailing output may arrive as further
    /// `OutputReady`/`Osc` events after `Exited`.
    Exited(ExitStatus),
}

/// A [`Session`] driven by a real PTY: the spawned program's output lands
/// in the grid, input goes back to its stdin.
///
/// The caller pumps the session: output becomes visible when
/// [`drain_events`](Self::drain_events) (or
/// [`try_recv_event`](Self::try_recv_event)) feeds queued PTY bytes into
/// the core. A frontend calls it once per frame; a test calls it until the
/// event it wants arrives.
pub struct LiveSession {
    session: Session,
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    output_rx: Option<Receiver<Vec<u8>>>,
    exit_rx: Receiver<ExitStatus>,
    kill_requested: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    waiter: Option<JoinHandle<()>>,
    child_pid: Option<u32>,
    /// Lifecycle state as seen by the event path: `Running` until
    /// `finish_exit` queues the `Exited` event, `Exited` from then on.
    state: SessionState,
    /// Events produced but not yet surfaced to the caller: `drain_events`
    /// empties this, `try_recv_event` pops one at a time.
    pending: VecDeque<LiveEvent>,
    /// The output channel disconnected (the reader thread finished): every
    /// byte the PTY will ever produce has been fed.
    reader_done: bool,
    /// `Exited` was already surfaced.
    exit_emitted: bool,
    /// The raw-output tail backing [`output_tail`](Self::output_tail):
    /// `tail[tail_start..]` is the retained window (≤
    /// [`OUTPUT_TAIL_BOUND`]); `tail_start` only advances, and dead
    /// bytes before it are compacted away once they outgrow the bound
    /// themselves, so appends stay amortized-cheap under floods.
    tail: Vec<u8>,
    tail_start: usize,
    /// Optional mirror receiving raw PTY bytes as the reader thread
    /// reads them — see [`set_output_mirror`](Self::set_output_mirror).
    /// Shared with the reader thread; `None` when detached (or after a
    /// failed mirror write).
    output_mirror: OutputMirror,
}

impl LiveSession {
    /// Open a PTY and spawn the configured program in it.
    ///
    /// The returned session is [`SessionState::Running`]; the `Starting`
    /// phase exists only inside this call.
    pub fn spawn(options: LiveSessionOptions) -> Result<Self, SpawnError> {
        let session = Session::new(options.session);

        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(pty_size(options.session.size))
            .map_err(|e| SpawnError::OpenPty(e.to_string()))?;

        let program = options.resolved_program();
        let mut cmd = CommandBuilder::new(&program);
        cmd.args(&options.args);
        cmd.env("TERM", DEFAULT_TERM);
        for (key, value) in &options.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &options.cwd {
            cmd.cwd(cwd);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| SpawnError::Spawn {
                program: program.clone(),
                reason: e.to_string(),
            })?;
        let child_pid = child.process_id();

        // If handle setup fails the child must not be left running.
        let (reader, writer) = match acquire_io(&*pair.master) {
            Ok(io) => io,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };

        // Drop our slave fd: while we hold it the master can never see EOF,
        // so the reader would never finish.
        drop(pair.slave);

        let (output_tx, output_rx) = mpsc::sync_channel(OUTPUT_CHANNEL_BOUND);
        let (exit_tx, exit_rx) = mpsc::channel();
        let kill_requested = Arc::new(AtomicBool::new(false));
        let output_mirror: OutputMirror = Arc::new(Mutex::new(None));

        // The waiter owns the child; spawn it first so that a reader-spawn
        // failure can still request a kill and reap the child.
        let waiter = spawn_thread("ait-term waiter", {
            let kill_requested = Arc::clone(&kill_requested);
            move || {
                wait_loop(child, kill_requested, exit_tx);
            }
        })?;
        let reader = match spawn_thread("ait-term reader", {
            let output_mirror = Arc::clone(&output_mirror);
            move || {
                read_loop(reader, output_tx, output_mirror);
            }
        }) {
            Ok(reader) => reader,
            Err(e) => {
                kill_requested.store(true, Ordering::SeqCst);
                let _ = waiter.join();
                return Err(e);
            }
        };

        Ok(Self {
            session,
            master: pair.master,
            writer: Mutex::new(writer),
            output_rx: Some(output_rx),
            exit_rx,
            kill_requested,
            state: SessionState::Running,
            reader: Some(reader),
            waiter: Some(waiter),
            child_pid,
            pending: VecDeque::new(),
            reader_done: false,
            exit_emitted: false,
            tail: Vec::new(),
            tail_start: 0,
            output_mirror,
        })
    }

    /// Current session lifecycle state, as seen by the event path:
    /// `Exited` once [`drain_events`](Self::drain_events) (or
    /// [`try_recv_event`](Self::try_recv_event)) has produced the
    /// [`LiveEvent::Exited`] event — even if the caller has not popped it
    /// yet — `Running` until then.
    pub fn state(&self) -> SessionState {
        self.state.clone()
    }

    /// The spawned child's process id, if the platform reports one.
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Read-only view of the grid (viewport + scrollback). Reflects output
    /// drained so far.
    pub fn grid(&self) -> GridView<'_> {
        self.session.grid()
    }

    /// Whether the application enabled bracketed-paste mode (`DECSET`
    /// 2004) — see [`Session::bracketed_paste`].
    pub fn bracketed_paste(&self) -> bool {
        self.session.bracketed_paste()
    }

    /// The renderable cursor: viewport position and shape.
    pub fn cursor(&self) -> Cursor {
        self.session.cursor()
    }

    /// Damage accumulated since the last call; clears the tracker.
    pub fn damage(&mut self) -> Damage {
        self.session.damage()
    }

    /// Current grid size.
    pub fn size(&self) -> Size {
        self.session.size()
    }

    /// Resize: forward the new winsize to the PTY (the kernel signals the
    /// child's foreground process group) and resize the grid. The grid
    /// resizes even if the PTY write fails (e.g. the child already died).
    pub fn resize(&mut self, size: Size) -> io::Result<()> {
        let result = self
            .master
            .resize(pty_size(size))
            .map_err(|e| io::Error::other(e.to_string()));
        self.session.resize(size);
        result
    }

    /// Encode a [`KeyEvent`] the way [`Session::encode_key`] would.
    pub fn encode_key(&self, event: KeyEvent) -> Vec<u8> {
        self.session.encode_key(event)
    }

    /// Send a key press to the program: encoded via
    /// [`Session::encode_key`] (application cursor/keypad modes honored)
    /// and written to the PTY.
    pub fn send_key(&self, event: KeyEvent) -> io::Result<()> {
        self.send_bytes(&self.session.encode_key(event))
    }

    /// The tail of the raw PTY output this session has consumed: the
    /// most recent bytes — up to [`OUTPUT_TAIL_BOUND`] (64 KiB) — that
    /// [`drain_events`](Self::drain_events) / [`try_recv_event`](Self::try_recv_event)
    /// fed into the grid, exactly as read from the PTY (escape sequences
    /// included, nothing parsed or stripped).
    ///
    /// "Consumed" is literal: output sitting in the reader channel is
    /// not in the tail until a drain feeds it, so a session that is
    /// never drained reports an empty tail no matter how much the child
    /// wrote. Intended for failure dumps (Spec E01): it answers "what
    /// did the program emit last", not "everything it ever wrote".
    pub fn output_tail(&self) -> &[u8] {
        &self.tail[self.tail_start..]
    }

    /// Mirror the raw PTY output to `writer` as the reader thread reads
    /// it — *before* the grid consumes it, so the mirror tracks the
    /// program live (unlike [`output_tail`](Self::output_tail), which
    /// only covers bytes already drained). Escape sequences pass
    /// through untouched: attach a terminal (e.g. `/dev/tty`) to watch
    /// the session render in real time. `None` detaches the mirror.
    ///
    /// The mirror is a tap, not part of the pipeline: a failing write
    /// detaches it silently rather than disturbing the session, and it
    /// changes nothing about [`drain_events`](Self::drain_events),
    /// backpressure or the event model.
    pub fn set_output_mirror(&self, writer: Option<Box<dyn Write + Send>>) {
        if let Ok(mut mirror) = self.output_mirror.lock() {
            *mirror = writer;
        }
    }

    /// Write raw bytes to the program's stdin.
    pub fn send_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("PTY writer lock poisoned"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// Feed all pending PTY output into the grid and return what happened.
    ///
    /// Ordering guarantee: within one drain the events are grouped by
    /// source — the [`LiveEvent::Osc`] events observed while feeding (in
    /// stream order, with the caveat documented on
    /// [`Session::feed_bytes`]), then one coalesced
    /// [`LiveEvent::OutputReady`] covering those bytes, then
    /// [`LiveEvent::Exited`] if the child has finished. Across drains,
    /// events are chronological. Terminal replies the core generates while
    /// feeding (device-status answers, …) are written back to the PTY
    /// before this returns.
    pub fn drain_events(&mut self) -> Vec<LiveEvent> {
        self.pull_events();
        self.pending.drain(..).collect()
    }

    /// Non-blocking single event from the same stream
    /// [`drain_events`](Self::drain_events) returns; `None` when nothing is
    /// pending.
    pub fn try_recv_event(&mut self) -> Option<LiveEvent> {
        if self.pending.is_empty() {
            self.pull_events();
        }
        self.pending.pop_front()
    }

    /// One drain round into `pending`: feed queued output, then check the
    /// waiter for an exit report.
    fn pull_events(&mut self) {
        self.feed_pending();
        if let Some(status) = self.pending_exit() {
            self.finish_exit(status);
        }
    }

    /// Drain every output chunk the reader has queued into the core,
    /// queueing the events it produced ([`LiveEvent::Osc`]s in stream
    /// order, then one coalesced [`LiveEvent::OutputReady`]).
    fn feed_pending(&mut self) {
        let mut fed = false;
        {
            let Some(rx) = &self.output_rx else {
                return;
            };
            loop {
                match rx.try_recv() {
                    Ok(chunk) => {
                        push_tail(&mut self.tail, &mut self.tail_start, &chunk);
                        for event in self.session.feed_bytes(&chunk) {
                            self.pending.push_back(LiveEvent::Osc(event));
                        }
                        fed = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    // The reader finished: no more output will ever arrive.
                    Err(TryRecvError::Disconnected) => {
                        self.reader_done = true;
                        break;
                    }
                }
            }
        }
        if fed {
            self.flush_pty_responses();
            self.note_output_ready();
        }
    }

    /// Queue `OutputReady` unless one is already queued at the tail —
    /// consecutive feeds coalesce into a single repaint hint.
    fn note_output_ready(&mut self) {
        if !matches!(self.pending.back(), Some(LiveEvent::OutputReady)) {
            self.pending.push_back(LiveEvent::OutputReady);
        }
    }

    /// Write terminal-generated replies (device-status answers etc.) back
    /// to the PTY.
    fn flush_pty_responses(&mut self) {
        for response in self.session.take_pty_responses() {
            if let Err(e) = self.send_bytes(response.as_bytes()) {
                tracing::warn!("failed to write terminal reply to PTY: {e}");
                break;
            }
        }
    }

    /// The child's exit status, if the waiter reported one and `Exited`
    /// was not yet surfaced.
    fn pending_exit(&mut self) -> Option<ExitStatus> {
        if self.exit_emitted {
            return None;
        }
        self.exit_rx.try_recv().ok()
    }

    /// Queue `Exited`: first wait (bounded by [`READER_EXIT_GRACE`]) for
    /// the reader to deliver trailing output — those bytes' events land
    /// ahead of the `Exited` so the grid is complete when it fires.
    fn finish_exit(&mut self, status: ExitStatus) {
        self.exit_emitted = true;
        let deadline = Instant::now() + READER_EXIT_GRACE;
        loop {
            self.feed_pending();
            if self.reader_done || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL);
        }
        if self.reader_done {
            self.join_reader();
        }
        // `state()` mirrors the event stream: flip it as the event is
        // queued so a dead child can never report `Running`.
        self.state = SessionState::Exited(status.clone());
        self.pending.push_back(LiveEvent::Exited(status));
    }

    /// Join the reader thread if it already finished.
    fn join_reader(&mut self) {
        if self
            .reader
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            if let Some(handle) = self.reader.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        // Ask the waiter to kill the child (SIGHUP, escalating to SIGKILL).
        self.kill_requested.store(true, Ordering::SeqCst);

        // Disconnect the output channel: a reader blocked on `send` (the
        // flood case) wakes up and exits instead of hanging teardown.
        drop(self.output_rx.take());

        // The waiter kills and reaps the child.
        if let Some(waiter) = self.waiter.take() {
            let _ = waiter.join();
        }

        // Once the child is dead the PTY hits EOF and the reader exits;
        // bound the join anyway — a leaked grandchild holding the slave
        // must not hang drop. `JoinHandle` detaches when dropped, so an
        // unfinished reader simply outlives us without blocking anyone.
        let deadline = Instant::now() + READER_JOIN_GRACE;
        while let Some(handle) = &self.reader {
            if handle.is_finished() {
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!("PTY reader thread did not exit; detaching");
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.join_reader();
    }
}

/// Append a consumed chunk to the output tail, keeping only the last
/// [`OUTPUT_TAIL_BOUND`] bytes. `tail[start..]` is the live window; the
/// dead prefix is compacted away once it alone exceeds the bound, so a
/// flood pays one occasional `drain` instead of a memmove per chunk.
/// (Free function, not a method: `feed_pending` calls it while holding
/// a borrow of `output_rx`.)
fn push_tail(tail: &mut Vec<u8>, start: &mut usize, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    if tail.len() - *start > OUTPUT_TAIL_BOUND {
        *start = tail.len() - OUTPUT_TAIL_BOUND;
    }
    if *start >= OUTPUT_TAIL_BOUND {
        tail.drain(..*start);
        *start = 0;
    }
}

/// The optional live output tap attached with
/// [`LiveSession::set_output_mirror`]: shared with the reader thread,
/// `None` until a writer is set (and reset to `None` if a mirrored
/// write fails).
type OutputMirror = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// Mirror one freshly-read chunk to the output mirror, if attached. A
/// failing write detaches the mirror rather than disturbing the
/// pipeline (mirroring is observability, not data path); a poisoned
/// lock skips the chunk.
fn mirror_write(mirror: &OutputMirror, chunk: &[u8]) {
    let Ok(mut mirror) = mirror.lock() else {
        return;
    };
    if let Some(writer) = mirror.as_mut() {
        if writer.write_all(chunk).is_err() || writer.flush().is_err() {
            *mirror = None;
        }
    }
}

/// The reader thread's loop: push PTY output into the bounded channel.
/// Blocks on `read` (no output) or on `send` (consumer behind); exits on
/// EOF/EIO (child's slave closed) or when the receiver is gone (teardown).
fn read_loop(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::SyncSender<Vec<u8>>,
    mirror: OutputMirror,
) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Mirror before send: the tap stays live even when
                // backpressure has the reader blocked on a full channel.
                mirror_write(&mirror, &buf[..n]);
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(e) => {
                // EIO is how Linux reports "the slave side is fully
                // closed" — the normal EOF path. Anything else is worth a
                // log line.
                if e.kind() != io::ErrorKind::Other {
                    tracing::warn!("PTY read failed: {e}");
                }
                break;
            }
        }
    }
}

/// The waiter thread's loop: poll the child (blocking `wait` cannot be
/// interrupted, so we poll to stay responsive to kill requests), then
/// record and report its exit status.
fn wait_loop(
    mut child: Box<dyn Child + Send + Sync>,
    kill_requested: Arc<AtomicBool>,
    exit_tx: mpsc::Sender<ExitStatus>,
) {
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break ExitStatus::from_pty(status),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("failed to poll PTY child: {e}");
                break ExitStatus {
                    code: 1,
                    signal: Some(format!("reaping the child failed: {e}")),
                };
            }
        }
        if kill_requested.swap(false, Ordering::SeqCst) {
            // `Child::kill` (the std::process::Child impl portable-pty
            // returns on unix) sends SIGHUP, waits briefly, then escalates
            // to SIGKILL — unlike clone_killer's SIGHUP-only signaller.
            if let Err(e) = child.kill() {
                tracing::debug!("PTY child kill request failed (already dead?): {e}");
            }
        }
        std::thread::sleep(POLL);
    };
    // Unbounded channel: never blocks, so the waiter always exits.
    let _ = exit_tx.send(status);
}

fn spawn_thread(
    name: &str,
    f: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle<()>, SpawnError> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .map_err(SpawnError::Thread)
}

/// The two I/O handles cloned off the PTY master: output stream and input
/// stream.
type PtyIo = (Box<dyn Read + Send>, Box<dyn Write + Send>);

/// Reader/writer handles off the master, or the error to report.
fn acquire_io(master: &dyn MasterPty) -> Result<PtyIo, SpawnError> {
    let reader = master
        .try_clone_reader()
        .map_err(|e| SpawnError::Io(e.to_string()))?;
    let writer = master
        .take_writer()
        .map_err(|e| SpawnError::Io(e.to_string()))?;
    Ok((reader, writer))
}

/// Program resolution: explicit option, else `$SHELL`, else the
/// platform default shell. An empty `$SHELL` counts as unset — and the
/// same `program`-then-`$SHELL` precedence holds on every OS, so a
/// Git/MSYS `bash.exe` on `%SHELL%` beats the Windows probe too
/// (Spec R16, §5.14).
fn resolve_shell(program: Option<OsString>, env_shell: Option<OsString>) -> OsString {
    program
        .or_else(|| env_shell.filter(|s| !s.is_empty()))
        .unwrap_or_else(platform_default_shell)
}

/// The platform default once explicit program and `$SHELL` both miss:
/// `/bin/sh` on unix (REQUIREMENTS.md §5.1).
#[cfg(not(windows))]
fn platform_default_shell() -> OsString {
    OsString::from(FALLBACK_SHELL)
}

/// The platform default once explicit program and `$SHELL` both miss:
/// the PowerShell probe on Windows (Spec R16, §5.14).
#[cfg(windows)]
fn platform_default_shell() -> OsString {
    windows_default_shell(std::env::var_os("PATH"), std::env::var_os("ComSpec"))
}

/// The Windows default-shell chain: the first of `pwsh.exe` /
/// `powershell.exe` found probing `path` (the name preference outranks
/// PATH order — `pwsh.exe` wins even when `powershell.exe` sits in an
/// earlier dir), else `%ComSpec%`, else `cmd.exe` as the last resort
/// (`%ComSpec%` is a system variable but nothing guarantees it is set).
/// Returns full paths so the spawn never depends on the child's own
/// search and the resolved basename stays `pwsh.exe`/`powershell.exe`
/// for the injection planner (Spec R16-T3). Parameterized on the env
/// values so unit tests exercise the chain off-Windows — ConPTY itself
/// is Windows-only.
#[cfg(any(windows, test))]
fn windows_default_shell(path: Option<OsString>, comspec: Option<OsString>) -> OsString {
    if let Some(path) = path {
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        for name in ["pwsh.exe", "powershell.exe"] {
            for dir in &dirs {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return candidate.into_os_string();
                }
            }
        }
    }
    comspec
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("cmd.exe"))
}

/// Grid [`Size`] → kernel winsize, clamped to the `u16` range and at least
/// one cell.
fn pty_size(size: Size) -> PtySize {
    let clamp = |cells: usize| u16::try_from(cells.max(1)).unwrap_or(u16::MAX);
    PtySize {
        rows: clamp(size.lines),
        cols: clamp(size.columns),
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_program_wins_over_shell_env() {
        assert_eq!(
            resolve_shell(
                Some(OsString::from("/bin/zsh")),
                Some(OsString::from("/bin/bash"))
            ),
            OsString::from("/bin/zsh")
        );
    }

    #[test]
    fn shell_env_is_used_when_no_explicit_program() {
        assert_eq!(
            resolve_shell(None, Some(OsString::from("/bin/bash"))),
            OsString::from("/bin/bash")
        );
    }

    /// The `/bin/sh` fallback is the unix default only — on Windows the
    /// same call resolves through `windows_default_shell`.
    #[cfg(not(windows))]
    #[test]
    fn missing_shell_env_falls_back_to_bin_sh() {
        assert_eq!(resolve_shell(None, None), OsString::from("/bin/sh"));
    }

    #[cfg(not(windows))]
    #[test]
    fn empty_shell_env_counts_as_unset() {
        assert_eq!(
            resolve_shell(None, Some(OsString::new())),
            OsString::from("/bin/sh")
        );
    }

    /// `resolved_program` is the options-side view of `resolve_shell` —
    /// the same value `spawn` launches, so a frontend can title from it.
    #[test]
    fn resolved_program_is_what_spawn_would_launch() {
        let options = LiveSessionOptions {
            program: Some(OsString::from("/bin/zsh")),
            ..Default::default()
        };
        assert_eq!(options.resolved_program(), OsString::from("/bin/zsh"));
    }

    /// A temp dir that removes itself on drop — the PATH-probe tests'
    /// fixture: `windows_default_shell` stats real paths, so the probe
    /// needs real files to find.
    struct ProbeDir(PathBuf);

    impl ProbeDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("ait-term-probe-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("probe dir should create");
            Self(path)
        }

        /// Create an empty entry named like an executable on PATH.
        fn touch(&self, name: &str) {
            std::fs::write(self.0.join(name), b"").expect("probe file should write");
        }

        /// The dir alone as a one-entry PATH value.
        fn path(&self) -> OsString {
            std::env::join_paths([&self.0]).expect("probe dir should join")
        }
    }

    impl Drop for ProbeDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn windows_probe_returns_the_full_path_of_pwsh_on_path() {
        let dir = ProbeDir::new("pwsh");
        dir.touch("pwsh.exe");
        assert_eq!(
            windows_default_shell(Some(dir.path()), None),
            dir.0.join("pwsh.exe").into_os_string()
        );
    }

    #[test]
    fn windows_probe_prefers_pwsh_over_powershell_regardless_of_path_order() {
        // powershell.exe sits in the EARLIER dir — pwsh still wins: the
        // name preference outranks PATH order (Spec R16, §5.14).
        let earlier = ProbeDir::new("earlier");
        let later = ProbeDir::new("later");
        earlier.touch("powershell.exe");
        later.touch("pwsh.exe");
        let path = std::env::join_paths([&earlier.0, &later.0]).expect("probe dirs should join");
        assert_eq!(
            windows_default_shell(Some(path), None),
            later.0.join("pwsh.exe").into_os_string()
        );
    }

    #[test]
    fn windows_probe_uses_powershell_when_pwsh_is_absent() {
        let dir = ProbeDir::new("winps");
        dir.touch("powershell.exe");
        assert_eq!(
            windows_default_shell(
                Some(dir.path()),
                Some(OsString::from("C:\\Windows\\System32\\cmd.exe")),
            ),
            dir.0.join("powershell.exe").into_os_string()
        );
    }

    #[test]
    fn windows_comspec_wins_when_no_powershell_is_on_path() {
        // Real dirs, just no PowerShell in them.
        let dir = ProbeDir::new("empty");
        let comspec = OsString::from("C:\\Windows\\System32\\cmd.exe");
        assert_eq!(
            windows_default_shell(Some(dir.path()), Some(comspec.clone())),
            comspec
        );
    }

    #[test]
    fn windows_comspec_wins_when_path_is_unset() {
        let comspec = OsString::from("C:\\Windows\\System32\\cmd.exe");
        assert_eq!(windows_default_shell(None, Some(comspec.clone())), comspec);
    }

    #[test]
    fn windows_cmd_exe_is_the_last_resort() {
        let dir = ProbeDir::new("bare");
        // Nothing on PATH and `%ComSpec%` unset or empty — `cmd.exe`
        // resolves through the system directory search at spawn.
        for comspec in [None, Some(OsString::new())] {
            assert_eq!(
                windows_default_shell(Some(dir.path()), comspec),
                OsString::from("cmd.exe")
            );
        }
    }

    #[test]
    fn pty_size_clamps_to_at_least_one_cell() {
        let size = pty_size(Size::new(0, 0));
        assert_eq!(size.cols, 1);
        assert_eq!(size.rows, 1);
    }
}
