//! The `agent` primitive: [`Agent::act`]/[`Agent::assert`], the verbs
//! that mix natural-language steps into an otherwise deterministic test
//! (Spec E01, ticket E01-T2).
//!
//! Each call is one numbered step with a committed `*.jsonl` recording.
//! Two paths:
//!
//! - **Replay (default).** The step's inputs are re-sent through the
//!   real encoding path and the recorded screen-state hash is
//!   re-asserted — no driver, no model, byte-deterministic. A missing
//!   file or edited step text fails with instructions; a screen that
//!   stops matching fails as **drift** with the recorded-vs-actual
//!   diff.
//! - **Record (`AIT_E2E_RECORD`).** The active [`Driver`] resolves the
//!   step against the settled screen and the file is rewritten. With
//!   the replay-only [`Authored`] driver this fails with guidance.
//!
//! `AIT_E2E_RECORD` is read once at [`E2e::spawn`]; tests override it
//! with [`E2e::set_record_mode`], which keeps self-tests free of
//! environment races.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::driver::{Input, Verdict};
use crate::harness::E2e;
use crate::recording::{self, Header, Op, Recording};
use crate::screen::{self, ScreenState};

/// Poll interval for the settle/await loops (matches the harness's).
const POLL: Duration = Duration::from_millis(5);

/// A screen unchanged for this long counts as settled at record time.
/// Generous next to a local PTY's millisecond turnaround; an app that
/// stays mid-transition longer simply records the pre-settle state,
/// which replay then matches just as early.
const SETTLE_STABLE: Duration = Duration::from_millis(150);

/// Upper bound on settling — after it the latest state is taken as-is
/// rather than failing (a perpetually-moving screen can't be
/// hash-guarded anyway, so it records what it records).
const SETTLE_BUDGET: Duration = Duration::from_secs(5);

/// Replay drift check: a screen stable this long while still not
/// matching the recording is declared drifted early — symmetric with
/// [`SETTLE_STABLE`]'s notion of "done changing", scaled up for margin.
/// Apps with genuinely delayed output would have recorded their
/// pre-output state too, so this introduces no new flake window.
const DRIFT_STABLE: Duration = Duration::from_millis(500);

/// Outer bound on waiting for the recorded screen — the live-test
/// tier's generous value.
const AWAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// The `agent.*` half of a test — a borrow of [`E2e`] that owns the
/// step sequence. `e2e.agent().act("…")` is the intended call shape.
pub struct Agent<'e> {
    e2e: &'e mut E2e,
}

impl<'e> Agent<'e> {
    /// [`E2e::agent`] is the public entry.
    pub(crate) fn new(e2e: &'e mut E2e) -> Self {
        Self { e2e }
    }
    /// `agent.act`: the driver chose these inputs once; replay re-sends
    /// them and re-asserts the recorded screen.
    ///
    /// # Panics
    /// On missing/stale recordings, drift, or a record-mode driver that
    /// cannot resolve — all first-class test failures.
    pub fn act(&mut self, goal: &str) {
        self.step(Op::Act, goal);
    }

    /// `agent.assert`: the driver judged the claim once; replay
    /// re-asserts the recorded verdict by re-asserting the recorded
    /// screen.
    ///
    /// # Panics
    /// On missing/stale recordings, drift, a recorded `fail` verdict,
    /// or a record-mode verdict of `fail` (with the driver's reason).
    pub fn assert(&mut self, claim: &str) {
        self.step(Op::Assert, claim);
    }

    /// Dispatch one numbered step to the record or replay path.
    fn step(&mut self, op: Op, text: &str) {
        let step = self.e2e.agent_step;
        self.e2e.agent_step += 1;
        let path = recording::step_path(
            &recording::recordings_root(self.e2e.recordings_dir.as_deref()),
            step,
        );
        if self.e2e.record {
            self.record(step, op, text, &path);
        } else {
            self.replay(step, op, text, &path);
        }
    }

    /// Record path (`AIT_E2E_RECORD`): resolve through the active
    /// driver, send the inputs, then write the settled screen and its
    /// hash as the step's new recording.
    fn record(&mut self, step: usize, op: Op, text: &str, path: &Path) {
        let screen = settle(self.e2e);
        let (inputs, verdict, screen) = match op {
            Op::Act => {
                let inputs = self.e2e.driver.act(text, &screen);
                for input in &inputs {
                    send_input(self.e2e, input);
                }
                // The recorded screen is the settled post-input state —
                // what replay waits for after re-sending the inputs.
                (inputs, None, settle(self.e2e))
            }
            Op::Assert => match self.e2e.driver.assert(text, &screen) {
                // Record exactly the screen that was judged, so the
                // hash guard re-asserts what the verdict saw.
                Verdict::Pass => (Vec::new(), Some(Verdict::Pass), screen),
                Verdict::Fail { reason } => panic!(
                    "agent step {step} (assert {text:?}) failed: {reason}\n\
                     --- screen ---\n{}",
                    screen.rows.join("\n")
                ),
            },
        };
        let recording = Recording {
            header: Header {
                v: recording::FORMAT_VERSION,
                step,
                op,
                text: text.to_string(),
                text_hash: recording::text_hash(op, text),
                program: self.e2e.program.clone(),
                screen_hash: screen.hash_hex(),
                screen,
                verdict,
            },
            inputs,
        };
        recording.save(path);
    }

    /// Replay path: re-send the recorded inputs, then assert the
    /// recorded screen-state hash. Every failure here is first-class.
    fn replay(&mut self, step: usize, op: Op, text: &str, path: &Path) {
        let Some(recording) = Recording::load(path) else {
            panic!(
                "agent step {step} ({} {text:?}) has no recording at {}\n\
                 Recordings are committed authored assets (Spec E01): write the \
                 .jsonl by hand, or run the test once with `AIT_E2E_RECORD=1` and a \
                 resolving driver (`E2e::set_driver`, or `AIT_E2E_DRIVER`'s \
                 Command backing).",
                op.as_str(),
                path.display()
            )
        };
        let header = &recording.header;
        assert!(
            header.op == op && header.text_hash == recording::text_hash(op, text),
            "agent step {step} at {} was recorded as {} {:?} but the test now \
             says {} {text:?} — editing a step's text invalidates its recording; \
             re-record with `AIT_E2E_RECORD=1`",
            path.display(),
            header.op.as_str(),
            header.text,
            op.as_str(),
        );
        for input in &recording.inputs {
            send_input(self.e2e, input);
        }
        self.await_recorded(step, op, text, path, &recording);
        if let Some(Verdict::Fail { reason }) = &header.verdict {
            panic!(
                "agent step {step} (assert {text:?}) replays a recording whose \
                 verdict is fail: {reason} — re-record with `AIT_E2E_RECORD=1`"
            );
        }
    }

    /// Wait — bounded by [`AWAIT_TIMEOUT`] — for the live screen to match
    /// the recording's screen-state hash. Exits early on the states that
    /// can never heal: a different grid size, a dead child, or a screen
    /// stable for [`DRIFT_STABLE`] while still different. Any miss is
    /// drift: a first-class failure carrying the screen diff.
    fn await_recorded(
        &mut self,
        step: usize,
        op: Op,
        text: &str,
        path: &Path,
        recording: &Recording,
    ) {
        let recorded = &recording.header.screen;
        let start = Instant::now();
        let mut last = ScreenState::capture(self.e2e);
        let mut last_change = Instant::now();
        loop {
            let screen = ScreenState::capture(self.e2e);
            if screen.hash_hex() == recording.header.screen_hash {
                return;
            }
            let reason = if (screen.columns, screen.lines) != (recorded.columns, recorded.lines) {
                Some("the grid size differs and cannot heal".to_string())
            } else if self.e2e.exited().is_some() {
                Some("the program exited before the screen matched".to_string())
            } else if screen != last {
                last = screen;
                last_change = Instant::now();
                None
            } else if last_change.elapsed() >= DRIFT_STABLE {
                Some(format!(
                    "the screen has been stable for {DRIFT_STABLE:?} without matching"
                ))
            } else if start.elapsed() >= AWAIT_TIMEOUT {
                Some(format!(
                    "timed out after {AWAIT_TIMEOUT:?} waiting for the recorded screen"
                ))
            } else {
                None
            };
            if let Some(reason) = reason {
                let actual = ScreenState::capture(self.e2e);
                panic!(
                    "agent step {step} ({} {text:?}) DRIFTED: {reason}.\n\
                     The live screen no longer matches the recording at {}.\n\
                     --- recorded screen ---\n{}\n\
                     --- actual screen ---\n{}\n\
                     --- diff (- recorded, + actual) ---\n{}\
                     If this change is intended, re-record with `AIT_E2E_RECORD=1`.",
                    op.as_str(),
                    path.display(),
                    recorded.rows.join("\n"),
                    actual.rows.join("\n"),
                    screen::diff(recorded, &actual),
                );
            }
            std::thread::sleep(POLL);
            self.e2e.pump();
        }
    }
}

/// Send one semantic [`Input`] through the harness's real encoding
/// path — `press` (parse chord → `encode_key`), `type_text`, or
/// `send_bytes`.
fn send_input(e2e: &E2e, input: &Input) {
    match input {
        Input::Key { key } => e2e.press(key),
        Input::Text { text } => e2e.type_text(text),
        Input::Bytes { bytes } => e2e.send_bytes(bytes),
    }
}

/// Pump until the screen is unchanged for [`SETTLE_STABLE`] (or
/// [`SETTLE_BUDGET`] elapses) and return the captured state — the
/// "after the step" screen a recording hashes.
fn settle(e2e: &mut E2e) -> ScreenState {
    let start = Instant::now();
    let mut last = ScreenState::capture(e2e);
    let mut last_change = Instant::now();
    loop {
        e2e.pump();
        let screen = ScreenState::capture(e2e);
        if screen != last {
            last = screen;
            last_change = Instant::now();
        } else if last_change.elapsed() >= SETTLE_STABLE || start.elapsed() >= SETTLE_BUDGET {
            return screen;
        }
        std::thread::sleep(POLL);
    }
}

/// Read once at spawn: is `AIT_E2E_RECORD` set to a truthy value?
pub(crate) fn record_flag() -> bool {
    match std::env::var("AIT_E2E_RECORD") {
        Ok(value) => !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}
