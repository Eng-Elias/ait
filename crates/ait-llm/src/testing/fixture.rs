//! In-process HTTP fixture server for contract and integration tests
//! (Spec R03 us-34): a scripted `tiny_http` server on a `std::thread` —
//! the real HTTP+SSE path over loopback, zero external network. Recorded
//! SSE transcripts live under `tests/fixtures/` and are replayed
//! verbatim. Behind the `test-fixtures` feature so `tiny_http` never
//! reaches a normal build.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tiny_http::{Header, Method, Response, Server, StatusCode};

/// One scripted response in the server's queue — one entry per incoming
/// request, consumed in order.
pub enum Script {
    /// A complete response: status, extra headers, whole body.
    Respond {
        /// HTTP status code.
        status: u16,
        /// Extra response headers (e.g. `Retry-After`, `Content-Type`).
        headers: Vec<(String, String)>,
        /// The body — a recorded SSE transcript or an error payload.
        body: String,
    },
    /// A `200 text/event-stream` response whose body is fed chunk by
    /// chunk through a channel — for tests that must stall mid-stream
    /// (cancellation, idle). Sending no more bytes keeps the reply open.
    ///
    /// tiny_http writes through a chunked encoder buffered at 8 KiB plus
    /// a 1 KiB `BufWriter`: a channel payload smaller than ~8 KiB sits in
    /// those buffers and never reaches the client until EOF. Payloads
    /// that must arrive before the stall have to exceed 8 KiB — an SSE
    /// `:`-comment line is spec-legal padding for that.
    DripSse(Receiver<Vec<u8>>),
}

/// A 200 SSE response replaying a recorded transcript.
pub fn sse(body: &str) -> Script {
    Script::Respond {
        status: 200,
        headers: vec![("Content-Type".into(), "text/event-stream".into())],
        body: body.to_string(),
    }
}

/// A non-2xx response with a JSON error body.
pub fn error_status(status: u16, body: &str) -> Script {
    Script::Respond {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: body.to_string(),
    }
}

/// A 429 response honoring the `Retry-After` contract.
pub fn rate_limited(retry_after_secs: &str, body: &str) -> Script {
    Script::Respond {
        status: 429,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("Retry-After".into(), retry_after_secs.into()),
        ],
        body: body.to_string(),
    }
}

/// What the adapter actually sent — captured on the server side so the
/// test can assert method, path, auth headers and body.
#[derive(Debug)]
pub struct CapturedRequest {
    /// The request line's method.
    pub method: String,
    /// The request path (`/chat/completions`, …).
    pub url: String,
    /// All request headers, lower-cased names for matching.
    pub headers: Vec<(String, String)>,
    /// The request body.
    pub body: String,
}

impl CapturedRequest {
    /// The value of a header (case-insensitive name), if present.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A `Read` fed by a channel: `Ok(bytes)` appends to the buffer, a
/// dropped sender is EOF, and a live-but-silent sender blocks — which is
/// exactly the mid-stream stall the cancellation tests need.
struct ChannelRead {
    rx: Receiver<Vec<u8>>,
    buf: VecDeque<u8>,
}

impl Read for ChannelRead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.buf.extend(chunk),
                Err(_) => return Ok(0),
            }
        }
        let n = self.buf.len().min(out.len());
        for slot in out.iter_mut().take(n) {
            *slot = self.buf.pop_front().expect("n is bounded by buf len");
        }
        Ok(n)
    }
}

/// A scripted HTTP server on loopback plus the request-capture channel.
pub struct Fixture {
    /// `http://127.0.0.1:<port>` — the `base_url` adapters are pointed at.
    pub base_url: String,
    captured: Receiver<CapturedRequest>,
    _thread: JoinHandle<()>,
}

impl Fixture {
    /// Start the server replaying `script`, one response per request.
    /// Runs on a daemon thread; it exits with the test process.
    pub fn serve(script: Vec<Script>) -> Self {
        let server = Server::http("127.0.0.1:0").expect("bind loopback fixture server");
        let addr = server
            .server_addr()
            .to_ip()
            .expect("fixture server listens on IP");
        let (tx, captured) = mpsc::channel::<CapturedRequest>();
        let mut scripts: VecDeque<Script> = script.into();
        let thread = thread::spawn(move || {
            while let Some(script) = scripts.pop_front() {
                let Ok(mut request) = server.recv() else {
                    break;
                };
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let _ = tx.send(CapturedRequest {
                    method: request.method().to_string(),
                    url: request.url().to_string(),
                    headers: request
                        .headers()
                        .iter()
                        .map(|h| (h.field.to_string(), h.value.as_str().to_string()))
                        .collect(),
                    body,
                });
                match script {
                    Script::Respond {
                        status,
                        headers,
                        body,
                    } => {
                        let mut response =
                            Response::from_string(body).with_status_code(StatusCode(status));
                        for (name, value) in headers {
                            response.add_header(
                                Header::from_bytes(name, value)
                                    .expect("scripted header must be ASCII"),
                            );
                        }
                        let _ = request.respond(response);
                    }
                    Script::DripSse(rx) => {
                        let response = Response::new(
                            StatusCode(200),
                            vec![Header::from_bytes("Content-Type", "text/event-stream")
                                .expect("ASCII header")],
                            ChannelRead {
                                rx,
                                buf: VecDeque::new(),
                            },
                            None,
                            None,
                        );
                        let _ = request.respond(response);
                    }
                }
            }
        });
        Fixture {
            base_url: format!("http://{addr}"),
            captured,
            _thread: thread,
        }
    }

    /// The request captured for the next scripted response — fails the
    /// test after `BOUND` if the adapter never sent it.
    pub fn captured(&self) -> CapturedRequest {
        self.captured
            .recv_timeout(BOUND)
            .expect("adapter did not send the expected request")
    }

    /// Asserts no (further) request arrived within a short grace period —
    /// e.g. after a terminal error or a cancelled request.
    pub fn assert_no_more_requests(&self) {
        assert!(
            self.captured
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "adapter sent an unexpected extra request"
        );
    }

    /// How many requests have been captured so far, non-blocking.
    pub fn captured_count(&self) -> usize {
        let mut count = 0;
        while self.captured.try_recv().is_ok() {
            count += 1;
        }
        count
    }
}

/// Generous bound on every "wait for the adapter" — loopback is fast;
/// a hang is a failure, not a slow pass.
const BOUND: Duration = Duration::from_secs(10);

/// POST is the only method any adapter uses.
pub fn assert_post(captured: &CapturedRequest) {
    assert_eq!(captured.method, Method::Post.to_string());
}
