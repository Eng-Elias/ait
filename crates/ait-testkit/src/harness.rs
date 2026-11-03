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

use std::cell::RefCell;
use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ait_term::{
    Cursor, ExitStatus, GridView, LiveEvent, LiveSession, LiveSessionOptions, SessionEvent,
    SessionOptions, Size, SpawnError,
};

use crate::agent::{self, Agent};
use crate::chord::parse_chord;
use crate::command::Command;
use crate::driver::{Authored, Driver};
use crate::evidence::{self, ActionEntry};
use crate::mouse::{self, MouseEvent};
use crate::screen::ScreenState;
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
    /// Extra environment pairs, applied *after* the pinned
    /// `SHELL`/`PS1`/`XDG_CONFIG_HOME` so they can override them — the
    /// escape hatch a scenario uses to pin a different prompt (or a
    /// different inner shell, or a fixture `settings.toml`) than the
    /// defaults.
    pub env: Vec<(OsString, OsString)>,
    /// Headed mode: mirror the spawned program's raw PTY output to the
    /// controlling terminal as it's read, so the app renders live while
    /// tests drive it (escape sequences land untouched — the app takes
    /// over the host terminal's alternate screen). `AIT_E2E_HEADED`
    /// sets the same flag globally at [`E2e::spawn`]. Run with
    /// `--test-threads=1`: parallel tests would interleave on the one
    /// terminal.
    pub headed: bool,
    /// Watch pacing: a fixed delay after every *input* verb, so a
    /// headed run stays readable to a human eye. `AIT_E2E_SLOWMO=<ms>`
    /// sets it at [`E2e::spawn`], overriding this field. It delays
    /// inputs only — waits stay predicate-based and bounded, so it can
    /// never make a test pass or fail differently.
    pub slowmo_ms: u64,
}

impl SpawnConfig {
    /// A spawn of `program` with `args` at `size`.
    pub fn new(program: impl Into<OsString>, args: &[&str], size: Size) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            size,
            env: Vec::new(),
            headed: false,
            slowmo_ms: 0,
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
    /// Spawn time — the `t+` column of the action log.
    started: Instant,
    /// The ordered action log every input verb and wait appends to —
    /// the third failure-evidence file (Spec E01, ticket E01-T5).
    /// `RefCell` because input verbs take `&self`.
    log: RefCell<Vec<ActionEntry>>,
    /// The spawned program's name, kept for recording provenance.
    program: String,
    /// The backing that resolves `agent` steps at record time —
    /// `AIT_E2E_DRIVER`'s [`Command`] when the variable is set at
    /// spawn, else the replay-only [`Authored`], until
    /// [`set_driver`](Self::set_driver) injects another.
    driver: Box<dyn Driver>,
    /// Recordings-root override from
    /// [`set_recordings_dir`](Self::set_recordings_dir); `None` resolves
    /// the `AIT_E2E_RECORDINGS`/`<crate>/tests/recordings` convention.
    recordings_dir: Option<PathBuf>,
    /// Record mode (`AIT_E2E_RECORD` at spawn, or
    /// [`set_record_mode`](Self::set_record_mode)): agent steps resolve
    /// through `driver` and rewrite their recordings instead of
    /// replaying.
    record: bool,
    /// The agent-step counter — each `agent().act`/`assert` consumes one
    /// index → its `<step>.jsonl`.
    agent_step: usize,
    /// Every `ClipboardWrite` the program has emitted so far, in order —
    /// harness sessions opt into `SessionOptions::osc52` so a spawned
    /// app's selection-copy surfaces here (Spec E01, ticket E01-T4's
    /// `expect_clipboard` verb).
    clipboard: Vec<String>,
    /// Headed mode is on — PTY output mirrors to the controlling
    /// terminal, and [`Drop`] restores that terminal's modes.
    headed: bool,
    /// Watch pacing — the fixed delay applied after each input verb
    /// (`SpawnConfig::slowmo_ms` / `AIT_E2E_SLOWMO`).
    slowmo: Duration,
}

impl E2e {
    /// Spawn `config`'s program in a PTY with the pinned environment:
    /// `SHELL`/`PS1`/`XDG_CONFIG_HOME` overridden, `cwd` a fresh temp
    /// dir, `TERM` the driver's `xterm-256color` default. OSC 52 is
    /// opted in so a spawned app's copies surface as events
    /// ([`clipboard_writes`](Self::clipboard_writes)).
    /// [`SpawnConfig::env`] pairs apply after the pinned variables,
    /// so they override them.
    pub fn spawn(config: SpawnConfig) -> Result<Self, SpawnError> {
        let cwd = TempDir::fresh().map_err(|e| SpawnError::Io(e.to_string()))?;
        let program = config.program.to_string_lossy().into_owned();
        let mut env = vec![
            (OsString::from("SHELL"), OsString::from(PINNED_SHELL)),
            (OsString::from("PS1"), OsString::from(PINNED_PS1)),
            // Since Round 3 a spawned `ait` loads
            // `$XDG_CONFIG_HOME/ait/settings.toml` — an unpinned value
            // would read the HOST's config and make the screen depend
            // on the developer's machine. Pin it to a path inside the
            // fresh cwd (missing → built-in defaults); a scenario can
            // still point it at a fixture via `SpawnConfig::env`.
            (
                OsString::from("XDG_CONFIG_HOME"),
                cwd.path().join("xdg-config").into_os_string(),
            ),
            // Debian-family global zshrcs run `compinit` unless this
            // is set — and `compinit` parks interactively on
            // "insecure directories" when the host's fpath holds
            // group/other-writable entries (CI runners). A parked
            // shell never draws a prompt, so integration tests time
            // out for an environment reason, not a product one.
            (OsString::from("skip_global_compinit"), OsString::from("1")),
        ];
        env.extend(config.env);
        let session = LiveSession::spawn(LiveSessionOptions {
            session: SessionOptions {
                size: config.size,
                osc52: true,
                ..Default::default()
            },
            program: Some(config.program),
            args: config.args,
            cwd: Some(cwd.path().to_path_buf()),
            env,
        })?;
        // Headed mode (`SpawnConfig::headed` or `AIT_E2E_HEADED`):
        // mirror raw PTY output to the controlling terminal as it's
        // read — the app takes over the host terminal's alternate
        // screen while the test drives it.
        let headed = config.headed || env_flag("AIT_E2E_HEADED");
        if headed {
            session.set_output_mirror(Some(headed_writer()));
        }
        // `AIT_E2E_DRIVER` selects the resolving driver for record mode
        // (Spec E01, ticket E01-T3); unset or blank keeps `Authored`.
        // Replay never consults the driver, so an env-selected `Command`
        // is inert outside `AIT_E2E_RECORD` runs.
        let driver: Box<dyn Driver> = match Command::from_env() {
            Some(command) => Box::new(command),
            None => Box::new(Authored),
        };
        Ok(Self {
            session,
            cwd,
            exited: None,
            started: Instant::now(),
            log: RefCell::new(Vec::new()),
            program,
            driver,
            recordings_dir: None,
            record: agent::record_flag(),
            agent_step: 0,
            clipboard: Vec::new(),
            headed,
            slowmo: slowmo(config.slowmo_ms),
        })
    }

    /// Append `action` to the ordered action log, stamped with the time
    /// since spawn and the current [`ScreenState::hash`] — the same hash
    /// a recording's `screen_hash` field stores, so the dump's log can be
    /// compared against recorded steps (the evidence a failure dump
    /// prints).
    fn log_action(&self, action: String) {
        self.log.borrow_mut().push(ActionEntry {
            at: self.started.elapsed(),
            action,
            screen_hash: self.screen_state().hash(),
        });
    }

    /// The `agent.*` primitive (Spec E01): [`Agent::act`] /
    /// [`Agent::assert`] steps resolved through the active [`Driver`] at
    /// record time and replayed byte-deterministically from committed
    /// `*.jsonl` recordings otherwise. The default driver is
    /// [`Authored`] — replay-only, zero configuration.
    pub fn agent(&mut self) -> Agent<'_> {
        Agent::new(self)
    }

    /// Inject the [`Driver`] agent steps resolve through at record time
    /// — self-tests plug in a stub here; `AIT_E2E_DRIVER`'s [`Command`]
    /// backing is already installed at spawn, so this replaces it.
    /// Never consulted on the replay path.
    pub fn set_driver(&mut self, driver: impl Driver + 'static) {
        self.driver = Box::new(driver);
    }

    /// Point recordings at `dir` instead of the
    /// `AIT_E2E_RECORDINGS`/`<crate>/tests/recordings` convention — how
    /// self-tests give each case its own writable tree.
    pub fn set_recordings_dir(&mut self, dir: impl Into<PathBuf>) {
        self.recordings_dir = Some(dir.into());
    }

    /// Override record mode explicitly (the default is
    /// `AIT_E2E_RECORD` at spawn) — keeps tests free of environment
    /// races.
    pub fn set_record_mode(&mut self, record: bool) {
        self.record = record;
    }

    /// Whether agent steps re-resolve and rewrite recordings.
    pub fn record_mode(&self) -> bool {
        self.record
    }

    /// The next agent-step index — each `agent().act`/`assert` consumes
    /// one → its `<step>.jsonl` (crate-internal; [`Agent`] owns the
    /// sequence).
    pub(crate) fn next_agent_step(&mut self) -> usize {
        let step = self.agent_step;
        self.agent_step += 1;
        step
    }

    /// The [`Driver`] agent steps resolve through at record time
    /// (crate-internal — replay never consults it).
    pub(crate) fn driver(&mut self) -> &mut dyn Driver {
        &mut *self.driver
    }

    /// The recordings-root override
    /// [`set_recordings_dir`](Self::set_recordings_dir) installed, if any
    /// (crate-internal).
    pub(crate) fn recordings_dir(&self) -> Option<&Path> {
        self.recordings_dir.as_deref()
    }

    /// The spawned program's name — recording provenance
    /// (crate-internal).
    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    /// The current screen as a serializable [`ScreenState`] — what a
    /// [`Driver`] is handed and what the recording guard hashes.
    pub fn screen_state(&self) -> ScreenState {
        ScreenState::capture(self)
    }

    /// Press a named chord (`"Alt+T"`, `"Ctrl+PageDown"`, `"F5"`, …),
    /// encoded the way a real legacy terminal would send it — see
    /// [`parse_chord`].
    ///
    /// # Panics
    /// On an unparseable chord or a failed PTY write.
    pub fn press(&self, chord: &str) {
        self.log_action(format!("press({chord:?})"));
        let event = parse_chord(chord).unwrap_or_else(|e| panic!("press({chord:?}): {e}"));
        self.session
            .send_key(event)
            .unwrap_or_else(|e| panic!("press({chord:?}): PTY write failed: {e}"));
        self.pace();
    }

    /// Type literal text: its UTF-8 bytes to the program's stdin,
    /// exactly as written (`\n` stays `\n` — use [`press`](Self::press)
    /// `"Enter"` for the `\r` terminals send).
    ///
    /// # Panics
    /// On a failed PTY write.
    pub fn type_text(&self, text: &str) {
        self.log_action(format!("type_text({text:?})"));
        self.write("type_text", text.as_bytes());
    }

    /// The raw-bytes escape hatch — anything [`press`](Self::press)
    /// can't express (bracketed paste, exotic sequences).
    ///
    /// # Panics
    /// On a failed PTY write.
    pub fn send_bytes(&self, bytes: &[u8]) {
        self.log_action(evidence::describe_bytes(bytes));
        self.write("send_bytes", bytes);
    }

    /// The PTY write behind every input verb (each verb logs its own
    /// action first, so the write itself is shared); `verb` names the
    /// caller so a write failure names the actual verb.
    fn write(&self, verb: &str, bytes: &[u8]) {
        self.session
            .send_bytes(bytes)
            .unwrap_or_else(|e| panic!("{verb}: PTY write failed: {e}"));
        self.pace();
    }

    /// Watch pacing: the fixed post-input delay when `slowmo` is set —
    /// presentation only; waits stay predicate-based (a slowmo run
    /// observes exactly the same sequence, just slower).
    fn pace(&self) {
        if !self.slowmo.is_zero() {
            std::thread::sleep(self.slowmo);
        }
    }

    /// Send one SGR (1006) mouse event at the 1-based screen cell
    /// `(column, row)` — the coordinates the wire itself uses.
    ///
    /// # Panics
    /// On a failed PTY write.
    pub fn send_mouse(&self, event: MouseEvent, column: u16, row: u16) {
        self.log_action(format!("send_mouse({event:?} @{column},{row})"));
        self.write("send_mouse", &mouse::encode(event, column, row));
    }

    /// Resize the PTY winsize and the grid.
    ///
    /// # Panics
    /// On a failed winsize write (a just-dead child).
    pub fn resize(&mut self, size: Size) {
        self.log_action(format!("resize({}x{})", size.columns, size.lines));
        self.session.resize(size).expect("resize: PTY write failed");
        self.pace();
    }

    /// Wait — bounded by [`WAIT_TIMEOUT`] — until `pred` accepts the
    /// current screen. The session is drained each poll, so terminal
    /// replies (device-attributes answers, …) keep flowing.
    ///
    /// # Panics
    /// On timeout (with the final screen text), or early when the
    /// program exits while the predicate still fails.
    pub fn wait_for_screen(&mut self, description: &str, mut pred: impl FnMut(&Self) -> bool) {
        self.log_action(format!("wait_for_screen({description:?})"));
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            self.pump();
            if pred(self) {
                return;
            }
            if let Some(status) = self.exited.clone() {
                // The exit event can fire while the reader thread still
                // holds the child's final output — a command that prints
                // and dies instantly would otherwise flake the wait on a
                // grid that is one drain behind. Give the reader a short
                // grace window to land the tail before failing.
                let grace = Instant::now() + Duration::from_millis(500);
                while !pred(self) && Instant::now() < grace {
                    self.pump();
                    std::thread::sleep(WAIT_POLL);
                }
                if !pred(self) {
                    panic!(
                        "waiting for {description}: the program exited ({status:?})\n\
                         --- screen ---\n{}",
                        self.screen_text()
                    );
                }
                return;
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
        self.log_action("wait_for_exit".to_string());
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

    /// One drain round: feed queued PTY output into the grid, record
    /// the exit status when `Exited` surfaces and collect OSC 52
    /// clipboard writes as they arrive.
    pub(crate) fn pump(&mut self) {
        for event in self.session.drain_events() {
            match event {
                LiveEvent::Exited(status) => self.exited = Some(status),
                LiveEvent::Osc(SessionEvent::ClipboardWrite { contents, .. }) => {
                    self.clipboard.push(contents)
                }
                _ => {}
            }
        }
    }

    /// Every `SessionEvent::ClipboardWrite` the program has emitted so
    /// far (decoded contents), oldest first — what the scenario
    /// `expect_clipboard` verb and code-first clipboard assertions
    /// read. The harness's session opts into `SessionOptions::osc52`,
    /// which is what makes a spawned app's own OSC 52 copy arrive here.
    pub fn clipboard_writes(&self) -> &[String] {
        &self.clipboard
    }

    /// The visible screen as text: every viewport row, trimmed,
    /// `\n`-joined — scrollback excluded.
    pub fn screen_text(&self) -> String {
        self.session
            .grid()
            .visible_rows()
            .map(|row| row.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The screen text ready for an `insta` text snapshot (Spec E01's
    /// `snapshot` verb): [`screen_text`](Self::screen_text) with the
    /// spawn's fresh temp cwd redacted to `[cwd]` — the same redaction
    /// [`ScreenState::capture`] applies, so snapshots stay
    /// machine-independent without each test redacting by hand.
    pub fn snapshot(&self) -> String {
        self.screen_state().rows.join("\n")
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

    /// The tail of raw PTY output (Spec E01, ticket E01-T5): the most
    /// recent bytes the session consumed — up to `LiveSession`'s 64 KiB
    /// bound — escapes and all. See [`LiveSession::output_tail`] for
    /// the exact "consumed" semantics.
    pub fn pty_tail(&self) -> &[u8] {
        self.session.output_tail()
    }

    /// Write the failure-evidence dump (Spec E01): `screen.txt` (final
    /// screen text), `pty-tail.bin` (the raw PTY tail) and
    /// `actions.log` (the ordered action log with screen-state hashes)
    /// under [`dump_root`](crate::dump_root)`/<test_name>/`; returns
    /// the dump directory.
    ///
    /// Normally nobody calls this — a panic while an `E2e` is alive
    /// dumps automatically (see `Drop`), naming the dir after the
    /// test's thread. Call it directly when there is no test-thread
    /// name to derive one from — e.g. the scenario runner (E01-T4)
    /// dumps once per failing scenario with the scenario's name.
    pub fn dump_evidence(&self, test_name: &str) -> io::Result<PathBuf> {
        evidence::write_dump(
            test_name,
            &self.screen_text(),
            self.pty_tail(),
            &self.log.borrow(),
        )
    }
}

impl Drop for E2e {
    /// On unwind — any failed step, whether a harness wait/assert or a
    /// test-side `assert!` — dump the failure evidence before the panic
    /// propagates (Spec E01: "a failed step leaves dumps"). The dump is
    /// best-effort: I/O failure during a panic must not abort.
    fn drop(&mut self) {
        if std::thread::panicking() {
            let name = evidence::current_test_name();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = self.dump_evidence(&name);
            }));
        }
        // A headed run left the program's mode-enables on the host
        // terminal — leave-alt-screen, mouse reporting, bracketed
        // paste, kitty flags — unless the child shut down cleanly and
        // emitted its own reset. Restore it: a stuck ?1002/?2004 makes
        // scrolls and selections echo escape codes in the shell.
        if self.headed {
            restore_tty();
        }
    }
}

/// Truthy `AIT_E2E_*` env-var convention shared by the harness's flags:
/// unset, empty, `0` or `false` (any case) is off; anything else is on.
pub(crate) fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}

/// The headed-mode mirror target: the controlling terminal (`/dev/tty`)
/// when one is available — so watching works even with `cargo test`'s
/// captured output — falling back to stderr otherwise.
fn headed_writer() -> Box<dyn io::Write + Send> {
    match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        Ok(tty) => Box::new(tty),
        Err(_) => Box::new(io::stderr()),
    }
}

/// The watch pacing for a spawn: `AIT_E2E_SLOWMO=<ms>` overrides
/// `SpawnConfig::slowmo_ms` (it's the runtime knob a watcher turns).
fn slowmo(config_ms: u64) -> Duration {
    match std::env::var("AIT_E2E_SLOWMO") {
        Ok(value) => Duration::from_millis(value.parse().unwrap_or(0)),
        Err(_) => Duration::from_millis(config_ms),
    }
}

/// Best-effort restore of the host terminal's modes after a headed
/// run: the mirrored bytes enabled alt-screen, mouse reporting
/// (1002/1003/1006), bracketed paste and kitty-keyboard flags on
/// `/dev/tty`, and a child that didn't shut down cleanly leaves them
/// set — after which every scroll/selection echoes escape codes into
/// the shell. Written to a fresh `/dev/tty` handle so it can't depend
/// on a mirror still being attached.
fn restore_tty() {
    const RESTORE: &[u8] = b"\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[<u\x1b[?1049l\x1b[0m";
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(RESTORE);
        let _ = tty.flush();
    }
}
