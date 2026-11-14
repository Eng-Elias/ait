//! The `ShellBridge`/`CommandExecutor` seam (Spec R06 testing
//! decisions / issue #87): a *scripted* bridge implementation — no
//! PTY anywhere — that records the bytes offered to it and resolves
//! `run_in_band` waits by feeding hand-built `7701` `A…R…B` frame
//! streams through ait-shell's real [`RunAccumulator`]
//! (`crates/ait/src/inband.rs` tests are the prior art). The typed
//! tiers (R06-T5) are driven by hand-built `OSC 133` mark streams and
//! a scripted raw-output tail carrying the `__AIT_END` sentinel.
//!
//! What these tests prove:
//!
//! - `UserShellExecutor` on an `Integrated` session returns the
//!   command's merged stdout+stderr and exit code (Spec R06 us-12);
//! - `MarksOnly` sessions complete on the first `133;D` after
//!   type-in — the real precmd order `D,A,B` means no `B` precedes
//!   it — and capture the rows from the send-time cursor anchor (or
//!   a post-typing `B`/`C`) to D's row; D's payload is the exit
//!   code — a payload-free D reports "exit unknown" (us-13/15);
//! - `Absent` sessions type the sentinel wrap and scan the raw tail
//!   for `__AIT_END_<id>=<code>` — output and exit code both come
//!   from the sentinel (us-14);
//! - a missing `D`, and a sentinel that never prints, fail
//!   boundedly — never a hang, a panic, or a guessed exit code;
//! - an exited or gone session is a clean error, never a hang
//!   (us-34);
//! - cancellation (Esc) interrupts the wait promptly;
//! - the `CommandExecutor` signature carries nothing
//!   user-shell-specific — §5.9's ControlMaster/daemon/no-op tiers
//!   are sketched against it as compile-checked stubs (us-35).

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ait_agent::{
    select_executor_with_program, BridgeError, BridgeEvent, CommandExecutor, DetachedExecutor,
    ExecutorError, RemoteState, ShellBridge, ShellIntegration, UserShellExecutor,
};
use ait_shell::{RunAccumulator, RunResult, ShellEvent};
use tokio_util::sync::CancellationToken;

/// Bound on every wait — in-memory channels answer far under it; a
/// hang is a failure, not a slow pass.
const BOUND: Duration = Duration::from_secs(10);

/// One complete `A … R×N … B` response stream for `payload` — the
/// events a well-formed `__ait_run` reply decodes to. Chunks at the
/// snippet's 500-byte slice so oversized payloads exercise the
/// multi-frame path (the `inband.rs` tests' `run_stream` precedent).
fn run_stream(payload: &[u8]) -> Vec<ShellEvent> {
    let mut events = vec![ShellEvent::RunBegin];
    for chunk in payload.chunks(500) {
        events.push(ShellEvent::RunChunk {
            payload: chunk.to_vec(),
        });
    }
    events.push(ShellEvent::RunEnd);
    events
}

/// Resolve a `7701` frame `stream` through a real [`RunAccumulator`] —
/// exactly the way `ait`'s `InBandExec` decodes the live traffic.
fn accumulate(stream: Vec<ShellEvent>) -> Option<RunResult> {
    let mut accumulator = RunAccumulator::new();
    let mut result = None;
    for event in stream {
        if let Some(done) = accumulator.feed(&event) {
            result = Some(done);
        }
    }
    result
}

/// The test's view of what a scripted bridge saw — every field shared
/// with the bridge through `Arc` so the bridge can move into the
/// executor while the assertions keep their handle.
#[derive(Clone, Default)]
struct Shares {
    /// Every byte offered to `send_bytes` — the "did the executor
    /// type anything" assertion.
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    /// A waiter per `run_in_band` request, in order — the test sends
    /// each request's [`RunResult`] on its own channel.
    runs: Arc<Mutex<Vec<Sender<RunResult>>>>,
    /// The commands `run_in_band` was asked to execute, in order.
    commands: Arc<Mutex<Vec<Vec<u8>>>>,
    /// The event senders `subscribe` installed.
    subscribers: Arc<Mutex<Vec<Sender<BridgeEvent>>>>,
    /// The scripted cursor row in mark-line coordinates —
    /// `cursor_line` answers it; the `MarksOnly` tier pins it as the
    /// send-time anchor.
    cursor: Arc<Mutex<usize>>,
    /// The scripted grid in mark-line coordinates — `grid_lines`
    /// slices it for the `MarksOnly` tier's capture.
    grid: Arc<Mutex<Vec<String>>>,
    /// The scripted raw-output tail — `output_tail` snapshots it for
    /// the `Absent` tier's sentinel scan.
    tail: Arc<Mutex<Vec<u8>>>,
}

impl Shares {
    /// Answer the newest `run_in_band` request with the stream
    /// `payload`'s frames reassemble to.
    fn respond(&self, payload: &[u8]) {
        let result = accumulate(run_stream(payload)).expect("the scripted stream completes a run");
        let runs = self.runs.lock().expect("runs lock");
        let waiter = runs.last().expect("a pending run_in_band request");
        waiter.send(result).expect("the executor still waits");
    }

    /// The commands `run_in_band` recorded.
    fn commands(&self) -> Vec<Vec<u8>> {
        self.commands.lock().expect("commands lock").clone()
    }

    /// Everything `send_bytes` was offered.
    fn sent(&self) -> Vec<Vec<u8>> {
        self.sent.lock().expect("sent lock").clone()
    }

    /// The newest installed subscription sender — the typed tiers
    /// subscribe before typing, so one arrives inside [`BOUND`].
    fn subscriber(&self) -> Sender<BridgeEvent> {
        let deadline = std::time::Instant::now() + BOUND;
        loop {
            if let Some(sender) = self.subscribers.lock().expect("subs lock").last() {
                return sender.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the executor never subscribed"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The next `send_bytes` — the typed tiers write exactly once
    /// (the trace guarantee, us-16); the sentinel's id rides inside
    /// it for `Absent`-tier responses.
    fn next_typed(&self) -> Vec<u8> {
        let deadline = std::time::Instant::now() + BOUND;
        loop {
            if let Some(bytes) = self.sent.lock().expect("sent lock").last() {
                return bytes.clone();
            }
            assert!(std::time::Instant::now() < deadline, "nothing was typed");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The scripted cursor row — the send-time anchor `cursor_line`
    /// reports.
    fn set_cursor(&self, line: usize) {
        *self.cursor.lock().expect("cursor lock") = line;
    }

    /// Append raw output to the scripted tail — the bytes the
    /// session emitted since the run began (echo included).
    fn emit(&self, bytes: &[u8]) {
        self.tail
            .lock()
            .expect("tail lock")
            .extend_from_slice(bytes);
    }
}

/// The scripted bridge: reports `state`, records every byte and
/// command offered, and hands the test one waiter channel per
/// `run_in_band` request.
struct ScriptedBridge {
    /// What [`integration_state`](ShellBridge::integration_state)
    /// reports — the executor's tier selector reads it per call.
    state: ShellIntegration,
    /// What [`remote_state`](ShellBridge::remote_state) reports —
    /// `Local` by default; the remote-session tests arm a `Remote`.
    remote: Mutex<RemoteState>,
    /// What [`shell_name`](ShellBridge::shell_name) reports — the
    /// session program's basename the `Absent` sentinel keys on;
    /// `""` (POSIX) unless the test names a Windows shell.
    shell: Mutex<String>,
    /// The recorded traffic.
    shares: Shares,
    /// When `Some`, `run_in_band` fails with this error instead of
    /// accepting — the exited/gone-session seams.
    fail_run: Mutex<Option<BridgeError>>,
}

/// A bridge plus the shared state the test asserts against.
fn scripted(state: ShellIntegration) -> (ScriptedBridge, Shares) {
    let shares = Shares::default();
    let bridge = ScriptedBridge {
        state,
        remote: Mutex::new(RemoteState::Local),
        shell: Mutex::new(String::new()),
        shares: shares.clone(),
        fail_run: Mutex::new(None),
    };
    (bridge, shares)
}

impl ShellBridge for ScriptedBridge {
    fn integration_state(&self) -> Result<ShellIntegration, BridgeError> {
        Ok(self.state)
    }

    fn remote_state(&self) -> Result<RemoteState, BridgeError> {
        Ok(self.remote.lock().expect("remote lock").clone())
    }

    fn shell_name(&self) -> Result<String, BridgeError> {
        Ok(self.shell.lock().expect("shell lock").clone())
    }

    fn run_in_band(&self, command: &[u8]) -> Result<Receiver<RunResult>, BridgeError> {
        if let Some(error) = self.fail_run.lock().expect("fail lock").take() {
            return Err(error);
        }
        let (tx, rx) = channel();
        self.shares
            .commands
            .lock()
            .expect("commands lock")
            .push(command.to_vec());
        self.shares.runs.lock().expect("runs lock").push(tx);
        Ok(rx)
    }

    fn send_bytes(&self, bytes: &[u8]) -> Result<(), BridgeError> {
        self.shares
            .sent
            .lock()
            .expect("sent lock")
            .push(bytes.to_vec());
        Ok(())
    }

    fn subscribe(&self) -> Result<Receiver<BridgeEvent>, BridgeError> {
        let (tx, rx) = channel();
        self.shares.subscribers.lock().expect("subs lock").push(tx);
        Ok(rx)
    }

    fn cursor_line(&self) -> Result<usize, BridgeError> {
        Ok(*self.shares.cursor.lock().expect("cursor lock"))
    }

    fn grid_lines(&self, start: i32, end: i32) -> Result<Vec<String>, BridgeError> {
        // Mark-line coordinates — the executor passes the lines its
        // `PromptMark` events carried; the script clips to what the
        // grid holds (absent edges are dropped, like the real grid).
        let grid = self.shares.grid.lock().expect("grid lock");
        let start = usize::try_from(start).unwrap_or(0).min(grid.len());
        let end = usize::try_from(end).unwrap_or(0).min(grid.len()).max(start);
        Ok(grid[start..end].to_vec())
    }

    fn output_tail(&self) -> Result<Vec<u8>, BridgeError> {
        Ok(self.shares.tail.lock().expect("tail lock").clone())
    }
}

/// An `Integrated` run returns the merged output and exit code the
/// `7701` frames carried (Spec R06 us-12) — the executor asked the
/// bridge for the in-band run and nothing else (no raw `send_bytes`).
#[test]
fn integrated_run_returns_merged_output_and_exit_code() {
    let (bridge, shares) = scripted(ShellIntegration::Integrated);
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let responder = std::thread::spawn({
        let shares = shares.clone();
        move || {
            // Give the executor a beat to land its request, then the
            // shell's answer: `A R B` carrying `<id>;<merged>;<code>`.
            std::thread::sleep(Duration::from_millis(20));
            shares.respond(b"0;merged out;with;semicolons\n;7");
        }
    });

    let result = executor.run("echo hi").expect("an Integrated run resolves");
    responder.join().expect("responder");

    assert_eq!(result.output, b"merged out;with;semicolons\n".to_vec());
    assert_eq!(result.exit_code, Some(7));
    assert_eq!(shares.commands(), vec![b"echo hi".to_vec()]);
    assert!(
        shares.sent().is_empty(),
        "the in-band tier writes through run_in_band, never raw bytes"
    );
}

/// An `OSC 133` prompt mark as a subscriber sees it — `line` is the
/// mark's absolute grid line (scrollback-then-viewport index plus
/// dropped rows), the coordinate `grid_lines` resolves.
fn mark(kind: char, payload: Option<&str>, line: usize) -> BridgeEvent {
    BridgeEvent::PromptMark {
        kind,
        payload: payload.map(str::to_owned),
        line,
    }
}

/// The `__AIT_END_<id>` marker the executor wove into the typed
/// sentinel wrap — the `Absent` tier's completion token, fished out
/// of the recorded bytes so the script can print its own result line.
fn sentinel_marker(typed: &[u8]) -> Vec<u8> {
    let text = String::from_utf8(typed.to_vec()).expect("the typed line is text");
    let at = text
        .find("__AIT_END_")
        .expect("the sentinel wrap was typed");
    let rest = &text[at..];
    let end = rest.find('=').expect("the sentinel carries `=`");
    rest.as_bytes()[..end].to_vec()
}

/// A `MarksOnly` run types the command at the prompt — visible in the
/// grid (us-13, us-16) — completes on the first `133;D` after
/// type-in, captures the grid rows from the span's anchor to D's
/// row as output, and reads the exit code from D's payload.
///
/// The script drives the REAL mark order ait-shell's rc snippets
/// produce: `D,A,B` at precmd means this prompt's `B` landed before
/// the subscription existed, and the post-typing feed is
/// `C,D,A,B` — the `C` (output start) re-opens the span the
/// send-time cursor line had pinned.
#[test]
fn marks_only_run_captures_rows_under_the_real_mark_order() {
    let (bridge, shares) = scripted(ShellIntegration::MarksOnly);
    shares
        .grid
        .lock()
        .expect("grid lock")
        .extend((0..30).map(|i| format!("row {i}")));
    // The prompt the command is typed on sits at absolute row 10 —
    // its `B` was emitted before the executor subscribed.
    shares.set_cursor(10);
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let events = shares.subscriber();
            let _typed = shares.next_typed();
            // Real order after Enter: preexec's `C` on the output's
            // first row, precmd's `D` (with the exit code) on the
            // next prompt's row, then that prompt's `A`/`B`.
            events.send(mark('C', None, 11)).expect("C");
            std::thread::sleep(Duration::from_millis(10));
            events.send(mark('D', Some("7"), 14)).expect("D");
            // The next prompt's marks may already race the
            // resolved run — the receiver can be gone.
            let _ = events.send(mark('A', None, 14));
            let _ = events.send(mark('B', None, 14));
        }
    });

    let result = executor.run("echo hi").expect("the typed run resolves");
    driver.join().expect("driver");

    assert_eq!(result.exit_code, Some(7));
    assert_eq!(
        result.output,
        b"row 11\nrow 12\nrow 13".to_vec(),
        "output is the rows from C to D — D's own prompt row excluded"
    );
    assert_eq!(
        shares.sent(),
        vec![b"echo hi\n".to_vec()],
        "the command was typed verbatim — the visible trace"
    );
    assert!(
        shares.commands().is_empty(),
        "a typed tier never touches the in-band channel"
    );
}

/// A `133;D` arriving with NO mark in front of it — the normal
/// completion under the precmd `D,A,B` order when a shell emits no
/// `C` — still resolves: the send-time cursor line is the span's
/// start (us-13's bounded-failure rule never invents a span, it
/// pins one).
#[test]
fn marks_only_d_without_a_prior_mark_completes() {
    let (bridge, shares) = scripted(ShellIntegration::MarksOnly);
    shares
        .grid
        .lock()
        .expect("grid lock")
        .extend((0..30).map(|i| format!("row {i}")));
    shares.set_cursor(5);
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let events = shares.subscriber();
            let _typed = shares.next_typed();
            events.send(mark('D', Some("0"), 9)).expect("D");
        }
    });

    let result = executor
        .run("echo hi")
        .expect("a mark-free D is the normal completion");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(
        result.output,
        b"row 5\nrow 6\nrow 7\nrow 8".to_vec(),
        "the send-time cursor line anchored the span"
    );
}

/// Shells whose `B` DOES ride post-typing (a precmd order unlike
/// ait-shell's): the mark re-opens the send-time anchor at its own
/// row — the paired-`B` capture still works.
#[test]
fn marks_only_a_post_typing_b_reanchors_the_span() {
    let (bridge, shares) = scripted(ShellIntegration::MarksOnly);
    shares
        .grid
        .lock()
        .expect("grid lock")
        .extend((0..30).map(|i| format!("row {i}")));
    shares.set_cursor(9); // the anchor the post-typing `B` replaces
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let events = shares.subscriber();
            let _typed = shares.next_typed();
            events.send(mark('B', None, 10)).expect("B");
            events.send(mark('D', Some("3"), 14)).expect("D");
        }
    });

    let result = executor.run("echo hi").expect("the run resolves");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, Some(3));
    assert_eq!(
        result.output,
        b"row 10\nrow 11\nrow 12\nrow 13".to_vec(),
        "output is the rows between the post-typing B and D"
    );
}

/// A `133;D` with no payload reports "exit unknown" — the model is
/// told honestly rather than handed a guessed code (us-15).
#[test]
fn marks_only_payload_free_d_reports_exit_unknown() {
    let (bridge, shares) = scripted(ShellIntegration::MarksOnly);
    shares
        .grid
        .lock()
        .expect("grid lock")
        .extend((0..30).map(|i| format!("row {i}")));
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let events = shares.subscriber();
            let _typed = shares.next_typed();
            events.send(mark('C', None, 10)).expect("C");
            events.send(mark('D', None, 12)).expect("D");
        }
    });

    let result = executor.run("echo hi").expect("the run still resolves");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, None, "payload-free D = exit unknown");
    assert_eq!(result.output, b"row 10\nrow 11".to_vec());
}

/// An unparseable `133;D` payload is "exit unknown" too — never
/// invented (us-15's rule covers garbage as well as absence).
#[test]
fn marks_only_garbage_d_payload_is_exit_unknown() {
    let (bridge, shares) = scripted(ShellIntegration::MarksOnly);
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let events = shares.subscriber();
            let _typed = shares.next_typed();
            events.send(mark('C', None, 10)).expect("C");
            events.send(mark('D', Some("bogus"), 11)).expect("D");
        }
    });

    let result = executor.run("true").expect("the run resolves");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, None);
}

/// A `D` that never arrives — the shell's marks stopped — fails
/// inside the typed bound instead of hanging (us-13's "never a
/// hang" half).
#[test]
fn marks_only_missing_d_times_out_boundedly() {
    let (bridge, shares) = scripted(ShellIntegration::MarksOnly);
    let mut executor = UserShellExecutor::with_typed_timeout(
        Box::new(bridge),
        CancellationToken::new(),
        Duration::from_millis(150),
    );

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let events = shares.subscriber();
            let _typed = shares.next_typed();
            events.send(mark('C', None, 10)).expect("C");
        }
    });

    let started = std::time::Instant::now();
    let error = executor.run("sleep 3600").expect_err("no D means a bound");
    driver.join().expect("driver");
    assert!(matches!(error, ExecutorError::TypedTimedOut), "{error:?}");
    assert!(started.elapsed() < BOUND, "the bound was actually bounded");
}

/// A second `B` mid-run re-opens the capture at the newer mark — the
/// `RunAccumulator` tear-and-restart rule applied to marks: D pairs
/// with the latest mark, never the stale one.
#[test]
fn marks_only_second_b_restarts_the_span() {
    let (bridge, shares) = scripted(ShellIntegration::MarksOnly);
    shares
        .grid
        .lock()
        .expect("grid lock")
        .extend((0..30).map(|i| format!("row {i}")));
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let events = shares.subscriber();
            let _typed = shares.next_typed();
            events.send(mark('B', None, 10)).expect("B1");
            events.send(mark('C', None, 12)).expect("C");
            events.send(mark('D', Some("0"), 15)).expect("D");
        }
    });

    let result = executor.run("echo hi").expect("the run resolves");
    driver.join().expect("driver");
    assert_eq!(
        result.output,
        b"row 12\nrow 13\nrow 14".to_vec(),
        "the span re-opened at the later mark"
    );
}

/// An `Absent` run types `cmd; printf '__AIT_END_<id>=%d\n' $?` — the
/// whole wrapped line stays visible in the grid (us-14/16) — then
/// scans the raw tail for the sentinel: the bytes between the echoed
/// type-in line and the sentinel line are the output, the sentinel's
/// number is the exit code.
#[test]
fn absent_run_reads_sentinel_output_and_exit_code() {
    let (bridge, shares) = scripted(ShellIntegration::Absent);
    shares
        .tail
        .lock()
        .expect("tail lock")
        .extend_from_slice(b"user@host:~$ ");
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let _events = shares.subscriber();
            let typed = shares.next_typed();
            let marker = sentinel_marker(&typed);
            // The session echoes the typed line, runs it, prints the
            // result line, then redraws the prompt — all raw bytes.
            let mut stream = typed.clone();
            // The PTY echoes `\n` as `\r\n`.
            *stream.last_mut().expect("a typed line") = b'\r';
            stream.extend_from_slice(b"\noutput line one\r\n__AIT_END_");
            stream.extend_from_slice(&marker[b"__AIT_END_".len()..]);
            stream.extend_from_slice(b"=7\r\nuser@host:~$ ");
            shares.emit(&stream);
        }
    });

    let result = executor
        .run("printf 'output line one\\n'")
        .expect("the sentinel resolves the run");
    driver.join().expect("driver");

    assert_eq!(result.exit_code, Some(7));
    assert_eq!(
        result.output, b"output line one",
        "output = bytes between the echoed type-in and the sentinel line"
    );
    let typed = &shares.sent()[0];
    let text = String::from_utf8(typed.clone()).expect("typed is text");
    assert!(
        text.starts_with("printf 'output line one\\n'; printf '__AIT_END_"),
        "the sentinel wrap was typed verbatim: {text:?}"
    );
    assert!(
        text.ends_with("=%d\\n' $?\n"),
        "the wrap ends with the $? print and the Enter: {text:?}"
    );
}

/// A sentinel that never prints — the shell swallowed the line, died
/// quietly, or isn't a shell at all — is a bounded failure, never a
/// hang (us-14's failure shape). Tail activity (a chatty session)
/// only postpones the bound while bytes keep arriving.
#[test]
fn absent_missing_sentinel_fails_boundedly() {
    let (bridge, shares) = scripted(ShellIntegration::Absent);
    let mut executor = UserShellExecutor::with_typed_timeout(
        Box::new(bridge),
        CancellationToken::new(),
        Duration::from_millis(150),
    );

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let _events = shares.subscriber();
            let _typed = shares.next_typed();
            // Noise, but no sentinel: echoes and other commands'
            // output must not resolve the wait.
            shares.emit(b"unrelated output\r\nmore noise\r\n");
        }
    });

    let started = std::time::Instant::now();
    let error = executor.run("true").expect_err("no sentinel means a bound");
    driver.join().expect("driver");
    assert!(matches!(error, ExecutorError::TypedTimedOut), "{error:?}");
    assert!(started.elapsed() < BOUND, "the bound was actually bounded");
}

/// A torn sentinel — the marker printed without its `=<code>` tail,
/// or with garbage where the code belongs — resolves nothing; the
/// wait ends at the bound rather than on a fabricated exit code.
#[test]
fn absent_torn_sentinel_never_resolves() {
    let (bridge, shares) = scripted(ShellIntegration::Absent);
    let mut executor = UserShellExecutor::with_typed_timeout(
        Box::new(bridge),
        CancellationToken::new(),
        Duration::from_millis(150),
    );

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let _events = shares.subscriber();
            let typed = shares.next_typed();
            let marker = sentinel_marker(&typed);
            let mut stream = Vec::new();
            stream.extend_from_slice(&marker);
            stream.extend_from_slice(b"\r\n"); // no `=<code>` at all
            stream.extend_from_slice(&marker);
            stream.extend_from_slice(b"=notanumber\r\n");
            shares.emit(&stream);
        }
    });

    let started = std::time::Instant::now();
    let error = executor
        .run("echo hi")
        .expect_err("a torn sentinel cannot resolve");
    driver.join().expect("driver");
    assert!(matches!(error, ExecutorError::TypedTimedOut), "{error:?}");
    assert!(started.elapsed() < BOUND);
}

// ── Spec R16 §5.14: the Absent sentinel is per shell family ──────
//
// `shell_name` (the same basename the planner keyed on) picks the
// sentinel's syntax: PowerShell `Write-Output` carrying
// `$LASTEXITCODE` — cleared first so a stale native code can't
// poison a cmdlet run — cmd's `call echo %^ERRORLEVEL%`, everything
// else the POSIX `printf` form. A remote session always takes the
// POSIX form — the remote shell is the POSIX bootstrap however the
// local program spells.

/// A `pwsh` session's `Absent` run types the PowerShell sentinel —
/// the `Write-Output`/`$LASTEXITCODE` form, not `printf` — and still
/// completes on `<marker>=<digits>` in the tail. `powershell.exe`
/// (the 5.1 host) spells the same family.
#[test]
fn absent_pwsh_types_the_powershell_sentinel() {
    for shell in ["pwsh", "powershell.exe", "PWSH.EXE"] {
        let (bridge, shares) = scripted(ShellIntegration::Absent);
        *bridge.shell.lock().expect("shell lock") = shell.to_owned();
        shares
            .tail
            .lock()
            .expect("tail lock")
            .extend_from_slice(b"PS C:\\> ");
        let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

        let driver = std::thread::spawn({
            let shares = shares.clone();
            move || {
                let _events = shares.subscriber();
                let typed = shares.next_typed();
                let marker = sentinel_marker(&typed);
                // The interactive echo carries the literal
                // `$LASTEXITCODE`/`$(…)` text — `=`-without-digits,
                // never a result — then the run's real result line.
                let mut stream = typed.clone();
                *stream.last_mut().expect("typed line") = b'\r';
                stream.extend_from_slice(b"\nsmoke output\r\n__AIT_END_");
                stream.extend_from_slice(&marker[b"__AIT_END_".len()..]);
                stream.extend_from_slice(b"=4\r\nPS C:\\> ");
                shares.emit(&stream);
            }
        });

        let result = executor
            .run("Write-Output 'smoke output'")
            .expect("the pwsh sentinel resolves the run");
        driver.join().expect("driver");

        assert_eq!(result.exit_code, Some(4), "{shell}");
        let text = String::from_utf8(shares.sent()[0].clone()).expect("typed is text");
        assert!(
            text.contains("; Write-Output \"__AIT_END_"),
            "{shell}: the PowerShell sentinel was typed: {text:?}"
        );
        assert!(
            text.contains("LASTEXITCODE"),
            "{shell}: the exit code rides $LASTEXITCODE: {text:?}"
        );
        assert!(
            !text.contains("printf"),
            "{shell}: never the POSIX form: {text:?}"
        );
    }
}

/// A `cmd.exe` session's `Absent` run types the cmd sentinel —
/// `& call echo <marker>=%^ERRORLEVEL%`: `call` plus the `^`-escape
/// defers `%ERRORLEVEL%` past cmd's parse-time expansion so it reads
/// THIS command's code (a bare `%ERRORLEVEL%` would report the
/// previous line's).
#[test]
fn absent_cmd_types_the_cmd_sentinel() {
    for shell in ["cmd.exe", "CMD.EXE"] {
        let (bridge, shares) = scripted(ShellIntegration::Absent);
        *bridge.shell.lock().expect("shell lock") = shell.to_owned();
        let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

        let driver = std::thread::spawn({
            let shares = shares.clone();
            move || {
                let _events = shares.subscriber();
                let typed = shares.next_typed();
                let marker = sentinel_marker(&typed);
                let mut stream = typed.clone();
                *stream.last_mut().expect("typed line") = b'\r';
                stream.extend_from_slice(b"\nout\r\n__AIT_END_");
                stream.extend_from_slice(&marker[b"__AIT_END_".len()..]);
                stream.extend_from_slice(b"=2\r\nC:\\>");
                shares.emit(&stream);
            }
        });

        let result = executor
            .run("echo out")
            .expect("the cmd sentinel resolves the run");
        driver.join().expect("driver");

        assert_eq!(result.exit_code, Some(2), "{shell}");
        let text = String::from_utf8(shares.sent()[0].clone()).expect("typed is text");
        assert!(
            text.contains(" & call echo __AIT_END_"),
            "{shell}: the cmd sentinel was typed: {text:?}"
        );
        assert!(
            text.ends_with("=%^ERRORLEVEL%\n"),
            "{shell}: the escaped expansion, not the parse-time one: {text:?}"
        );
    }
}

/// A sentinel result's exit code may be negative — `exit -1` under
/// pwsh, a killed process's status under cmd — and still parses:
/// `-1` is a result, not a torn line.
#[test]
fn absent_sentinel_parses_a_negative_exit_code() {
    let (bridge, shares) = scripted(ShellIntegration::Absent);
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let _events = shares.subscriber();
            let typed = shares.next_typed();
            let marker = sentinel_marker(&typed);
            let mut stream = typed.clone();
            *stream.last_mut().expect("typed line") = b'\r';
            stream.extend_from_slice(b"\n__AIT_END_");
            stream.extend_from_slice(&marker[b"__AIT_END_".len()..]);
            stream.extend_from_slice(b"=-1\r\n");
            shares.emit(&stream);
        }
    });

    let result = executor.run("exit -1").expect("a signed code resolves");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, Some(-1));
}

/// A remote session reached through ssh() always types the POSIX
/// sentinel — the remote shell is the POSIX bootstrap however the
/// LOCAL program spells (a pwsh tab's unbootstrapped remote gets
/// `printf`, never `Write-Output`).
#[test]
fn a_remote_session_keeps_the_posix_sentinel() {
    let (bridge, shares) = scripted(ShellIntegration::Integrated);
    *bridge.shell.lock().expect("shell lock") = "pwsh.exe".to_owned();
    *bridge.remote.lock().expect("remote lock") = RemoteState::Remote {
        target: "prod".to_owned(),
        control_path: None,
        bootstrapped: false,
    };
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let _events = shares.subscriber();
            let typed = shares.next_typed();
            let marker = sentinel_marker(&typed);
            let mut stream = typed.clone();
            *stream.last_mut().expect("typed line") = b'\r';
            stream.extend_from_slice(b"\nremote-out\r\n__AIT_END_");
            stream.extend_from_slice(&marker[b"__AIT_END_".len()..]);
            stream.extend_from_slice(b"=0\r\n");
            shares.emit(&stream);
        }
    });

    let result = executor.run("echo hi").expect("the sentinel tier runs");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, Some(0));
    let text = String::from_utf8(shares.sent()[0].clone()).expect("typed is text");
    assert!(
        text.contains("printf '__AIT_END_"),
        "the remote shell takes the POSIX form: {text:?}"
    );
    assert!(
        !text.contains("Write-Output") && !text.contains("call echo"),
        "the local pwsh spelling never reaches a remote: {text:?}"
    );
}

/// The session exiting mid-wait — on either typed tier — is
/// `SessionLost` promptly (us-34), not a bound's-length wait for
/// marks that can never arrive.
#[test]
fn exited_session_interrupts_typed_waits() {
    for state in [ShellIntegration::MarksOnly, ShellIntegration::Absent] {
        let (bridge, shares) = scripted(state);
        let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

        let driver = std::thread::spawn({
            let shares = shares.clone();
            move || {
                let events = shares.subscriber();
                let _typed = shares.next_typed();
                events.send(BridgeEvent::Exited).expect("Exited");
            }
        });

        let started = std::time::Instant::now();
        let error = executor.run("sleep 60").expect_err("a dead session errors");
        driver.join().expect("driver");
        assert!(
            matches!(error, ExecutorError::SessionLost),
            "{state:?}: {error:?}"
        );
        assert!(started.elapsed() < BOUND, "the exit answered promptly");
    }
}

/// Esc mid-typed-wait: cancellation interrupts the mark/sentinel
/// poll exactly the way it interrupts the in-band wait.
#[test]
fn cancel_interrupts_typed_waits() {
    for state in [ShellIntegration::MarksOnly, ShellIntegration::Absent] {
        let (bridge, _shares) = scripted(state);
        let token = CancellationToken::new();
        let mut executor = UserShellExecutor::new(Box::new(bridge), token.clone());

        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            token.cancel();
        });

        let started = std::time::Instant::now();
        let error = executor
            .run("sleep 60")
            .expect_err("a cancelled run errors");
        canceller.join().expect("canceller");
        assert!(
            matches!(error, ExecutorError::Cancelled),
            "{state:?}: {error:?}"
        );
        assert!(started.elapsed() < BOUND, "cancellation was prompt");
    }
}

/// A session that exited — or whose frontend end is gone — is a clean
/// executor error the caller turns into a tool result, never a hang
/// (Spec R06 us-34).
#[test]
fn exited_and_gone_sessions_error_cleanly() {
    for fail in [BridgeError::SessionExited, BridgeError::Gone] {
        let (bridge, _shares) = scripted(ShellIntegration::Integrated);
        *bridge.fail_run.lock().expect("fail lock") = Some(fail);
        let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

        let error = executor.run("echo hi").expect_err("a dead session errors");
        assert!(matches!(error, ExecutorError::Bridge(_)), "{error:?}");
    }
}

/// A run whose response channel dies mid-wait — the session exited
/// between the request and its `7701` frames — resolves as
/// `SessionLost`, not a forever-hang (us-34's other half).
#[test]
fn a_dropped_run_channel_is_session_lost() {
    let (bridge, shares) = scripted(ShellIntegration::Integrated);
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let dropper = std::thread::spawn(move || {
        // Clear only AFTER the run registers its waiter: a fixed sleep
        // races the registration under load, and a clear that lands
        // first leaves a live sender — the run then waits for frames
        // forever instead of seeing the mid-wait disconnect.
        let deadline = std::time::Instant::now() + BOUND;
        loop {
            if !shares.runs.lock().expect("runs lock").is_empty() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "run never registered");
            std::thread::sleep(Duration::from_millis(1));
        }
        shares.runs.lock().expect("runs lock").clear();
    });

    let error = executor.run("sleep 60").expect_err("a dead channel errors");
    dropper.join().expect("dropper");
    assert!(matches!(error, ExecutorError::SessionLost), "{error:?}");
}

/// Esc mid-run: the cancellation token interrupts the executor's wait
/// promptly — the task's own `Cancelled` event follows upstream.
#[test]
fn cancel_interrupts_the_wait_promptly() {
    let (bridge, _shares) = scripted(ShellIntegration::Integrated);
    let token = CancellationToken::new();
    let mut executor = UserShellExecutor::new(Box::new(bridge), token.clone());

    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        token.cancel();
    });

    let started = std::time::Instant::now();
    let error = executor
        .run("sleep 60")
        .expect_err("a cancelled run errors");
    canceller.join().expect("canceller");
    assert!(matches!(error, ExecutorError::Cancelled), "{error:?}");
    assert!(started.elapsed() < BOUND, "cancellation was prompt");
}

/// A torn `7701` stream — a second `A` mid-run — never resolves the
/// wait wrongly: the real [`RunAccumulator`] tears and restarts, and
/// the clean stream that follows still answers (the `inband.rs`
/// recovery test's shape, through the executor this time).
#[test]
fn torn_frame_streams_still_resolve_correctly() {
    let (bridge, shares) = scripted(ShellIntegration::Integrated);
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let responder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        // A torn stream: A, one R, then a fresh A — the accumulator
        // discards the first and reassembles the second.
        let stream = [
            ShellEvent::RunBegin,
            ShellEvent::RunChunk {
                payload: b"0;cut-".to_vec(),
            },
            ShellEvent::RunBegin,
        ]
        .into_iter()
        .chain(run_stream(b"0;real answer;0"))
        .collect();
        let result = accumulate(stream).expect("the clean stream completes");
        let waiter = shares.runs.lock().expect("runs lock")[0].clone();
        waiter.send(result).expect("waiter");
    });

    let result = executor.run("echo real").expect("the torn run resolves");
    responder.join().expect("responder");
    assert_eq!(result.output, b"real answer".to_vec());
    assert_eq!(result.exit_code, Some(0));
}

/// §5.9 shape proof (Spec R06 us-35): `CommandExecutor`'s signature
/// carries nothing user-shell-specific — ControlMaster, remote-daemon
/// and no-op tiers do their own I/O and need no `ShellBridge`. These
/// stubs are the sketch: they implement the trait against the same
/// `run` the user-shell executor serves.
#[test]
fn executor_tiers_need_no_bridge() {
    /// A §5.9 ControlMaster-style sketch: multiplexes over its own
    /// SSH channel — no `ShellBridge` anywhere in the impl.
    struct ControlMasterExecutor;
    impl CommandExecutor for ControlMasterExecutor {
        fn run(&mut self, command: &str) -> Result<ait_agent::CommandResult, ExecutorError> {
            // Owns its transport; the call shape is identical.
            let _ = command;
            Ok(ait_agent::CommandResult {
                output: b"remote out".to_vec(),
                exit_code: Some(0),
            })
        }
    }

    // The §5.9 no-op tier: nothing to run against answers a clean
    // error — `DetachedExecutor` IS this sketch, shipped.
    let mut detached: Box<dyn CommandExecutor> = Box::new(DetachedExecutor);
    assert!(matches!(
        detached.run("echo hi"),
        Err(ExecutorError::Unbound)
    ));

    let mut control_master: Box<dyn CommandExecutor> = Box::new(ControlMasterExecutor);
    let result = control_master.run("true").expect("the sketch runs");
    assert_eq!(result.exit_code, Some(0));
}

// ── Spec R09 §5.9: the executor tiers against a stub ssh ─────────
//
// A POSIX `ssh` stub on disk answers `-O check` and records every
// argv — the ControlMaster tier, demotion and `select_executor`'s
// probe all run without a network.

/// A stub `ssh` with its answers baked in: `-O check` exits
/// `@CHECK_RC@` (0 = live socket); every invocation's argv lands one
/// line per call in `@ARGV@`; the command form `cat`s `@OUT_FILE@`
/// and exits `@RUN_RC@`. The values live inside each test's own
/// script — `std::env::set_var` would leak them across the parallel
/// tests sharing this process (the R09 retro CI flake: one test's
/// run logged into another's argv file, another's RC answered as
/// transport).
#[cfg(unix)]
const SSH_STUB: &str = r#"#!/bin/sh
{
    printf 'stub:'
    for a in "$@"; do printf ' <%s>' "$a"; done
    printf '\n'
} >>"@ARGV@"
for a in "$@"; do
    if [ "$a" = check ]; then exit @CHECK_RC@; fi
done
cat "@OUT_FILE@"
exit @RUN_RC@
"#;

/// Install [`SSH_STUB`] in a scratch dir with `check_rc`/`run_rc`/`out`
/// baked into the script; returns the stub's path and the argv log
/// it writes.
#[cfg(unix)]
fn install_ssh_stub(
    tag: &str,
    check_rc: u8,
    run_rc: u8,
    out: &[u8],
) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let program = dir.path().join(format!("ssh-{tag}"));
    let log = dir.path().join("argv.log");
    let out_file = dir.path().join("stdout");
    std::fs::write(&out_file, out).expect("write stub stdout");
    let script = SSH_STUB
        .replace("@ARGV@", log.to_str().expect("utf-8 log"))
        .replace("@CHECK_RC@", &check_rc.to_string())
        .replace("@OUT_FILE@", out_file.to_str().expect("utf-8 out"))
        .replace("@RUN_RC@", &run_rc.to_string());
    std::fs::write(&program, script).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&program, permissions).unwrap();
    }
    (dir, program, log)
}

/// The recorded argv lines `log` holds.
#[cfg(unix)]
fn stub_calls(log: &std::path::Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

/// Spec R09-T5: the ControlMaster tier spawns `ssh -S <sock>
/// -o BatchMode=yes <target> -- <cmd> 2>&1` — `--` ends ssh's own
/// option parsing so a command starting with `-` is never a client
/// flag, and the remote shell merges stderr like `__ait_run` does.
#[cfg(unix)]
#[test]
fn control_master_runs_commands_over_the_socket() {
    let (_dir, program, log) = install_ssh_stub("cm", 0, 3, b"merged remote output");
    let mut executor = ait_agent::ControlMasterExecutor::with_program(
        program.to_str().expect("utf-8").to_owned(),
        "/run/ait/ssh/cm".to_owned(),
        "prod".to_owned(),
        CancellationToken::new(),
    );

    let result = executor
        .run("-ls --flags")
        .expect("the remote command resolves");
    assert_eq!(result.output, b"merged remote output");
    assert_eq!(result.exit_code, Some(3));
    let calls = stub_calls(&log);
    assert!(
        calls.contains(
            "stub: <-S> </run/ait/ssh/cm> <-o> <BatchMode=yes> <prod> <--> <-ls --flags> <2>&1>"
        ),
        "the argv shape, `--` before the command: {calls}"
    );
}

/// Spec R09-T5: a `255` exit is ssh's transport-failure band — it
/// surfaces as `Transport`, never as the remote command's exit code.
#[cfg(unix)]
#[test]
fn control_master_255_is_transport_not_an_exit_code() {
    let (_dir, program, _log) = install_ssh_stub("cm-255", 0, 255, b"");
    let mut executor = ait_agent::ControlMasterExecutor::with_program(
        program.to_str().expect("utf-8").to_owned(),
        "/run/ait/ssh/cm".to_owned(),
        "prod".to_owned(),
        CancellationToken::new(),
    );
    let error = executor.run("true").expect_err("255 = transport");
    assert!(matches!(error, ExecutorError::Transport(_)), "{error:?}");
}

/// Spec R09-T5's demote rule: a transport failure flips the tier
/// permanently — THIS run returns the failure (never replayed — the
/// command may have partially run remote-side), and every LATER run
/// goes straight to the user-shell fallback. The probe the
/// demotion replaced never runs again.
#[cfg(unix)]
#[test]
fn demotion_never_replays_and_never_recovers() {
    let (_dir, program, log) = install_ssh_stub("demote", 0, 255, b"");
    let (bridge, shares) = scripted(ShellIntegration::Absent);
    let primary = ait_agent::ControlMasterExecutor::with_program(
        program.to_str().expect("utf-8").to_owned(),
        "/run/ait/ssh/cm".to_owned(),
        "prod".to_owned(),
        CancellationToken::new(),
    );
    let fallback = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());
    let mut executor = ait_agent::DemotingExecutor::new(primary, fallback);

    // The failing run returns the transport error itself — no
    // silent replay through the typed tier.
    let error = executor.run("rm -rf /x").expect_err("the hit errors");
    assert!(matches!(error, ExecutorError::Transport(_)), "{error:?}");
    assert!(executor.demoted(), "demoted after the transport hit");
    assert!(
        shares.sent().is_empty(),
        "the failed command was never retyped through the fallback"
    );

    // The next command answers through the fallback (typed tier —
    // the Absent sentinel path), never touching ssh again.
    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let _events = shares.subscriber();
            let typed = shares.next_typed();
            let marker = sentinel_marker(&typed);
            let mut stream = typed.clone();
            *stream.last_mut().expect("typed line") = b'\r';
            stream.extend_from_slice(b"\nok\r\n__AIT_END_");
            stream.extend_from_slice(&marker[b"__AIT_END_".len()..]);
            stream.extend_from_slice(b"=0\r\n");
            shares.emit(&stream);
        }
    });
    let result = executor.run("echo ok").expect("the fallback runs");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, Some(0));
    let calls = stub_calls(&log);
    assert_eq!(
        calls.lines().count(),
        1,
        "ssh answered exactly the one failed run: {calls}"
    );
}

/// Spec R09-T5's selection matrix through the real
/// [`select_executor_with_program`] seam: `Remote` + a live socket
/// selects ControlMaster (demotion-wrapped); a dead socket, no
/// control path, and a non-remote session all stay on the user-shell
/// tiers; no bridge selects `DetachedExecutor`.
#[cfg(unix)]
#[test]
fn select_executor_picks_the_tier_the_socket_proves() {
    // Two stubs: the live one answers `-O check` 0 and `via socket`
    // to a run; the dead one answers check 255.
    let (live_dir, live_program, live_log) = install_ssh_stub("select-live", 0, 0, b"via socket");
    let (dead_dir, dead_program, _dead_log) = install_ssh_stub("select-dead", 255, 0, b"");
    let live_program = live_program.to_str().expect("utf-8").to_owned();
    let dead_program = dead_program.to_str().expect("utf-8").to_owned();

    // No bridge → the no-op tier.
    let mut executor = select_executor_with_program(None, CancellationToken::new(), &live_program);
    assert!(matches!(executor.run("x"), Err(ExecutorError::Unbound)));

    // Remote + a live socket (stub answers check 0) → the
    // ControlMaster tier: a command spawns `ssh -S`, never the
    // typed/in-band path.
    let (bridge, shares) = scripted(ShellIntegration::Absent);
    *bridge.remote.lock().expect("remote lock") = RemoteState::Remote {
        target: "prod".to_owned(),
        control_path: Some("/run/ait/ssh/cm".to_owned()),
        bootstrapped: true,
    };
    let mut executor = select_executor_with_program(
        Some(Box::new(bridge)),
        CancellationToken::new(),
        &live_program,
    );
    let result = executor.run("echo hi").expect("the socket runs");
    assert_eq!(result.output, b"via socket");
    assert!(shares.sent().is_empty(), "nothing typed at the PTY");

    // Remote + a DEAD socket (check 255) → the user-shell tiers.
    let (bridge, shares) = scripted(ShellIntegration::Absent);
    *bridge.remote.lock().expect("remote lock") = RemoteState::Remote {
        target: "prod".to_owned(),
        control_path: Some("/run/ait/ssh/cm".to_owned()),
        bootstrapped: true,
    };
    let mut executor = select_executor_with_program(
        Some(Box::new(bridge)),
        CancellationToken::new(),
        &dead_program,
    );
    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let _events = shares.subscriber();
            let typed = shares.next_typed();
            let marker = sentinel_marker(&typed);
            let mut stream = typed.clone();
            *stream.last_mut().expect("typed line") = b'\r';
            stream.extend_from_slice(b"\n\r\n__AIT_END_");
            stream.extend_from_slice(&marker[b"__AIT_END_".len()..]);
            stream.extend_from_slice(b"=0\r\n");
            shares.emit(&stream);
        }
    });
    let result = executor.run("echo hi").expect("the typed tier runs");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, Some(0));

    // Remote with NO control path → same user-shell selection — and
    // the dead-socket probe is never even paid: the live stub's argv
    // still holds only the `-O check` + the one `ssh -S` run above.
    let (bridge, _shares) = scripted(ShellIntegration::Absent);
    *bridge.remote.lock().expect("remote lock") = RemoteState::Remote {
        target: "prod".to_owned(),
        control_path: None,
        bootstrapped: true,
    };
    let mut executor = select_executor_with_program(
        Some(Box::new(bridge)),
        CancellationToken::new(),
        &live_program,
    );
    let driver = std::thread::spawn({
        let shares = _shares.clone();
        move || {
            let _events = shares.subscriber();
            let typed = shares.next_typed();
            let marker = sentinel_marker(&typed);
            let mut stream = typed.clone();
            *stream.last_mut().expect("typed line") = b'\r';
            stream.extend_from_slice(b"\n\r\n__AIT_END_");
            stream.extend_from_slice(&marker[b"__AIT_END_".len()..]);
            stream.extend_from_slice(b"=0\r\n");
            shares.emit(&stream);
        }
    });
    let result = executor.run("echo hi").expect("the typed tier runs");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(
        stub_calls(&live_log).lines().count(),
        2,
        "the no-control-path selection paid no probe: {}",
        stub_calls(&live_log)
    );
    drop((live_dir, dead_dir));
}

/// Spec R09 us-18: a remote session whose remote shell never
/// bootstrapped — `Remote { bootstrapped: false }` over a locally
/// `Integrated` tab — must NOT park on `__ait_run` frames that can
/// never arrive: the executor clamps to the `Absent` sentinel tier.
#[test]
fn an_unbootstrapped_remote_clamps_off_the_in_band_tier() {
    let (bridge, shares) = scripted(ShellIntegration::Integrated);
    *bridge.remote.lock().expect("remote lock") = RemoteState::Remote {
        target: "prod".to_owned(),
        control_path: None,
        bootstrapped: false,
    };
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let driver = std::thread::spawn({
        let shares = shares.clone();
        move || {
            let _events = shares.subscriber();
            let typed = shares.next_typed();
            let marker = sentinel_marker(&typed);
            let mut stream = typed.clone();
            *stream.last_mut().expect("typed line") = b'\r';
            stream.extend_from_slice(b"\nremote-out\r\n__AIT_END_");
            stream.extend_from_slice(&marker[b"__AIT_END_".len()..]);
            stream.extend_from_slice(b"=2\r\n");
            shares.emit(&stream);
        }
    });

    let result = executor.run("echo hi").expect("the sentinel tier runs");
    driver.join().expect("driver");
    assert_eq!(result.exit_code, Some(2));
    assert!(
        shares.commands().is_empty(),
        "an unbootstrapped remote never issues run_in_band"
    );
    assert!(
        !shares.sent().is_empty(),
        "the command typed through the visible channel"
    );
}

/// The mirror: `bootstrapped: true` keeps the remote session on the
/// in-band tier — the remote shell's `__ait_run` answers.
#[test]
fn a_bootstrapped_remote_stays_in_band() {
    let (bridge, shares) = scripted(ShellIntegration::Integrated);
    *bridge.remote.lock().expect("remote lock") = RemoteState::Remote {
        target: "prod".to_owned(),
        control_path: Some("/run/ait/ssh/cm".to_owned()),
        bootstrapped: true,
    };
    let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

    let responder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        shares.respond(b"0;remote answer;9");
    });

    let result = executor.run("echo hi").expect("the in-band run resolves");
    responder.join().expect("responder");
    assert_eq!(result.output, b"remote answer");
    assert_eq!(result.exit_code, Some(9));
}
