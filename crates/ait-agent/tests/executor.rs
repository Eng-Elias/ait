//! The `ShellBridge`/`CommandExecutor` seam (Spec R06 testing
//! decisions / issue #87): a *scripted* bridge implementation — no
//! PTY anywhere — that records the bytes offered to it and resolves
//! `run_in_band` waits by feeding hand-built `7701` `A…R…B` frame
//! streams through ait-shell's real [`RunAccumulator`]
//! (`crates/ait/src/inband.rs` tests are the prior art).
//!
//! What these tests prove:
//!
//! - `UserShellExecutor` on an `Integrated` session returns the
//!   command's merged stdout+stderr and exit code (Spec R06 us-12);
//! - `MarksOnly`/`Absent` sessions refuse BEFORE any bytes are
//!   offered to the bridge (their typed-fallback tiers are R06-T5's);
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
    BridgeError, BridgeEvent, CommandExecutor, DetachedExecutor, ExecutorError, ShellBridge,
    ShellIntegration, UserShellExecutor,
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
}

/// The scripted bridge: reports `state`, records every byte and
/// command offered, and hands the test one waiter channel per
/// `run_in_band` request.
struct ScriptedBridge {
    /// What [`integration_state`](ShellBridge::integration_state)
    /// reports — the executor's tier selector reads it per call.
    state: ShellIntegration,
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
        shares: shares.clone(),
        fail_run: Mutex::new(None),
    };
    (bridge, shares)
}

impl ShellBridge for ScriptedBridge {
    fn integration_state(&self) -> Result<ShellIntegration, BridgeError> {
        Ok(self.state)
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

    fn grid_lines(&self, _start: i32, _end: i32) -> Result<Vec<String>, BridgeError> {
        Ok(Vec::new())
    }

    fn output_tail(&self) -> Result<Vec<u8>, BridgeError> {
        Ok(Vec::new())
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

/// `MarksOnly` and `Absent` sessions refuse BEFORE a byte is offered:
/// their typed-fallback tiers are R06-T5's — until then the executor
/// reports the state rather than typing a request a bare shell could
/// never answer (Spec R06 us-13/14 shape; standard 1's degrade).
#[test]
fn unintegrated_states_refuse_before_any_bytes() {
    for state in [ShellIntegration::MarksOnly, ShellIntegration::Absent] {
        let (bridge, shares) = scripted(state);
        let mut executor = UserShellExecutor::new(Box::new(bridge), CancellationToken::new());

        let error = executor
            .run("echo hi")
            .expect_err("a non-integrated session refuses");
        assert!(
            matches!(error, ExecutorError::Unsupported(s) if s == state),
            "{error:?}"
        );
        assert!(
            shares.commands().is_empty() && shares.sent().is_empty(),
            "a refused run offered nothing to the bridge"
        );
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
        std::thread::sleep(Duration::from_millis(20));
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
