//! [`E2e`]: the `LiveSession`-backed harness behind every E01 test
//! (Spec E01, ticket E01-T1).
//!
//! [`E2e::spawn`] launches a program in a real PTY under a pinned
//! environment — `SHELL=/bin/sh`, a fixed `PS1`, a fresh temp cwd,
//! `TERM=xterm-256color` (the `LiveSession` driver default) — so screens
//! reproduce identically across machines and CI (REQUIREMENTS.md §6:
//! no TTY, no network). Input verbs mirror user actions; assertions read
//! the rendered grid; every wait is bounded by [`WAIT_TIMEOUT`] and
//! polls like `ait-term`'s live tests — never a fixed sleep.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use ait_term::{
    Cursor, ExitStatus, GridView, LiveEvent, LiveSession, LiveSessionOptions, SessionOptions, Size,
    SpawnError,
};

use crate::chord::parse_chord;
use crate::mouse::{self, MouseEvent};
use crate::tempdir::TempDir;

/// Bound on every wait the harness performs: the live-test tier's
/// generous value (Spec E01 — bounded waits only).
const WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Poll interval inside the wait loops (matches `ait-term`'s live tests).
const WAIT_POLL: Duration = Duration::from_millis(5);

/// `SHELL` pinned for every spawned program — a spawned `ait`'s tabs run
/// it, so tab titles and session behavior reproduce across machines.
const PINNED_SHELL: &str = "/bin/sh";

/// `PS1` pinned likewise: a developer's exported prompt must never leak
/// into the spawned shell's prompt or the snapshots.
const PINNED_PS1: &str = "$ ";

/// What [`E2e::spawn`] launches.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// The program to run in the PTY.
    pub program: OsString,
    /// `argv[1..]` for the program.
    pub args: Vec<OsString>,
    /// The PTY's initial size in cells.
    pub size: Size,
}

impl SpawnConfig {
    /// A spawn of `program` with `args` at `size`.
    pub fn new(program: impl Into<OsString>, args: &[&str], size: Size) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            size,
        }
    }
}

/// One spawned program in a real PTY: keystrokes, text, raw bytes, SGR
/// mouse events and resizes in; the rendered grid, cursor and exit
/// status out.
///
/// Input verbs panic with context on failure — a chord that doesn't
/// parse or a PTY write that fails is a test bug or a dead child, and a
/// panic carries the screen for the failure dump.
pub struct E2e {
    session: LiveSession,
    cwd: TempDir,
    /// The child's status once the event stream surfaced `Exited`.
    exited: Option<ExitStatus>,
    /// Hook point for the raw-PTY-output tail ticket E01-T5 wires
    /// `LiveSession` into; until then the buffer honestly stays empty.
    tail: Vec<u8>,
}

impl E2e {
    /// Spawn `config`'s program in a PTY with the pinned environment:
    /// `SHELL`/`PS1` overridden, `cwd` a fresh temp dir, `TERM` the
    /// driver's `xterm-256color` default. OSC 52 is opted in so a later
    /// ticket's clipboard assertions see the app's copies as events.
    pub fn spawn(config: SpawnConfig) -> Result<Self, SpawnError> {
        let cwd = TempDir::fresh().map_err(|e| SpawnError::Io(e.to_string()))?;
        let session = LiveSession::spawn(LiveSessionOptions {
            session: SessionOptions {
                size: config.size,
                osc52: true,
                ..Default::default()
            },
            program: Some(config.program),
            args: config.args,
            cwd: Some(cwd.path().to_path_buf()),
            env: vec![
                (OsString::from("SHELL"), OsString::from(PINNED_SHELL)),
                (OsString::from("PS1"), OsString::from(PINNED_PS1)),
            ],
        })?;
        Ok(Self {
            session,
            cwd,
            exited: None,
            tail: Vec::new(),
        })
    }

    /// Press a named chord (`"Alt+T"`, `"Ctrl+PageDown"`, `"F5"`, …),
    /// encoded the way a real legacy terminal would send it — see
    /// [`parse_chord`].
    ///
    /// # Panics
    /// On an unparseable chord or a failed PTY write.
    pub fn press(&self, chord: &str) {
        let event = parse_chord(chord).unwrap_or_else(|e| panic!("press({chord:?}): {e}"));
        self.session
            .send_key(event)
            .unwrap_or_else(|e| panic!("press({chord:?}): PTY write failed: {e}"));
    }

    /// Type literal text: its UTF-8 bytes to the program's stdin,
    /// exactly as written (`\n` stays `\n` — use [`press`](Self::press)
    /// `"Enter"` for the `\r` terminals send).
    ///
    /// # Panics
    /// On a failed PTY write.
    pub fn type_text(&self, text: &str) {
        self.send_bytes(text.as_bytes());
    }

    /// The raw-bytes escape hatch — anything [`press`](Self::press)
    /// can't express (bracketed paste, exotic sequences).
    ///
    /// # Panics
    /// On a failed PTY write.
    pub fn send_bytes(&self, bytes: &[u8]) {
        self.session
            .send_bytes(bytes)
            .expect("send_bytes: PTY write failed");
    }

    /// Send one SGR (1006) mouse event at the 1-based screen cell
    /// `(column, row)` — the coordinates the wire itself uses.
    ///
    /// # Panics
    /// On a failed PTY write.
    pub fn send_mouse(&self, event: MouseEvent, column: u16, row: u16) {
        self.send_bytes(&mouse::encode(event, column, row));
    }

    /// Resize the PTY winsize and the grid.
    ///
    /// # Panics
    /// On a failed winsize write (a just-dead child).
    pub fn resize(&mut self, size: Size) {
        self.session.resize(size).expect("resize: PTY write failed");
    }

    /// Wait — bounded by [`WAIT_TIMEOUT`] — until `pred` accepts the
    /// current screen. The session is drained each poll, so terminal
    /// replies (device-attributes answers, …) keep flowing.
    ///
    /// # Panics
    /// On timeout (with the final screen text), or early when the
    /// program exits while the predicate still fails.
    pub fn wait_for_screen(&mut self, description: &str, mut pred: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            self.pump();
            if pred(self) {
                return;
            }
            if let Some(status) = &self.exited {
                panic!(
                    "waiting for {description}: the program exited ({status:?})\n\
                     --- screen ---\n{}",
                    self.screen_text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {WAIT_TIMEOUT:?} waiting for {description}\n\
                 --- screen ---\n{}",
                self.screen_text()
            );
            std::thread::sleep(WAIT_POLL);
        }
    }

    /// Wait — bounded by [`WAIT_TIMEOUT`] — for the child to exit and
    /// return its [`ExitStatus`].
    ///
    /// # Panics
    /// On timeout (with the final screen text).
    pub fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            self.pump();
            if let Some(status) = self.exited.clone() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {WAIT_TIMEOUT:?} waiting for the program to exit\n\
                 --- screen ---\n{}",
                self.screen_text()
            );
            std::thread::sleep(WAIT_POLL);
        }
    }

    /// One drain round: feed queued PTY output into the grid and record
    /// the exit status when `Exited` surfaces.
    fn pump(&mut self) {
        for event in self.session.drain_events() {
            if let LiveEvent::Exited(status) = event {
                self.exited = Some(status);
            }
        }
    }

    /// The visible screen as text: every viewport row, trimmed,
    /// `\n`-joined — scrollback excluded. What `insta` snapshots assert
    /// on.
    pub fn screen_text(&self) -> String {
        self.session
            .grid()
            .visible_rows()
            .map(|row| row.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One visible row, trimmed; `""` when out of range.
    pub fn line(&self, n: usize) -> String {
        self.session
            .grid()
            .line(n as i32)
            .map(|row| row.text().trim_end().to_string())
            .unwrap_or_default()
    }

    /// The renderable cursor: viewport position and shape.
    pub fn cursor(&self) -> Cursor {
        self.session.cursor()
    }

    /// The grid size the program renders into.
    pub fn size(&self) -> Size {
        self.session.size()
    }

    /// Read-only view of the grid (viewport + scrollback) for
    /// assertions richer than [`screen_text`](Self::screen_text).
    pub fn grid(&self) -> GridView<'_> {
        self.session.grid()
    }

    /// The fresh temp dir the program runs in.
    pub fn cwd(&self) -> &Path {
        self.cwd.path()
    }

    /// The child's status once `Exited` surfaced, else `None`.
    pub fn exited(&self) -> Option<&ExitStatus> {
        self.exited.as_ref()
    }

    /// The tail of raw PTY output, for failure dumps.
    ///
    /// **Hook point**: ticket E01-T5 adds a bounded raw-output tail to
    /// `LiveSession`; until then this returns the empty buffer the
    /// harness keeps — the accessor's contract (a byte slice covering
    /// the most recent output) already holds.
    pub fn pty_tail(&self) -> &[u8] {
        &self.tail
    }
}
