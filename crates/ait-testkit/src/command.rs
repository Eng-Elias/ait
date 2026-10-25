//! The `Command` [`Driver`] backing (Spec E01, ticket E01-T3): any
//! program that speaks the versioned JSON step protocol becomes a
//! driver — a wrapper script around a model endpoint, a canned
//! responder, anything. Zero new machinery: `std::process` only, no
//! HTTP client, no async runtime. Secrets reach the subprocess through
//! the inherited environment alone (CODING_STANDARDS §4 — never in
//! tests, recordings, or the repo).
//!
//! `AIT_E2E_DRIVER=<command>` selects it: [`E2e::spawn`](crate::E2e::spawn)
//! installs a `Command` in place of the replay-only [`Authored`] default
//! when the variable names a command. The value runs through `sh -c`,
//! so a plain program path works (`AIT_E2E_DRIVER=./driver.sh`) and
//! shell syntax too (`AIT_E2E_DRIVER="my-driver --model fast"`).
//!
//! Per agent step a fresh subprocess is spawned — the protocol is
//! stateless, which keeps wrappers trivial: one JSON request on stdin,
//! EOF, one JSON response on stdout, exit 0. A stateful
//! subprocess-per-test protocol would buy little (a wrapper can keep
//! its own state in a file) and would cost lifecycle machinery.
//!
//! Wire format v1 — see `docs/e2e-driver-protocol.md` for the full
//! reference and an example wrapper:
//!
//! ```json
//! {"v":1,"kind":"act","goal":"open a new tab","screen":{"columns":80,"lines":24,"rows":["$ "],"cursor":{"line":0,"column":2,"shape":"Block"}}}
//! {"v":1,"inputs":[{"key":"Alt+T"},{"text":"echo hi"},{"bytes":"1b5b353e"}]}
//! ```
//!
//! Every deviation — spawn failure, non-zero exit, malformed JSON, a
//! wrong `v`, or a subprocess still running at [`STEP_TIMEOUT`] — fails
//! the step (a panic, per the [`Driver`] contract) with the child's
//! captured stderr in the message, which lands in the failure dump as
//! evidence.

use std::io::{self, Read, Write};
use std::process::{self, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::driver::{Driver, Input, Verdict};
use crate::screen::ScreenState;

/// The environment variable that selects this backing, read once at
/// [`E2e::spawn`](crate::E2e::spawn).
pub(crate) const DRIVER_ENV: &str = "AIT_E2E_DRIVER";

/// The wire-protocol version, carried in `v` on both the request and
/// the response. Bump on any incompatible change — a mismatch fails the
/// step loudly rather than silently misbehaving.
pub(crate) const PROTOCOL_VERSION: u32 = 1;

/// Bound on one subprocess's lifetime — generous next to a model round
/// trip; a stuck wrapper is killed and the step fails.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// Poll interval while waiting for the subprocess (matches the
/// harness's wait loops).
const POLL: Duration = Duration::from_millis(5);

/// Grace bound on draining the pipes after the child exits — normally
/// instant; bounded so a wrapper that leaks a grandchild holding a pipe
/// open can't hang the test.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Cap on bytes read from each of the child's pipes — a runaway child
/// printing forever must not eat memory; no legitimate response
/// approaches this.
const MAX_PIPE: u64 = 1 << 20; // 1 MiB

/// A [`Driver`] that resolves each `act`/`assert` step by running a
/// configured command as a fresh subprocess: the step's JSON request on
/// the child's stdin, its JSON response from stdout (module docs above
/// for the protocol). Install it with [`E2e::set_driver`](crate::E2e::set_driver)
/// or by setting `AIT_E2E_DRIVER` before spawn.
///
/// # Panics
/// On any subprocess or protocol failure — that is the [`Driver`]
/// contract: a driver that cannot resolve is a test failure, and the
/// panic message (which carries the child's stderr) is the evidence.
pub struct Command {
    /// The `sh -c` command line — the `AIT_E2E_DRIVER` value verbatim.
    command: String,
    /// Per-step bound on the subprocess ([`STEP_TIMEOUT`] unless
    /// [`with_timeout`](Self::with_timeout) overrode it).
    timeout: Duration,
}

impl Command {
    /// A driver resolved by running `command` through `sh -c` once per
    /// agent step, with the default [`STEP_TIMEOUT`] bound.
    pub fn new(command: impl Into<String>) -> Self {
        Self::with_timeout(command, STEP_TIMEOUT)
    }

    /// [`new`](Self::new) with an explicit per-step timeout — mostly
    /// for tests that exercise the kill path without sitting on the
    /// real bound.
    pub fn with_timeout(command: impl Into<String>, timeout: Duration) -> Self {
        Self {
            command: command.into(),
            timeout,
        }
    }

    /// The driver `AIT_E2E_DRIVER` names at spawn, or `None` when the
    /// variable is unset or blank (a blank value is a configuration
    /// no-op, not a command).
    pub(crate) fn from_env() -> Option<Self> {
        let value = std::env::var(DRIVER_ENV).ok()?;
        let command = value.trim();
        if command.is_empty() {
            None
        } else {
            Some(Self::new(command))
        }
    }

    /// One stateless exchange: spawn `sh -c <command>`, feed `body` to
    /// its stdin, and read both pipes to EOF within `self.timeout`.
    /// Returns `(stdout, stderr)` on a clean exit; every deviation is a
    /// step failure with stderr in the message.
    fn exchange(&self, call: &Call, body: &[u8]) -> (Vec<u8>, String) {
        let mut child = match process::Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => self.fail(call, &format!("could not spawn `sh -c`: {e}"), ""),
        };

        // The request, then EOF — the protocol's framing. A write error
        // means the child died early; the exit-status path reports it.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body);
        }

        // Reader threads drain both pipes concurrently so a chatty
        // wrapper can't deadlock on a full stderr buffer.
        let stdout = drain(child.stdout.take().expect("stdout is piped"));
        let stderr = drain(child.stderr.take().expect("stderr is piped"));

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => self.fail(call, &format!("cannot poll the subprocess: {e}"), ""),
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = String::from_utf8_lossy(&collect(&stderr)).into_owned();
                self.fail(
                    call,
                    &format!(
                        "timed out after {:?} — the subprocess was killed",
                        self.timeout
                    ),
                    &stderr,
                );
            }
            thread::sleep(POLL);
        };

        let stdout = collect(&stdout);
        let stderr = String::from_utf8_lossy(&collect(&stderr)).into_owned();
        if !status.success() {
            self.fail(
                call,
                &format!("the subprocess exited with {status}"),
                &stderr,
            );
        }
        (stdout, stderr)
    }

    /// The `v` gate: parse the envelope first so a wrapper speaking a
    /// newer protocol — or printing something that isn't protocol JSON
    /// at all — fails clearly instead of misparsing.
    fn check_protocol(&self, call: &Call, stdout: &[u8], stderr: &str) {
        let envelope: Envelope = serde_json::from_slice(stdout).unwrap_or_else(|e| {
            self.fail(
                call,
                &format!(
                    "the response is not valid protocol JSON: {e}\n\
                     --- stdout ---\n{}",
                    String::from_utf8_lossy(stdout)
                ),
                stderr,
            )
        });
        if envelope.v != PROTOCOL_VERSION {
            self.fail(
                call,
                &format!(
                    "the response speaks protocol v{}, this ait-testkit reads v{PROTOCOL_VERSION}",
                    envelope.v
                ),
                stderr,
            );
        }
    }

    /// The step-failure shape (Spec E01: failure modes surface as test
    /// failures, stderr captured in evidence). Diverging — every call
    /// site is a panic path.
    fn fail(&self, call: &Call, problem: &str, stderr: &str) -> ! {
        panic!(
            "the Command driver {:?} failed to resolve the {} step {:?}: \
             {problem}\n--- stderr ---\n{stderr}",
            self.command, call.kind, call.text,
        )
    }
}

impl Driver for Command {
    fn act(&mut self, goal: &str, screen: &ScreenState) -> Vec<Input> {
        let call = Call {
            kind: "act",
            text: goal,
        };
        let body = serde_json::to_vec(&Request {
            v: PROTOCOL_VERSION,
            kind: "act",
            goal: Some(goal),
            claim: None,
            screen,
        })
        .expect("the request serializes");
        let (stdout, stderr) = self.exchange(&call, &body);
        self.check_protocol(&call, &stdout, &stderr);
        serde_json::from_slice::<ActResponse>(&stdout)
            .unwrap_or_else(|e| {
                self.fail(
                    &call,
                    &format!("the act response doesn't match the v1 schema: {e}"),
                    &stderr,
                )
            })
            .inputs
    }

    fn assert(&mut self, claim: &str, screen: &ScreenState) -> Verdict {
        let call = Call {
            kind: "assert",
            text: claim,
        };
        let body = serde_json::to_vec(&Request {
            v: PROTOCOL_VERSION,
            kind: "assert",
            goal: None,
            claim: Some(claim),
            screen,
        })
        .expect("the request serializes");
        let (stdout, stderr) = self.exchange(&call, &body);
        self.check_protocol(&call, &stdout, &stderr);
        serde_json::from_slice::<AssertResponse>(&stdout)
            .unwrap_or_else(|e| {
                self.fail(
                    &call,
                    &format!("the assert response doesn't match the v1 schema: {e}"),
                    &stderr,
                )
            })
            .verdict
    }
}

/// The step identity threaded through `exchange`/`check_protocol`/`fail`
/// — the failure messages name *which* call failed.
struct Call<'a> {
    /// `"act"` or `"assert"`.
    kind: &'a str,
    /// The goal or claim text.
    text: &'a str,
}

/// The request a step pipes to the subprocess's stdin — one JSON
/// object, then EOF (module docs for the wire format).
#[derive(Serialize)]
struct Request<'a> {
    /// [`PROTOCOL_VERSION`].
    v: u32,
    /// `"act"` or `"assert"`.
    kind: &'a str,
    /// The `act` goal — present iff `kind` is `"act"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    goal: Option<&'a str>,
    /// The `assert` claim — present iff `kind` is `"assert"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    claim: Option<&'a str>,
    /// The settled screen the step is resolved against.
    screen: &'a ScreenState,
}

/// The response envelope — `v` is validated before the body so version
/// drift reports as itself, not as a schema error.
#[derive(Deserialize)]
struct Envelope {
    /// Protocol version the wrapper speaks.
    v: u32,
}

/// `act` response: the semantic [`Input`]s to send — same serde shapes
/// as the recording format (`{"key":…}` / `{"text":…}` / `{"bytes":…}`).
#[derive(Deserialize)]
struct ActResponse {
    /// The inputs, sent in order.
    inputs: Vec<Input>,
}

/// `assert` response: `"pass"` or `{"fail":{"reason":…}}` — the
/// [`Verdict`] serde shape recordings store.
#[derive(Deserialize)]
struct AssertResponse {
    /// The driver's judgement.
    verdict: Verdict,
}

/// Read `reader` to EOF, capped at [`MAX_PIPE`].
fn read_capped(reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(MAX_PIPE).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Spawn a thread draining `reader` to EOF, delivering the bytes over
/// the returned channel.
fn drain(reader: impl Read + Send + 'static) -> Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(read_capped(reader));
    });
    rx
}

/// Collect a drained pipe — after the child exits its pipes hit EOF and
/// the readers finish promptly; [`DRAIN_GRACE`] bounds the wait so a
/// leaked grandchild holding a pipe open can't hang the test.
fn collect(rx: &Receiver<io::Result<Vec<u8>>>) -> Vec<u8> {
    match rx.recv_timeout(DRAIN_GRACE) {
        Ok(Ok(bytes)) => bytes,
        _ => Vec::new(),
    }
}
