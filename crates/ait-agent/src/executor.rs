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
//!   prompt via [`ShellBridge::send_bytes`]; completion is the first
//!   `OSC 133;D` mark after type-in on the
//!   [`subscribe`](ShellBridge::subscribe) feed. Real shells emit
//!   their marks in precmd order `D,A,B` (ait-shell's rc snippets) —
//!   the `B` that opened this prompt landed at prompt draw, before
//!   the subscription existed — so the span's start is pinned on the
//!   cursor's absolute line at send time
//!   ([`ShellBridge::cursor_line`]) and re-opened by any `B`/`C`
//!   that does arrive post-typing. Output is the grid rows from that
//!   anchor to `D`'s row — half-open: `D` lands on the *next*
//!   prompt's row — resolved through [`ShellBridge::grid_lines`];
//!   the exit code is `D`'s payload, and a payload-free `D` reports
//!   "unknown" — never a guess (us-15).
//! - **`Absent`** (R06-T5, us-14; §5.14 shell-aware since R16) — the
//!   command is typed wrapped in a sentinel spelled for the session's
//!   shell family: `cmd; printf '__AIT_END_<id>=%d\n' $?` on POSIX
//!   shells, `Write-Output`/`$LASTEXITCODE` on PowerShell,
//!   `call echo __AIT_END_<id>=%^ERRORLEVEL%` on cmd (remote sessions
//!   always take the POSIX
//!   form — the remote shell is the POSIX bootstrap). Completion and
//!   the exit code are scanned out of [`ShellBridge::output_tail`];
//!   the output is the tail between the echoed type-in line and the
//!   sentinel's own line (raw bytes — escapes may survive; the grid
//!   holds the clean trace).
//!
//! Both typed tiers leave everything they type visible in the user's
//! grid — the trace guarantee (us-16) — and both are *best effort
//! against the user's own typing*: a mark or a sentinel line the user
//! produced is indistinguishable from the run's own (risk §8.2, the
//! race `__ait_run` already accepts invisibly). A completion signal
//! that never arrives — a `D` the shell never emits or a sentinel
//! that never prints — is a bounded failure
//! ([`ExecutorError::TypedTimedOut`]), never a hang, panic or
//! guessed result.
//!
//! Cancellation rides the executor's own [`CancellationToken`] — the
//! same token the session's Esc path cancels — so a `run` parked on a
//! slow command still stops promptly.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ait_shell::{RunResult, ShellFamily};
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

/// Where the bridged session's commands actually run (Spec R09 §5.9):
/// the frontend's per-tab remote-context tracking distilled for the
/// executor tiers. `Remote` carries everything a transport needs —
/// the destination token and the ControlPath the `ssh()` wrapper
/// negotiated — because the executor owns no SSH state of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoteState {
    /// A plain local session — every tier targets the user's machine.
    Local,
    /// The tab's user is inside an integrated `ssh` session: `ssh()`
    /// emitted a vetted `ssh_begin`. Read per call — `ssh_end` or a
    /// remote `exit_shell` flips it back mid-task.
    Remote {
        /// The destination token the user typed (`user@host`,
        /// `host`, config alias) — ssh's `-O check`/command
        /// destination.
        target: String,
        /// The ControlPath pattern the attach negotiated
        /// (`ssh -G`-resolved, `%C`-tokened) — `None` when the
        /// connection carries no multiplexing at all.
        control_path: Option<String>,
        /// `true` once the REMOTE shell proved its own integration —
        /// a remote `init_shell`/lifecycle hook landed through the
        /// channel — NOT the tab's local state. `false` means the
        /// remote shell never integrated (no bootstrap sent, or a
        /// degraded plain `-l` shell): in-band `__ait_run` can never
        /// answer there, so [`UserShellExecutor`] drives the typed
        /// tiers instead (Spec R09 us-18).
        bootstrapped: bool,
    },
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

    /// The cursor's row in the same **absolute line space**
    /// [`BridgeEvent::PromptMark::line`] reports — the `MarksOnly`
    /// tier's send-time anchor (R06-T5): real shells emit their marks
    /// in precmd order `D,A,B`, so the `B` that opened this prompt
    /// landed before the tier subscribed and the run's start row is
    /// pinned here instead, before typing.
    fn cursor_line(&self) -> Result<usize, BridgeError>;

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

    /// Where commands land (Spec R09-T5): the tab's remote context —
    /// `Local` for every session that never entered an integrated
    /// `ssh`. The default answers `Local`, so bridges that predate the
    /// remote tier (and every scripted test bridge) need no change;
    /// the frontend's per-tab implementation tracks the real state.
    fn remote_state(&self) -> Result<RemoteState, BridgeError> {
        Ok(RemoteState::Local)
    }

    /// The session program's basename — the same string the injection
    /// planner matched and the §5.10.4 classifier's builtin table keys
    /// on (`bash`, `pwsh.exe`, `cmd.exe`). The `Absent` tier renders
    /// its completion sentinel in this name's
    /// [`ait_shell::ShellFamily`] (§5.14): PowerShell's `Write-Output`,
    /// cmd's `call echo`, the POSIX `printf` for everything else. The
    /// default answers `""` — bridges that predate the shell-aware
    /// sentinel (and every scripted test bridge) keep the POSIX form
    /// they were written against.
    fn shell_name(&self) -> Result<String, BridgeError> {
        Ok(String::new())
    }
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
    /// trustworthy output span behind it. Reserved vocabulary: the
    /// shipped `MarksOnly` tier anchors its span on the send-time
    /// cursor line, so a `D` with no `B` in front of it — the normal
    /// precmd ordering — completes rather than tearing.
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
    /// The out-of-band transport itself failed — `ssh` never spawned
    /// or answered `255` (its own error band: a dead socket, a
    /// severed connection, an unreachable host). Distinct from a
    /// remote command's own exit code, which lands in
    /// [`CommandResult::exit_code`]; this is the failure
    /// [`DemotingExecutor`] reads as "the tier is unusable — drop to
    /// the in-band path".
    Transport(String),
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
            Self::Transport(detail) => {
                write!(f, "the remote connection's transport failed: {detail}")
            }
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
    /// the user's grid (us-16) — then take the first `D` after
    /// type-in as completion, its payload as the exit code
    /// (`None`/garbage reporting "unknown" rather than a guess,
    /// us-15). Output is the half-open row span `start..D` read
    /// through [`ShellBridge::grid_lines`].
    ///
    /// The span's `start` CANNOT be a paired `133;B`: real shells
    /// emit their marks in precmd order `D,A,B` (ait-shell's rc
    /// snippets), so the `B` that opened this prompt landed at
    /// prompt draw — before the subscription existed — and the
    /// post-typing feed is `C,D,A,B`. The start is therefore pinned
    /// at send time on the cursor's absolute line
    /// ([`ShellBridge::cursor_line`]) and re-opened by any `B`/`C`
    /// that does arrive post-typing — `C` lands on the output's own
    /// first row, `B` on the command's — the `RunAccumulator`
    /// tear-and-restart rule applied to marks. A `D` with no mark in
    /// front of it is the NORMAL completion, never a torn one; a `D`
    /// that never comes is [`TypedTimedOut`](ExecutorError::TypedTimedOut).
    fn run_marks_only(&self, command: &str) -> Result<CommandResult, ExecutorError> {
        let events = self.bridge.subscribe()?;
        // Pinned BEFORE typing: the anchor a post-typing `B`/`C` may
        // still tighten, and the whole span's start when none arrives.
        let mut start = self.bridge.cursor_line()?;
        self.bridge.send_bytes(typed_line(command).as_slice())?;
        let mut deadline = self.fresh_deadline();
        loop {
            if self.cancel.is_cancelled() {
                return Err(ExecutorError::Cancelled);
            }
            match events.recv_timeout(RUN_POLL) {
                // A `B` or `C` after type-in re-opens the span at the
                // newer mark — `C` is the tighter anchor, landing on
                // the output's own first row.
                Ok(BridgeEvent::PromptMark {
                    kind: 'B' | 'C',
                    line,
                    ..
                }) => {
                    start = line;
                    deadline = self.fresh_deadline();
                }
                // The first `D` after type-in is the completion —
                // the precmd order means it normally arrives with no
                // `B` in front of it.
                Ok(BridgeEvent::PromptMark {
                    kind: 'D',
                    payload,
                    line,
                }) => {
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
    /// sentinel — `cmd; printf '__AIT_END_<id>=%d\n' $?` on POSIX
    /// shells, the §5.14 per-family spellings on Windows — the whole
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
        // The sentinel spells the shell that READS it (§5.14): a
        // remote session reached through ssh() is always the POSIX
        // bootstrap however the local program reports (§5.14's
        // "remote sessions are unchanged"); a local session keys on
        // the tab's own basename — the same resolved name the
        // planner matched — through `ait_shell::shell_family`.
        let family = match self.bridge.remote_state()? {
            RemoteState::Remote { .. } => ShellFamily::Posix,
            RemoteState::Local => ait_shell::shell_family(&self.bridge.shell_name()?),
        };
        let typed = sentinel_line(family, command, &marker);
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

    /// The text of the rows a `start..D` span covers — `grid_lines`
    /// in the marks' absolute line space, joined with `'\n'` after
    /// each row's grid-padding blanks are trimmed (the block copy's
    /// own rule). A `D` on or before `start`'s row yields no rows —
    /// the empty half-open range — which the half-open read reports
    /// as empty output rather than inventing lines.
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
        let mut state = self.bridge.integration_state()?;
        // Spec R09 us-18: a remote session whose shell never
        // confirmed integration answers no `__ait_run` — typing it
        // would park the wait on frames that can never arrive (the
        // tab's `Integrated` state is the LOCAL shell's leftover,
        // not the remote's). Clamp it to `Absent` so the sentinel
        // tier drives the remote shell through the visible channel
        // exactly like an unintegrated local one.
        if state == ShellIntegration::Integrated
            && matches!(
                self.bridge.remote_state()?,
                RemoteState::Remote {
                    bootstrapped: false,
                    ..
                }
            )
        {
            state = ShellIntegration::Absent;
        }
        match state {
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

/// The wrapped line the `Absent` tier types (§5.14): `command` plus
/// the sentinel that prints `<marker>=<exit code>` in `family`'s
/// syntax.
///
/// - POSIX shells keep the original `printf` form.
/// - PowerShell gets `Write-Output` carrying `$LASTEXITCODE` —
///   cleared to `$null` first, so a stale native code can't poison a
///   cmdlet run (cmdlets never set it): the pending native code
///   reports verbatim, else `?` reads 0/1 — the same rule the
///   snippet's `__ait_run` and `prompt` apply (`pwsh.ps1`).
/// - `cmd.exe` gets `call echo` with `^`-escaped `%ERRORLEVEL%` —
///   `call`'s second parse expands it at run time to THIS command's
///   code; a bare `%ERRORLEVEL%` would expand in cmd's parse-time
///   pass and report the *previous* line's errorlevel.
fn sentinel_line(family: ShellFamily, command: &str, marker: &str) -> String {
    match family {
        ShellFamily::PowerShell => format!(
            "$global:LASTEXITCODE = $null; {command}; Write-Output \"{marker}=$(if ($null -ne $global:LASTEXITCODE) {{ [int]$global:LASTEXITCODE }} elseif ($?) {{ 0 }} else {{ 1 }})\"\n"
        ),
        ShellFamily::Cmd => format!("{command} & call echo {marker}=%^ERRORLEVEL%\n"),
        ShellFamily::Posix => format!("{command}; printf '{marker}=%d\\n' $?\n"),
    }
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
/// at least one digit, optionally signed (the type-in echo carries
/// `=%d`/`=$LASTEXITCODE`/`=%^ERRORLEVEL%`, never a digit, so it can
/// never match; `exit -1` under pwsh or a killed process's cmd
/// status reports negative). The last valid match wins, so a
/// repeated result still resolves.
fn sentinel_result(tail: &[u8], marker: &str) -> Option<SentinelResult> {
    let marker = marker.as_bytes();
    let mut found = None;
    let mut offset = 0usize;
    let mut search = tail;
    while let Some(at) = find_subslice(search, marker) {
        let after = &search[at + marker.len()..];
        if let Some(rest) = after.strip_prefix(b"=") {
            let unsigned = rest.strip_prefix(b"-").unwrap_or(rest);
            let digits = unsigned.iter().take_while(|b| b.is_ascii_digit()).count();
            let len = rest.len() - unsigned.len() + digits;
            let parsed = std::str::from_utf8(&rest[..len])
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

/// `ssh`'s own failure band (Spec R09-T5): the client reports
/// transport trouble — a dead control socket, a severed connection —
/// as exit code `255`, and remote commands' own codes stay below it.
/// The overlap is OpenSSH's documented ambiguity (a remote command
/// legitimately exiting `255` reads as transport trouble); the
/// demotion it triggers lands on the in-band tier, never on nothing.
const SSH_TRANSPORT_CODE: i32 = 255;

/// The ControlMaster tier (Spec R09-T5, §5.9): `run_command`
/// multiplexed out-of-band over the SSH control socket the `ssh()`
/// wrapper attached — `ssh -S <sock> -o BatchMode=yes <target> -- <cmd>`
/// — so the agent's commands never share the terminal byte stream the
/// user is typing on. The remote command runs under the remote user's
/// login shell with `2>&1` appended (the `__ait_run` merge's mirror),
/// so [`CommandResult::output`] is the same merged stream every tier
/// reports.
pub struct ControlMasterExecutor {
    /// The ssh client to spawn — `ssh` on PATH in production; the test
    /// seam substitutes a stub that records argv.
    program: String,
    /// The ControlPath the `ssh()` wrapper negotiated — `%C`-tokened
    /// patterns pass verbatim; ssh expands them to the same socket.
    socket: String,
    /// The destination token — remote commands and `-O check` share it.
    target: String,
    /// The task's cancellation token — a parked `ssh` is killed on
    /// the [`RUN_POLL`] cadence like every other tier's wait.
    cancel: CancellationToken,
}

impl ControlMasterExecutor {
    /// An executor multiplexing over `socket` to `target`, spawning
    /// `ssh` from PATH, cancelled by `cancel`.
    pub fn new(socket: String, target: String, cancel: CancellationToken) -> Self {
        Self::with_program(ait_shell::SSH_PROGRAM.to_owned(), socket, target, cancel)
    }

    /// [`new`](Self::new) with the ssh program pinned — the stub seam
    /// the tests drive (`ssh -S`/`-O check` and the command form land
    /// in a recorded argv log instead of a network).
    pub fn with_program(
        program: String,
        socket: String,
        target: String,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            program,
            socket,
            target,
            cancel,
        }
    }
}

impl CommandExecutor for ControlMasterExecutor {
    fn run(&mut self, command: &str) -> Result<CommandResult, ExecutorError> {
        let mut child = Command::new(&self.program)
            .args([
                OsStr::new("-S"),
                OsStr::new(&self.socket),
                OsStr::new("-o"),
                OsStr::new("BatchMode=yes"),
                OsStr::new(&self.target),
                // `--` ends ssh's own option parsing — a command
                // text that starts with `-` can never be read as a
                // client flag (OpenSSH strips it before the remote
                // string is sent).
                OsStr::new("--"),
                OsStr::new(command),
                // The remote shell merges stderr into stdout — the
                // same shape `__ait_run` reports in-band.
                OsStr::new("2>&1"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                ExecutorError::Transport(format!(
                    "could not spawn {} over {}: {error}",
                    self.program, self.socket
                ))
            })?;
        // Both pipes drain on threads — a large output (or a chatty
        // ssh) can never deadlock the poll loop on a full pipe.
        let stdout = child.stdout.take().map(drain_pipe);
        let stderr = child.stderr.take().map(drain_pipe);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if self.cancel.is_cancelled() {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(ExecutorError::Cancelled);
                    }
                    std::thread::sleep(RUN_POLL);
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ExecutorError::Transport(format!(
                        "waiting on the remote command failed: {error}"
                    )));
                }
            }
        };
        let output = stdout.map_or_else(Vec::new, |drain| drain.join().unwrap_or_default());
        let detail = stderr.map_or_else(Vec::new, |drain| drain.join().unwrap_or_default());
        match status.code() {
            Some(code) if code == SSH_TRANSPORT_CODE => Err(ExecutorError::Transport(format!(
                "ssh reported transport failure{}",
                tail_detail(&detail)
            ))),
            code => Ok(CommandResult {
                output,
                exit_code: code,
            }),
        }
    }
}

/// The demotion wrapper (Spec R09-T5): runs `primary` — the
/// ControlMaster tier — until a [`ExecutorError::Transport`] proves
/// the out-of-band channel dead, then answers every later command
/// through `fallback` (the user-shell executor) permanently. A dead
/// socket never recovers, so the task doesn't pay a failing probe
/// per call; non-transport results — remote commands' own exits —
/// never demote. The run that took the transport hit errors out
/// honestly rather than replaying — after partial remote execution
/// a blind re-run is the worse failure (demote, never hang, never
/// double-run).
pub struct DemotingExecutor {
    /// The out-of-band tier, tried first until demotion.
    primary: ControlMasterExecutor,
    /// The in-band path the task drops to — it shares the user's
    /// terminal stream, which is exactly what the remote tier exists
    /// to avoid; demotion keeps it the last resort, not the default.
    fallback: UserShellExecutor,
    /// `true` once a transport failure retired the primary.
    demoted: bool,
}

impl DemotingExecutor {
    /// Pair `primary` with the `fallback` it demotes into.
    pub fn new(primary: ControlMasterExecutor, fallback: UserShellExecutor) -> Self {
        Self {
            primary,
            fallback,
            demoted: false,
        }
    }

    /// Whether the out-of-band tier already failed over — the
    /// observation seam the tier-selection tests assert on.
    pub fn demoted(&self) -> bool {
        self.demoted
    }
}

impl CommandExecutor for DemotingExecutor {
    fn run(&mut self, command: &str) -> Result<CommandResult, ExecutorError> {
        if !self.demoted {
            match self.primary.run(command) {
                // A transport failure means the command may have
                // partially run on the remote host — demote the task
                // permanently but report THIS run's failure honestly
                // rather than replaying it through the fallback (a
                // double-run is the one thing worse than an error:
                // non-idempotent commands execute twice).
                Err(error @ ExecutorError::Transport(_)) => {
                    self.demoted = true;
                    return Err(error);
                }
                result => return result,
            }
        }
        self.fallback.run(command)
    }
}

/// §5.9's tier selection (Spec R09-T5), in preference order:
///
/// 1. The remote-daemon slot is deferred (Backlog R09) — nothing to
///    select this round.
/// 2. A `Remote` remote state with a control path whose socket
///    answers `ssh -S <sock> -O check` → the ControlMaster tier,
///    demoting into the in-band path on transport failure.
/// 3. Any other bridged session → the user-shell executor (in-band
///    `__ait_run` — remote through the bootstrapped shell — else the
///    typed tiers).
/// 4. No bridge at all → the detached no-op tier.
pub fn select_executor(
    bridge: Option<Box<dyn ShellBridge>>,
    cancel: CancellationToken,
) -> Box<dyn CommandExecutor> {
    select_executor_with_program(bridge, cancel, ait_shell::SSH_PROGRAM)
}

/// [`select_executor`] with the ssh client pinned — the test seam:
/// a stub `ssh` that answers `-O check` and records command
/// invocations lets the whole selection matrix run without a
/// network.
pub fn select_executor_with_program(
    bridge: Option<Box<dyn ShellBridge>>,
    cancel: CancellationToken,
    program: &str,
) -> Box<dyn CommandExecutor> {
    let Some(bridge) = bridge else {
        return Box::new(DetachedExecutor);
    };
    if let Ok(RemoteState::Remote {
        target,
        control_path: Some(socket),
        ..
    }) = bridge.remote_state()
    {
        if socket_alive(program, &socket, &target) {
            return Box::new(DemotingExecutor::new(
                ControlMasterExecutor::with_program(
                    program.to_owned(),
                    socket,
                    target,
                    cancel.clone(),
                ),
                UserShellExecutor::new(bridge, cancel),
            ));
        }
    }
    Box::new(UserShellExecutor::new(bridge, cancel))
}

/// Does `program -S <socket> -O check <target>` answer — the live
/// probe `select_executor` pays ONCE per task start (Spec R09-T5:
/// `Remote` + a live socket selects ControlMaster). `BatchMode=yes`
/// keeps a stale socket from prompting. Anything but a clean `0`
/// counts as dead — including a spawn failure, which simply selects
/// the in-band tier.
fn socket_alive(program: &str, socket: &str, target: &str) -> bool {
    Command::new(program)
        .args(["-S", socket, "-o", "BatchMode=yes", "-O", "check", target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Read `pipe` to EOF on its own thread — a full pipe can never
/// deadlock the caller's `try_wait` poll.
fn drain_pipe(mut pipe: impl io::Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

/// The ssh stderr tail for a transport error's detail — trimmed,
/// empty-tolerant.
fn tail_detail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}
