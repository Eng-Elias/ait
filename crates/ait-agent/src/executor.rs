//! The `CommandExecutor` seam (REQUIREMENTS.md §5.9, Spec R06 /
//! issue #83, ticket R06-T4): one operation — run a command, get back
//! `{merged output, exit code | unknown}` — behind which every §5.9
//! tier sits. The signature carries nothing user-shell-specific
//! (CODING_STANDARDS.md §2): Round 9's ControlMaster, remote-daemon
//! and no-op tiers do their own I/O and drop in without a bridge —
//! the compile-checked sketch lives in `tests/executor.rs`.
//!
//! Round 6 ships [`UserShellExecutor`], which runs commands in the
//! *user's own live shell* — her env, aliases, functions and cwd —
//! through a [`ShellBridge`]: the narrow seam the `ait` frontend
//! implements per `Tab` (Spec R06: the TUI renders cards, gathers
//! input and implements the shell bridge — all policy stays in this
//! crate, standard 9). The bridge is a channel proxy serviced by the
//! frontend's drain each tick, so every method is a request the main
//! loop answers while the agent worker waits on its own thread.
//!
//! `UserShellExecutor` is tiered on the session's *live* integration
//! state — read per call, never cached, because an `exit_shell` hook
//! can downgrade a tab mid-task:
//!
//! - **`Integrated`** (this ticket) — the §5.5.1 in-band path:
//!   [`ShellBridge::run_in_band`] delegates to `Tab::run_in_shell` /
//!   `InBandExec` (Spec R05's declared first consumer). The typed
//!   `__ait_run <id> <hex>` line stays visible in the user's grid —
//!   the trace guarantee every agent action inherits (us-23) — while
//!   the `7701` response frames resolve invisibly into the merged
//!   output + exact exit code.
//! - **`MarksOnly`** (R06-T5, us-13) — the command is typed at the
//!   prompt via [`ShellBridge::send_bytes`]; completion is the next
//!   `OSC 133;D` mark on the [`subscribe`](ShellBridge::subscribe)
//!   feed. Output is the grid rows between the `B` and `D` marks —
//!   half-open: `D` lands on the *next* prompt's row — resolved
//!   through [`ShellBridge::grid_lines`]; the exit code is `D`'s
//!   payload, and a payload-free `D` reports "unknown" — never a
//!   guess (us-15).
//! - **`Absent`** (R06-T5, us-14) — the command is typed wrapped in a
//!   sentinel: `cmd; printf '__AIT_END_<id>=%d\n' $?`. Completion and
//!   the exit code are scanned out of [`ShellBridge::output_tail`];
//!   the output is the tail between the echoed type-in line and the
//!   sentinel's own line (raw bytes — escapes may survive; the grid
//!   holds the clean trace).
//!
//! Both typed tiers leave everything they type visible in the user's
//! grid — the trace guarantee (us-16) — and both are *best effort
//! against the user's own typing*: a mark or a sentinel line the user
//! produced is indistinguishable from the run's own (risk §8.2, the
//! race `__ait_run` already accepts invisibly). Torn marks — a `D`
//! with no `B` before it — and a sentinel that never prints are
//! bounded failures ([`ExecutorError::TypedTorn`] /
//! [`ExecutorError::TypedTimedOut`]), never hangs, panics or guessed
//! results.
//!
//! Cancellation rides the executor's own [`CancellationToken`] — the
//! same token the session's Esc path cancels — so a `run` parked on a
//! slow command still stops promptly.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ait_shell::RunResult;
use tokio_util::sync::CancellationToken;

/// How often a pending in-band run wakes to check the cancellation
/// token — the `await_verdict` gate poll's precedent: the wait is a
/// poll only so Esc can answer it.
const RUN_POLL: Duration = Duration::from_millis(100);

/// Bound on a typed tier's wait for its completion signal, measured
/// from the last sign of life — a fresh mark, a `7700`-series event,
/// output the tail gained. Long enough that a legitimately quiet
/// command is unlikely to outlive it, finite so a shell that can
/// never answer fails instead of hanging (risk §8.2's "bounded
/// failure, never a hang"). Embedders and tests that need a shorter
/// wait pass [`UserShellExecutor::with_typed_timeout`].
pub const TYPED_TIMEOUT: Duration = Duration::from_secs(300);

/// The sentinel the `Absent` tier wraps commands in: the shell prints
/// `__AIT_END_<id>=<exit code>` when the command finishes. The
/// `=<digit>` shape is what distinguishes the *result* line from the
/// type-in line's echo, which carries the same marker text verbatim.
const SENTINEL_PREFIX: &str = "__AIT_END_";

/// Sentinel ids, unique per process — a stale `__AIT_END` line in the
/// tail (a previous task's sentinel still in scrollback, the user
/// re-running the printf) can never collide with a live wait's id.
static NEXT_SENTINEL: AtomicU64 = AtomicU64::new(0);

/// How much shell integration the bridged session reports (§5.5) —
/// `UserShellExecutor`'s tier selector. The three names are the
/// spec's; the frontend maps its own `IntegrationState` onto them,
/// so `ait-agent` never depends on the frontend (standard 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellIntegration {
    /// No integration reported — no marks, no hooks.
    Absent,
    /// The shell emits `OSC 133` prompt marks but not our hooks.
    MarksOnly,
    /// The injected snippet is live — `__ait_run`/`7701` work.
    Integrated,
}

/// One observation from the bridged session — the subscription
/// stream [`ShellBridge::subscribe`] yields. The typed-fallback
/// tiers (R06-T5) drive on prompt marks and exits; `7700`-series
/// traffic arrives pre-decoded as [`ShellEvent`]s.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum BridgeEvent {
    /// A decoded ait-shell event — `7700` hooks, `7701` run frames,
    /// reserved codes — exactly as `ait_shell::decode` produced it.
    Shell(ait_shell::ShellEvent),
    /// An `OSC 133` prompt mark — the `MarksOnly` tier's completion
    /// signal (`D` ends a command; `payload` is the mark's
    /// `;`-joined rest — `133;D;0` → `Some("0")`).
    PromptMark {
        /// The mark's `OSC 133` letter: `'A'`, `'B'`, `'C'` or `'D'`.
        kind: char,
        /// Everything after the kind, when the mark carried any.
        payload: Option<String>,
        /// The mark's **absolute** line — the record-time
        /// scrollback-then-viewport index plus the lines dropped
        /// since the session started (the block assembler's own
        /// coordinate space): invariant under scrolling and eviction,
        /// so a `B` recorded before a long scroll still names its
        /// row. Feed it to [`ShellBridge::grid_lines`], which does
        /// the live-grid conversion the frontend tracks.
        line: usize,
    },
    /// The session's child exited — every pending run will never
    /// answer, and no future one can be typed.
    Exited,
}

/// Why a bridge call failed. Every variant is a clean tool error the
/// model can report — the bridge never wedges the worker.
#[derive(Debug)]
#[non_exhaustive]
pub enum BridgeError {
    /// The session's integration state can't serve the operation —
    /// `run_in_band` on a non-`Integrated` session is refused before
    /// a single byte is written.
    NotIntegrated(ShellIntegration),
    /// The session already exited: a typed request could never be
    /// answered, so nothing was written.
    SessionExited,
    /// The frontend's end of the bridge is gone — the tab closed or
    /// the app is shutting down.
    Gone,
    /// Writing bytes to the session's PTY failed.
    Write(io::Error),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotIntegrated(state) => write!(
                f,
                "cannot run in shell: the session's integration state is {state:?}, not Integrated"
            ),
            Self::SessionExited => write!(f, "cannot run in shell: the session has exited"),
            Self::Gone => write!(f, "cannot run in shell: the tab's session is gone"),
            Self::Write(error) => write!(f, "cannot run in shell: PTY write failed: {error}"),
        }
    }
}

impl std::error::Error for BridgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Write(error) => Some(error),
            _ => None,
        }
    }
}

/// The narrow seam the `ait` frontend implements per `Tab` (Spec R06,
/// §5.5): the only surface `UserShellExecutor` reaches the user's
/// shell through. Every method is non-blocking on the frontend side
/// — requests are serviced by the tab's drain on the next app tick —
/// and every failure mode is a [`BridgeError`], never a wedge.
///
/// `run_in_band` is the only method the `Integrated` tier consumes;
/// `send_bytes`, `subscribe`, `grid_lines` and `output_tail` are the
/// typed-fallback tiers' (R06-T5) surface, and `output_tail`/`grid_lines`
/// are also what a block-capture consumer reads.
pub trait ShellBridge: Send {
    /// The session's live integration state — the tier selector.
    /// Read per call, never cached: a vetted `exit_shell` hook can
    /// downgrade a tab to `Absent` mid-task.
    fn integration_state(&self) -> Result<ShellIntegration, BridgeError>;

    /// §5.5.1 in-band execution: ask the live shell to run `command`
    /// — the shell's env, aliases, functions and cwd — and return the
    /// channel its [`RunResult`] arrives on. The implementation types
    /// `__ait_run <id> <hex(command)>` at the prompt (the line stays
    /// visible — the trace guarantee, us-23) and the `7701` response
    /// frames resolve the result through the shared drain.
    ///
    /// Refuses with [`BridgeError::NotIntegrated`] or
    /// [`BridgeError::SessionExited`] BEFORE a single byte is written
    /// — a bare shell would run `__ait_run` as a bogus command and
    /// turn the request into prompt noise.
    fn run_in_band(&self, command: &[u8]) -> Result<Receiver<RunResult>, BridgeError>;

    /// Write raw bytes to the session's PTY input — the typed
    /// fallback tiers' (R06-T5) only way in. The `Integrated` tier
    /// writes through [`run_in_band`](Self::run_in_band), never here.
    fn send_bytes(&self, bytes: &[u8]) -> Result<(), BridgeError>;

    /// Subscribe to the session's decoded-event stream from this
    /// point on: `7700`-series events, `OSC 133` prompt marks and the
    /// exit — the typed tiers' (R06-T5) completion-detection feed.
    /// `7701` frames flow to `run_in_band` waiters through the shared
    /// accumulator; subscribers see them too.
    fn subscribe(&self) -> Result<Receiver<BridgeEvent>, BridgeError>;

    /// Grid text for rows `start..end` in the **absolute line space**
    /// [`BridgeEvent::PromptMark::line`] reports — the `MarksOnly`
    /// tier's output-capture surface (R06-T5): `grid_lines(b, d)`
    /// reads the rows between the marks, D's own prompt row excluded
    /// by the half-open range. The frontend converts through the
    /// dropped-line and scrollback offsets it tracks; rows that left
    /// the grid are simply absent from the result.
    fn grid_lines(&self, start: i32, end: i32) -> Result<Vec<String>, BridgeError>;

    /// The session's most recent raw output (the frontend's bounded
    /// output tail — escape sequences included, nothing parsed or
    /// stripped) — the `Absent` tier's sentinel-scan surface
    /// (R06-T5).
    fn output_tail(&self) -> Result<Vec<u8>, BridgeError>;
}

/// One command's outcome — the §5.9 contract: `{merged output, exit
/// code | unknown}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    /// Merged stdout+stderr, exactly as captured.
    pub output: Vec<u8>,
    /// The exit code — `None` when the shell reported completion
    /// without one (a payload-free `133;D`, us-15): never guessed.
    pub exit_code: Option<i32>,
}

/// Why a [`CommandExecutor::run`] failed. Every variant maps to a
/// clean tool result the model can read — an executor error is model
/// input, never a task failure (the `ToolOutput` rule).
#[derive(Debug)]
#[non_exhaustive]
pub enum ExecutorError {
    /// The task has no shell bound to run commands in — a detached
    /// conversation, a headless caller, or a frontend that never
    /// attached its `ShellBridge`.
    Unbound,
    /// The session's integration state can't serve `run_command` —
    /// kept for §5.9 vocabulary completeness; the shipped tiers cover
    /// every `ShellIntegration`.
    Unsupported(ShellIntegration),
    /// A typed tier's completion signal never arrived inside
    /// [`TYPED_TIMEOUT`] of the last sign of life — the `133;D` that
    /// never came or the sentinel that never printed (Spec R06
    /// us-13/14): a bounded failure, never a hang.
    TypedTimedOut,
    /// A typed tier's completion arrived torn — a `133;D` with no
    /// `B` recorded before it — so no trustworthy output span exists.
    /// An error the model reports, never a guessed result (us-15's
    /// "never invent" rule extended to capture).
    TypedTorn,
    /// The shell bridge refused or failed — carries the bridge's own
    /// detail (`NotIntegrated`, `SessionExited`, `Gone`, `Write`).
    Bridge(BridgeError),
    /// The run's result channel closed before answering — the
    /// session died mid-run. Distinct from
    /// [`Bridge(BridgeError::SessionExited)`](Self::Bridge): the
    /// request was already typed, so the command may have partially
    /// run — the tool result says so honestly.
    SessionLost,
    /// The task's cancellation token fired while the run was in
    /// flight — the frontend's Esc path.
    Cancelled,
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unbound => write!(
                f,
                "this task has no live shell bound to run commands in"
            ),
            Self::Unsupported(state) => write!(
                f,
                "the session's shell integration is {state:?} — no executor tier serves it"
            ),
            Self::TypedTimedOut => write!(
                f,
                "the shell never signaled the command's completion — no 133;D mark, no sentinel line — it may still be running"
            ),
            Self::TypedTorn => write!(
                f,
                "the shell's completion marks arrived torn — a 133;D with no 133;B before it — the command's output span is untrustworthy"
            ),
            Self::Bridge(error) => write!(f, "{error}"),
            Self::SessionLost => write!(
                f,
                "the session exited before the command's result arrived — it may have partially run"
            ),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for ExecutorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bridge(error) => Some(error),
            _ => None,
        }
    }
}

impl From<BridgeError> for ExecutorError {
    fn from(error: BridgeError) -> Self {
        Self::Bridge(error)
    }
}

/// The §5.9 seam: one operation — run a command — behind which the
/// executor tiers sit (Spec R06, CODING_STANDARDS.md §2).
/// Implementations do their own I/O however their tier dictates:
/// [`UserShellExecutor`] drives a [`ShellBridge`] into the user's
/// live shell; Round 9's ControlMaster, remote-daemon and no-op
/// tiers own their transports and never see a bridge. The signature
/// is deliberately bare of anything user-shell-specific — `command`
/// is the only input, [`CommandResult`] the only output, and every
/// failure mode lands in [`ExecutorError`] so the caller turns it
/// into a tool result.
///
/// Blocking by design: the call site is the agent worker's
/// synchronous dispatch (the `await_verdict` gate's precedent — the
/// worker's runtime exists for the LLM stream, tool execution is a
/// poll loop). Implementations must honor cancellation through
/// whichever token they were constructed with.
pub trait CommandExecutor: Send {
    /// Run `command` to completion and return its merged output and
    /// exit code. `command` is shell text the *model* produced — it
    /// reaches the shell verbatim through whatever channel the tier
    /// owns; sanitization is the shell's own parsing, as it is for
    /// the user's typed input.
    fn run(&mut self, command: &str) -> Result<CommandResult, ExecutorError>;
}

/// The user-shell executor (Spec R06): `run_command` in the user's
/// own live shell, tiered on the tab's live [`ShellIntegration`].
/// `Integrated` runs §5.5.1 in-band `__ait_run` through
/// [`ShellBridge::run_in_band`]; `MarksOnly` and `Absent` type the
/// command at the prompt — the visible trace (us-16) — and detect
/// completion from `133;D` or the `__AIT_END` sentinel.
///
/// Requests on one session serialize on the wire — an interactive
/// shell runs one prompt line at a time — and tool calls execute
/// sequentially already (us-33), so the executor keeps no pending
/// map of its own: each `run` waits on its own result channel.
pub struct UserShellExecutor {
    /// The frontend's per-tab bridge.
    bridge: Box<dyn ShellBridge>,
    /// The task's cancellation token — the Esc path interrupts a
    /// parked wait, checked on the [`RUN_POLL`] cadence.
    cancel: CancellationToken,
    /// The typed tiers' wait bound — [`TYPED_TIMEOUT`] unless the
    /// caller asked otherwise. Measured from the last sign of life,
    /// not from the type-in: a chatty session stays alive.
    typed_timeout: Duration,
}

impl UserShellExecutor {
    /// An executor driving `bridge`, cancelled by `cancel` — the
    /// session hands it the same token its Esc path fires.
    pub fn new(bridge: Box<dyn ShellBridge>, cancel: CancellationToken) -> Self {
        Self::with_typed_timeout(bridge, cancel, TYPED_TIMEOUT)
    }

    /// An executor whose typed tiers bound their completion wait at
    /// `typed_timeout` of silence — embedders and tests that cannot
    /// wait the default five minutes.
    pub fn with_typed_timeout(
        bridge: Box<dyn ShellBridge>,
        cancel: CancellationToken,
        typed_timeout: Duration,
    ) -> Self {
        Self {
            bridge,
            cancel,
            typed_timeout,
        }
    }

    /// The `MarksOnly` tier (us-13): type the command — it stays in
    /// the user's grid (us-16) — then pair the next `B` and `D`
    /// marks on the subscription feed. Output is the half-open row
    /// span `B..D` read through [`ShellBridge::grid_lines`]; the exit
    /// code is `D`'s payload, `None`/garbage reporting "unknown"
    /// rather than a guess (us-15). A `D` with no `B` before it is
    /// [`TypedTorn`](ExecutorError::TypedTorn); a `D` that never
    /// comes is [`TypedTimedOut`](ExecutorError::TypedTimedOut). A
    /// second `B` re-opens the span at the newer mark — the
    /// `RunAccumulator` tear-and-restart rule applied to marks.
    fn run_marks_only(&self, command: &str) -> Result<CommandResult, ExecutorError> {
        let events = self.bridge.subscribe()?;
        self.bridge.send_bytes(typed_line(command).as_slice())?;
        let mut start: Option<usize> = None;
        let mut deadline = self.fresh_deadline();
        loop {
            if self.cancel.is_cancelled() {
                return Err(ExecutorError::Cancelled);
            }
            match events.recv_timeout(RUN_POLL) {
                Ok(BridgeEvent::PromptMark {
                    kind: 'B', line, ..
                }) => {
                    start = Some(line);
                    deadline = self.fresh_deadline();
                }
                Ok(BridgeEvent::PromptMark {
                    kind: 'D',
                    payload,
                    line,
                }) => {
                    let Some(start) = start else {
                        return Err(ExecutorError::TypedTorn);
                    };
                    return Ok(CommandResult {
                        output: self.mark_span_text(start, line)?,
                        exit_code: payload.as_deref().and_then(|code| code.parse::<i32>().ok()),
                    });
                }
                // The session died mid-run — no `D` is coming.
                Ok(BridgeEvent::Exited) => return Err(ExecutorError::SessionLost),
                // `A`/`C` marks and any decoded `7700`-series
                // traffic are signs of life — the bound slides.
                Ok(_) => deadline = self.fresh_deadline(),
                // A dead feed means the service side is gone — the
                // session can never answer again.
                Err(RecvTimeoutError::Disconnected) => return Err(ExecutorError::SessionLost),
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        return Err(ExecutorError::TypedTimedOut);
                    }
                }
            }
        }
    }

    /// The `Absent` tier (us-14): type the command wrapped in a
    /// sentinel — `cmd; printf '__AIT_END_<id>=%d\n' $?`, the whole
    /// line visible in the grid (us-16) — then scan the raw output
    /// tail for `__AIT_END_<id>=<digits>`. The result line's number
    /// is the exit code; the output is the tail between the echoed
    /// type-in line and the sentinel's own line. A sentinel that
    /// never prints fails at the bound (us-14's "never a hang").
    ///
    /// The id is unique per run, so the whole tail can be scanned
    /// directly: no stale `__AIT_END` line in scrollback — an earlier
    /// task's sentinel, or a printf the user ran herself — can ever
    /// carry a live wait's id.
    fn run_absent(&self, command: &str) -> Result<CommandResult, ExecutorError> {
        let events = self.bridge.subscribe()?;
        let marker = format!("{SENTINEL_PREFIX}{}", next_sentinel_id());
        // `;` after a trailing `;`/blank would be a syntax error in
        // the user's shell — trim so the wrap always parses.
        let command = command.trim_end_matches([';', ' ', '\t', '\n']);
        let typed = format!("{command}; printf '{marker}=%d\\n' $?\n");
        self.bridge.send_bytes(typed.as_bytes())?;

        let mut deadline = self.fresh_deadline();
        // Growth detector: fresh bytes shift the tail, which counts
        // as a sign of life and re-arms the bound.
        let mut last_tail = self.bridge.output_tail()?;
        loop {
            if self.cancel.is_cancelled() {
                return Err(ExecutorError::Cancelled);
            }
            match events.recv_timeout(RUN_POLL) {
                Ok(BridgeEvent::Exited) => return Err(ExecutorError::SessionLost),
                // Marks and `7700`-series traffic are signs of life.
                Ok(_) => deadline = self.fresh_deadline(),
                Err(RecvTimeoutError::Disconnected) => return Err(ExecutorError::SessionLost),
                Err(RecvTimeoutError::Timeout) => {}
            }
            let tail = self.bridge.output_tail()?;
            if tail != last_tail {
                deadline = self.fresh_deadline();
                last_tail = tail.clone();
            }
            if let Some(result) = sentinel_result(&tail, &marker) {
                return Ok(CommandResult {
                    output: absent_output(&tail, result.line_start, &marker),
                    exit_code: Some(result.exit_code),
                });
            }
            if Instant::now() >= deadline {
                return Err(ExecutorError::TypedTimedOut);
            }
        }
    }

    /// `now + typed_timeout` — the completion wait's sliding bound.
    fn fresh_deadline(&self) -> Instant {
        Instant::now() + self.typed_timeout
    }

    /// The text of the rows a `B…D` span covers — `grid_lines` in
    /// the marks' absolute line space, joined with `'\n'` after each
    /// row's grid-padding blanks are trimmed (the block copy's own
    /// rule). A `D` on or before `B`'s row yields no rows — the empty
    /// half-open range — which the half-open read reports as empty
    /// output rather than inventing lines.
    fn mark_span_text(&self, start: usize, end: usize) -> Result<Vec<u8>, ExecutorError> {
        let start = i32::try_from(start).unwrap_or(i32::MAX);
        let end = i32::try_from(end).unwrap_or(i32::MAX);
        let rows = self.bridge.grid_lines(start, end)?;
        let mut output = Vec::new();
        for row in rows {
            if !output.is_empty() {
                output.push(b'\n');
            }
            output.extend_from_slice(row.trim_end().as_bytes());
        }
        Ok(output)
    }
}

impl CommandExecutor for UserShellExecutor {
    /// Run `command` in the user's live shell, on whichever tier the
    /// tab's *live* state serves — read per call, never cached: a
    /// vetted `exit_shell` hook can downgrade `Integrated` to
    /// `Absent` mid-task. `Integrated` parks on the in-band result
    /// channel; `MarksOnly`/`Absent` type the command and wait on
    /// their completion signals. Every wait polls at the
    /// [`RUN_POLL`] cadence so Esc still answers mid-command.
    fn run(&mut self, command: &str) -> Result<CommandResult, ExecutorError> {
        if self.cancel.is_cancelled() {
            return Err(ExecutorError::Cancelled);
        }
        match self.bridge.integration_state()? {
            ShellIntegration::Integrated => {}
            ShellIntegration::MarksOnly => return self.run_marks_only(command),
            ShellIntegration::Absent => return self.run_absent(command),
        }
        let result = self.bridge.run_in_band(command.as_bytes())?;
        loop {
            if self.cancel.is_cancelled() {
                return Err(ExecutorError::Cancelled);
            }
            match result.recv_timeout(RUN_POLL) {
                Ok(done) => {
                    return Ok(CommandResult {
                        output: done.output,
                        exit_code: Some(done.exit_code),
                    })
                }
                Err(RecvTimeoutError::Timeout) => continue,
                // The sender set lives in the frontend's pending map;
                // its death means the session is gone or exited —
                // the request was typed, so the command may have
                // partially run (us-34's honest version).
                Err(RecvTimeoutError::Disconnected) => return Err(ExecutorError::SessionLost),
            }
        }
    }
}

/// The line a typed tier writes: `command` plus the Enter that runs
/// it — verbatim, the same trust the user's own typing gets (`run`'s
/// contract: sanitization is the shell's parsing).
fn typed_line(command: &str) -> Vec<u8> {
    let mut typed = command.as_bytes().to_vec();
    typed.push(b'\n');
    typed
}

/// A sentinel id unique per run — process id plus a global sequence
/// so a live wait's marker never collides with a stale `__AIT_END`
/// line the scrollback still holds (an earlier task's sentinel, or a
/// printf the user ran herself).
fn next_sentinel_id() -> String {
    let seq = NEXT_SENTINEL.fetch_add(1, Ordering::Relaxed);
    format!("{}_{seq}", std::process::id())
}

/// Where the sentinel's result line begins in the tail, and what it
/// reported.
struct SentinelResult {
    /// The result line's start offset — the byte after the previous
    /// line break (the sentinel prints on its own line).
    line_start: usize,
    /// The parsed exit code.
    exit_code: i32,
}

/// The sentinel's *result* line in `tail` — `<marker>=` followed by
/// at least one digit (the type-in echo carries `=%d`, never a
/// digit, so it can never match). The last valid match wins, so a
/// repeated result still resolves.
fn sentinel_result(tail: &[u8], marker: &str) -> Option<SentinelResult> {
    let marker = marker.as_bytes();
    let mut found = None;
    let mut offset = 0usize;
    let mut search = tail;
    while let Some(at) = find_subslice(search, marker) {
        let after = &search[at + marker.len()..];
        if let Some(rest) = after.strip_prefix(b"=") {
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            let parsed = std::str::from_utf8(&rest[..digits])
                .ok()
                .and_then(|code| code.parse::<i32>().ok());
            if digits > 0 {
                if let Some(exit_code) = parsed {
                    let line_start = search[..at]
                        .iter()
                        .rposition(|b| *b == b'\n')
                        .map(|p| offset + p + 1)
                        .unwrap_or(offset);
                    found = Some(SentinelResult {
                        line_start,
                        exit_code,
                    });
                }
            }
        }
        offset += at + marker.len();
        search = &search[at + marker.len()..];
    }
    found
}

/// The `Absent` run's output: the tail's bytes before the sentinel's
/// line, minus the echoed type-in line — identified by the *first*
/// occurrence of the marker text itself, which the type-in echo is
/// guaranteed to carry (`$?` prints its digits only in the result;
/// the id is unique to this run). If the echo got mangled past
/// recognition — a redrawing prompt can interleave escapes — the
/// whole region stands: noisy, but never invented. Exactly one
/// trailing line break is dropped — it separated the output from
/// the result line.
fn absent_output(tail: &[u8], line_start: usize, marker: &str) -> Vec<u8> {
    let region = &tail[..line_start];
    let start = find_subslice(region, marker.as_bytes())
        .and_then(|at| {
            region[at + marker.len()..]
                .iter()
                .position(|b| *b == b'\n')
                .map(|p| at + marker.len() + p + 1)
        })
        .unwrap_or(0);
    let mut output = region[start..].to_vec();
    if output.ends_with(b"\r\n") {
        output.truncate(output.len() - 2);
    } else if matches!(output.last(), Some(b'\n') | Some(b'\r')) {
        output.pop();
    }
    output
}

/// `needle`'s first position in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The §5.9 no-op tier, shipped: a task bound to no live shell —
/// a detached conversation, a test, a frontend that never attached
/// its bridge — answers a clean [`ExecutorError::Unbound`] the
/// caller renders as a tool result rather than refusing to exist.
pub struct DetachedExecutor;

impl CommandExecutor for DetachedExecutor {
    fn run(&mut self, _command: &str) -> Result<CommandResult, ExecutorError> {
        Err(ExecutorError::Unbound)
    }
}
