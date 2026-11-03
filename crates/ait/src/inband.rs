//! The in-band `__ait_run` request seam (REQUIREMENTS.md §5.5.1,
//! Spec R05 / issue #75).
//!
//! `ait` can ask the *live* shell to run a command invisibly — with the
//! user's env, aliases, functions and cwd — and collect the merged
//! stdout+stderr plus exit code back. The wire shape lives in
//! `ait_shell::run`: [`run_request`](ait_shell::run_request) types
//! `__ait_run <id> <hex(cmd)>` at the prompt (the line stays visible —
//! in-band exec always leaves a trace, Spec R05 us-23) and the
//! snippet's replies arrive as `OSC 7701` `A` … `R`×N … `B` frames,
//! which — like every private OSC — never render.
//!
//! [`InBandExec`] is the per-tab bookkeeping a `Tab` owns:
//!
//! - **request ids** — a monotonically increasing counter, handed out
//!   per tab and never reused;
//! - **gating** — [`request`](Self::request) refuses (and writes NOT A
//!   SINGLE BYTE) unless the tab's [`IntegrationState`] is
//!   `Integrated`: a shell without the snippet would run `__ait_run`
//!   as a bogus command and turn the caller's request into prompt
//!   noise;
//! - **the pending map** — `id → Sender<RunResult>`.
//!   [`observe`](Self::observe) feeds every decoded `7701` frame into
//!   ONE shared [`RunAccumulator`]: `R` frames carry no id — the wire
//!   can only ever hold one run at a time, so per-id accumulators
//!   would hold identical state. On `B`, the reassembled payload's
//!   `<id>` picks the waiter and the [`RunResult`] is delivered on
//!   its channel.
//!
//! **Round 6 seam:** `Tab::run_in_shell(command)` is the whole API the
//! in-band tier of `CommandExecutor` needs — it returns a
//! [`RunRequest`] whose [`Receiver`] the executor awaits
//! (`recv_timeout` bounds the wait by the step budget) or forwards
//! onto its own channel. Requests on one tab serialize on the wire —
//! an interactive shell runs one prompt line at a time — but several
//! may pend at once; each resolves by id. The `ShellBridge` service
//! (Spec R06-T4) is the first production caller: it hands the
//! request's result channel back to the agent worker, which polls it
//! alongside its cancellation token.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use ait_shell::{RunAccumulator, RunResult, ShellEvent};

use crate::integration::IntegrationState;

/// Per-tab `__ait_run` bookkeeping: the id counter, the shared frame
/// accumulator and the `id → waiter` map. Owned by `Tab`; fed from
/// `TabManager::drain_events` beside [`IntegrationState::observe`].
#[derive(Debug, Default)]
pub struct InBandExec {
    /// The id the next request gets — never reused within a tab, so a
    /// late response can never be misattributed to a later request.
    next_id: u64,
    /// Reassembles whichever `A…R*B` stream is on the wire (`R` frames
    /// are untagged and runs serialize — see the module doc).
    accumulator: RunAccumulator,
    /// In-flight requests: wire id → the waiter's channel end.
    pending: HashMap<String, Sender<RunResult>>,
}

impl InBandExec {
    /// An empty seam: no ids handed out, nothing pending.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the live shell to run `command`, returning the handle the
    /// [`RunResult`] arrives on.
    ///
    /// Refuses with [`InBandError::NotIntegrated`] unless `state` is
    /// `Integrated` — then `send` is never invoked, nothing is typed
    /// and no id is burned. On `Integrated` tabs the request line is
    /// written through `send` (the caller passes
    /// [`LiveSession::send_bytes`](ait_term::LiveSession::send_bytes))
    /// and the id pends until [`observe`](Self::observe) resolves it.
    /// A failed `send` burns the id — a gap in the sequence is
    /// harmless — but leaves nothing pending.
    pub fn request(
        &mut self,
        state: IntegrationState,
        command: &[u8],
        send: impl FnOnce(&[u8]) -> io::Result<()>,
    ) -> Result<RunRequest, InBandError> {
        if state != IntegrationState::Integrated {
            return Err(InBandError::NotIntegrated(state));
        }
        let id = self.next_id;
        self.next_id += 1;
        let key = id.to_string();
        send(&ait_shell::run_request(&key, command)).map_err(InBandError::Write)?;
        let (tx, rx) = channel();
        self.pending.insert(key, tx);
        Ok(RunRequest { id, result: rx })
    }

    /// Fold one decoded shell event into the channel — the
    /// `drain_events` bookkeeping that sits beside
    /// [`IntegrationState::observe`] and shares that drain's single
    /// `ait_shell::decode` per `Private` sequence. Only `OSC 7701`
    /// frames move it: `A`/`R`/`B` reassemble the in-flight stream,
    /// and a completed run resolves against the pending map by the
    /// payload's `<id>` — the [`RunResult`] goes to its waiter.
    ///
    /// Returns the completed result, or `None` for non-run events and
    /// torn streams. A completion whose id was never requested (stray
    /// traffic, a torn earlier stream) is dropped from the map but
    /// still returned so callers can log it.
    pub fn observe(&mut self, event: &ShellEvent) -> Option<RunResult> {
        let result = self.accumulator.feed(event)?;
        if let Some(waiter) = self.pending.remove(&result.id) {
            // A gone receiver only means the caller stopped waiting.
            let _ = waiter.send(result.clone());
        }
        Some(result)
    }

    /// Whether request `id` is still awaiting its response.
    #[allow(dead_code)] // only tests read it today; Round 6's executor can
                        // poll it for "still in flight" bookkeeping.
    pub fn is_pending(&self, id: u64) -> bool {
        self.pending.contains_key(&id.to_string())
    }

    /// The session died — drop every pending waiter so each
    /// `Receiver` reports `Disconnected` and its caller fails cleanly
    /// (a mid-run exit must be a tool error, never a hang — Spec R06
    /// us-34). `drain_events` calls it on `LiveEvent::Exited`.
    pub fn fail_pending(&mut self) {
        self.pending.clear();
    }
}

/// A `__ait_run` request in flight — the handle its [`RunResult`]
/// arrives on. [`id`](Self::id) matches the `<id>` typed on the
/// request line and echoed back inside the reassembled payload.
#[derive(Debug)]
pub struct RunRequest {
    id: u64,
    result: Receiver<RunResult>,
}

impl RunRequest {
    /// The request's id — the `<id>` in `__ait_run <id> <hex>`.
    #[allow(dead_code)] // tests assert id allocation; the bridge's reply
                        // carries the receiver only.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The channel the resolved [`RunResult`] arrives on: `recv` /
    /// `recv_timeout` per the caller's budget.
    #[allow(dead_code)] // the executor awaits through `into_receiver`.
    pub fn receiver(&self) -> &Receiver<RunResult> {
        &self.result
    }

    /// Consume the request into its result channel — the
    /// `ShellBridge` service's hand-off: the worker awaits the
    /// [`RunResult`] on its own thread while the frontend's drain
    /// keeps resolving it.
    pub fn into_receiver(self) -> Receiver<RunResult> {
        self.result
    }

    /// Block up to `timeout` for the response — the test-facing await.
    #[allow(dead_code)] // the executor awaits through `into_receiver`;
                        // tests keep the simpler handle.
    pub fn wait(&self, timeout: Duration) -> Result<RunResult, RecvTimeoutError> {
        self.result.recv_timeout(timeout)
    }
}

/// Why a `__ait_run` request was refused or failed before the shell
/// could answer it.
#[derive(Debug)]
#[non_exhaustive]
pub enum InBandError {
    /// The tab's shell isn't `Integrated` — integration absent or only
    /// `OSC 133` marks reported. No request bytes were written.
    NotIntegrated(IntegrationState),
    /// The session already exited: a typed request could never be
    /// answered, so nothing was written.
    SessionExited,
    /// Writing the request line to the PTY failed.
    Write(io::Error),
}

impl fmt::Display for InBandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotIntegrated(state) => write!(
                f,
                "cannot run in shell: tab's integration state is {state:?}, not Integrated"
            ),
            Self::SessionExited => {
                write!(f, "cannot run in shell: the session has exited")
            }
            Self::Write(e) => write!(f, "cannot run in shell: PTY write failed: {e}"),
        }
    }
}

impl std::error::Error for InBandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Write(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The pure seam: `send` is a recording spy and `observe` takes
    //! `SessionEvent::Private` values constructed in place — the same
    //! shape `feed_bytes` hands the drain. Real-PTY plumbing tests
    //! (typed line visible, frames resolving through `drain_events`,
    //! `7701` bytes never rendering) live in `tabs.rs`.
    use super::*;
    use std::cell::RefCell;

    /// One complete `A … R×N … B` response stream for `payload` — the
    /// events a well-formed `__ait_run` reply decodes to (wire-level
    /// coverage of those same frames lives in `ait-shell`'s codec
    /// tests). Chunks at the snippet's 500-byte slice so oversized
    /// payloads exercise the multi-frame path.
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

    /// A `send` that records every byte it was offered and succeeds.
    fn spy(sent: &RefCell<Vec<Vec<u8>>>) -> impl FnMut(&[u8]) -> io::Result<()> + '_ {
        move |bytes| {
            sent.borrow_mut().push(bytes.to_vec());
            Ok(())
        }
    }

    /// Request ids increase monotonically per tab and land on the wire
    /// as `__ait_run <id> <hex(cmd)>` (the `ait_shell::run_request`
    /// emit).
    #[test]
    fn request_ids_increase_and_type_the_request_line() {
        let mut exec = InBandExec::new();
        let sent = RefCell::new(Vec::new());

        let first = exec
            .request(IntegrationState::Integrated, b"echo a", spy(&sent))
            .unwrap();
        let second = exec
            .request(IntegrationState::Integrated, b"echo b", spy(&sent))
            .unwrap();

        assert_eq!(first.id(), 0);
        assert_eq!(second.id(), 1);
        let sent = sent.borrow();
        assert_eq!(sent[0], ait_shell::run_request("0", b"echo a"));
        assert_eq!(sent[1], ait_shell::run_request("1", b"echo b"));
    }

    /// `Absent` and `MarksOnly` tabs refuse cleanly: a
    /// `NotIntegrated` error, no bytes offered to `send`, and no id
    /// burned — the next `Integrated` request is still `0`.
    #[test]
    fn unintegrated_tabs_write_nothing() {
        for state in [IntegrationState::Absent, IntegrationState::MarksOnly] {
            let mut exec = InBandExec::new();
            let sent = RefCell::new(Vec::new());

            let err = exec
                .request(state, b"echo a", spy(&sent))
                .expect_err("a non-integrated tab refuses the request");
            assert!(
                matches!(err, InBandError::NotIntegrated(s) if s == state),
                "{err:?}"
            );
            assert!(sent.borrow().is_empty(), "refused requests type nothing");

            let ok = exec
                .request(IntegrationState::Integrated, b"x", spy(&sent))
                .unwrap();
            assert_eq!(ok.id(), 0, "a refused request does not burn an id");
        }
    }

    /// A failed write surfaces as `InBandError::Write` and leaves
    /// nothing pending — the request can never resolve.
    #[test]
    fn write_failures_report_and_pend_nothing() {
        let mut exec = InBandExec::new();
        let err = exec
            .request(IntegrationState::Integrated, b"echo", |_| {
                Err(io::Error::other("pty gone"))
            })
            .expect_err("a dead PTY write fails the request");

        assert!(matches!(err, InBandError::Write(_)), "{err:?}");
        assert!(!exec.is_pending(0));
    }

    /// A chunked response — several `R` frames — reassembles into one
    /// `RunResult` delivered on the request's channel: merged output
    /// keeps its own `;`s and the exit code parses off the tail.
    #[test]
    fn chunked_response_resolves_the_waiter() {
        let mut exec = InBandExec::new();
        let req = exec
            .request(IntegrationState::Integrated, b"echo hi", |_| Ok(()))
            .unwrap();
        assert!(exec.is_pending(0));

        let payload = b"0;merged out;with;semicolons\n;7";
        let mut completions = Vec::new();
        for event in run_stream(payload) {
            if let Some(done) = exec.observe(&event) {
                completions.push(done);
            }
        }

        let result = req.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.id, "0");
        assert_eq!(result.output, b"merged out;with;semicolons\n".to_vec());
        assert_eq!(result.exit_code, 7);
        assert_eq!(completions.len(), 1, "exactly the B completes the run");
        assert!(!exec.is_pending(0), "a resolved request leaves the map");
    }

    /// Several pending requests resolve independently — each `B`'s
    /// reassembled `<id>` picks its own waiter.
    #[test]
    fn pending_requests_resolve_by_id() {
        let mut exec = InBandExec::new();
        let r0 = exec
            .request(IntegrationState::Integrated, b"a", |_| Ok(()))
            .unwrap();
        let r1 = exec
            .request(IntegrationState::Integrated, b"b", |_| Ok(()))
            .unwrap();

        for event in run_stream(b"0;first;0") {
            exec.observe(&event);
        }
        for event in run_stream(b"1;second;3") {
            exec.observe(&event);
        }

        assert_eq!(r0.wait(Duration::from_secs(1)).unwrap().output, b"first");
        let second = r1.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(second.output, b"second");
        assert_eq!(second.exit_code, 3);
    }

    /// Failure paths degrade without panic: a `B` closing a malformed
    /// payload resolves nothing, a completion for an unrequested id
    /// drops but is still reported, and a mid-run `A` tears the
    /// previous stream — the pending request waits for a clean stream.
    #[test]
    fn malformed_and_stray_frames_never_resolve_wrongly() {
        let mut exec = InBandExec::new();
        let req = exec
            .request(IntegrationState::Integrated, b"echo", |_| Ok(()))
            .unwrap();

        // A `B` whose buffer is no `<id>;<out>;<code>` shape at all.
        let mut completions = Vec::new();
        for event in run_stream(b"garbage-without-semicolons") {
            completions.extend(exec.observe(&event));
        }
        assert!(
            completions.is_empty(),
            "a malformed payload completes nothing"
        );
        assert!(exec.is_pending(0));

        // A well-formed stream for an id nobody requested: reported to
        // the drain (Some) but delivered nowhere.
        let mut completions = Vec::new();
        for event in run_stream(b"99;stray;0") {
            completions.extend(exec.observe(&event));
        }
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].id, "99");
        assert!(exec.is_pending(0), "a stray id does not resolve 0");
        assert!(matches!(
            req.wait(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));

        // A torn stream for 0 — a second `A` resets the accumulator —
        // then 0's real response still resolves cleanly.
        for event in [
            ShellEvent::RunBegin,
            run_stream(b"0;cut-")[1].clone(),
            ShellEvent::RunBegin,
        ] {
            exec.observe(&event);
        }
        for event in run_stream(b"0;real;0") {
            exec.observe(&event);
        }
        assert_eq!(req.wait(Duration::from_secs(1)).unwrap().output, b"real");
    }

    /// Events that aren't `7701` frames — hook events, reserved codes,
    /// stray `R`/`B` outside a run — move nothing.
    #[test]
    fn non_run_events_are_inert() {
        let mut exec = InBandExec::new();
        let req = exec
            .request(IntegrationState::Integrated, b"echo", |_| Ok(()))
            .unwrap();

        // A real precmd hook through the wire decode (ait has no
        // serde_json dep to build `value` literals with).
        let precmd = {
            let hex: String =
                r#"{"hook":"precmd","value":{}}"#.bytes().map(|b| format!("{b:02x}")).collect();
            ait_shell::decode(b"7700", &[hex.into_bytes()]).unwrap()
        };
        for event in [
            precmd,
            ShellEvent::Reserved { code: 7702 },
            // Stray run frames outside an `A` bracket are inert.
            ShellEvent::RunChunk {
                payload: b"x".to_vec(),
            },
            ShellEvent::RunEnd,
        ] {
            assert_eq!(exec.observe(&event), None, "{event:?}");
        }
        assert!(exec.is_pending(0), "noise resolves nothing");
        let _ = req; // stays pending on drop — no hang, no panic
    }
}
