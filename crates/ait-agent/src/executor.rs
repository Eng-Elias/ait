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
//! - **`MarksOnly`/`Absent`** (R06-T5) — typed-fallback tiers: the
//!   command is typed at the prompt via
//!   [`ShellBridge::send_bytes`], completion is detected from the
//!   `133;D` mark (output = grid rows between B and D, exit = D's
//!   payload — `None` reads as "unknown", never a guess, us-15) or
//!   from a sentinel scanned out of [`ShellBridge::output_tail`].
//!   The bridge methods those tiers drive exist now; until they
//!   land, a non-`Integrated` state is a clean tool error — the
//!   agent degrades instead of typing noise into a bare shell.
//!
//! Cancellation rides the executor's own [`CancellationToken`] — the
//! same token the session's Esc path cancels — so a `run` parked on a
//! slow command still stops promptly.

use std::fmt;
use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use ait_shell::RunResult;
use tokio_util::sync::CancellationToken;

/// How often a pending in-band run wakes to check the cancellation
/// token — the `await_verdict` gate poll's precedent: the wait is a
/// poll only so Esc can answer it.
const RUN_POLL: Duration = Duration::from_millis(100);

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
        /// The scrollback-then-viewport line the mark was recorded
        /// at — the typed tier's "output = grid rows between B and
        /// D" coordinate (resolved via [`ShellBridge::grid_lines`]).
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

    /// Grid text for rows `start..end` in `GridView::line`
    /// coordinates (`0..` = viewport top, negatives reach into
    /// scrollback) — the `MarksOnly` tier's output-capture surface
    /// (R06-T5). Rows past either edge are simply absent from the
    /// result; a mark-recorded line converts with the scrollback
    /// offset the frontend tracks.
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
    /// the typed-fallback tiers for `MarksOnly`/`Absent` land in
    /// R06-T5.
    Unsupported(ShellIntegration),
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
                "the session's shell integration is {state:?} — only an Integrated shell can run commands yet"
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
/// Round 6 ships the `Integrated` tier — §5.5.1 in-band `__ait_run`
/// through [`ShellBridge::run_in_band`]; `MarksOnly`/`Absent` type
/// the command in R06-T5.
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
}

impl UserShellExecutor {
    /// An executor driving `bridge`, cancelled by `cancel` — the
    /// session hands it the same token its Esc path fires.
    pub fn new(bridge: Box<dyn ShellBridge>, cancel: CancellationToken) -> Self {
        Self { bridge, cancel }
    }
}

impl CommandExecutor for UserShellExecutor {
    /// Run `command` in the user's live shell. The `Integrated` tier
    /// asks the bridge for the in-band run — the request line's typed
    /// trace stays in the grid — and parks on the result channel at
    /// the [`RUN_POLL`] cadence so Esc still answers mid-command. A
    /// non-`Integrated` state refuses before any bytes are offered
    /// (R06-T5's typed tiers slot into the `match` arm this guard
    /// currently is).
    fn run(&mut self, command: &str) -> Result<CommandResult, ExecutorError> {
        if self.cancel.is_cancelled() {
            return Err(ExecutorError::Cancelled);
        }
        match self.bridge.integration_state()? {
            ShellIntegration::Integrated => {}
            other => return Err(ExecutorError::Unsupported(other)),
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
