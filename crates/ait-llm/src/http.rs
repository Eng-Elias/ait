//! The HTTP+SSE machinery shared by the three adapters (R03-T5): bounded
//! retry with `Retry-After`, request dispatch under a
//! [`CancellationToken`], and the event pump that turns a 2xx response
//! body into typed [`StreamEvent`](crate::LlmProvider::StreamEvent)s.
//!
//! Policy lives here, not in the adapters (Spec R03):
//!
//! - **us-31/32** — at most [`Retry::max_attempts`] total attempts;
//!   `base × 2ⁿ` backoff plus jitter; `Retry-After` honored capped at
//!   [`Retry::retry_after_cap`]; retried on 429/5xx and transport errors;
//!   every other failure is terminal.
//! - **us-30** — `cancel` aborts promptly: before dispatch, in flight and
//!   mid-backoff.
//! - **us-33** — [`STREAM_IDLE_TIMEOUT`] between stream events and no
//!   whole-request deadline (the 10 s connect timeout is set on the shared
//!   client, [`crate::endpoint::shared_client`]).

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures_util::{stream, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::provider::EventStream;
use crate::LlmError;

/// us-33: the stream may go quiet this long between events before it is
/// declared wedged. Long generations are not killed — only silence is.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bound on the non-2xx body excerpt kept in [`LlmError::HttpStatus`] — a
/// proxy or provider can return a whole HTML error page.
const BODY_EXCERPT_LIMIT: usize = 2048;

/// The bounded-retry policy (us-31): `max_attempts` total tries, `base ×
/// 2ⁿ` exponential backoff plus jitter, `Retry-After` honored capped at
/// `retry_after_cap`.
///
/// Fields are `pub` so contract tests (via the crate's `#[doc(hidden)]`
/// `testing` re-export) can inject compressed waits instead of sleeping
/// real seconds — an internal seam, not settings surface.
pub struct Retry {
    /// Total attempts allowed — the first try plus retries.
    pub max_attempts: u32,
    /// Backoff base: the wait after failed attempt `n` (0-indexed) is
    /// `base × 2ⁿ` plus up to one `base` of jitter.
    pub base: Duration,
    /// `Retry-After` is honored only up to this cap.
    pub retry_after_cap: Duration,
}

impl Default for Retry {
    /// Spec values: 3 attempts, 250 ms base, 10 s `Retry-After` cap.
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base: Duration::from_millis(250),
            retry_after_cap: Duration::from_secs(10),
        }
    }
}

impl Retry {
    /// The wait before retrying after failed attempt `n` (0-indexed):
    /// `retry_after` capped when the server sent one, else `base × 2ⁿ`
    /// plus `jitter` (a fraction of `base` in `[0, 1)`).
    pub(crate) fn delay(&self, n: u32, retry_after: Option<Duration>, jitter: f64) -> Duration {
        if let Some(after) = retry_after {
            return after.min(self.retry_after_cap);
        }
        let backoff = self.base.saturating_mul(2u32.saturating_pow(n));
        let jitter = self.base.mul_f64(jitter.clamp(0.0, 1.0));
        backoff.saturating_add(jitter)
    }
}

/// `POST {base_url}/{path}` — join honoring a `base_url` written with or
/// without a trailing slash (`base_url` carries any version prefix, §4.1).
pub(crate) fn url(base_url: &str, path: &str) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), path)
}

/// Issue `request` with bounded retry (us-31/32) and return the 2xx
/// response — its body is the SSE stream. Failure classes and `cancel`
/// behave as documented on the module.
///
/// Mid-stream failures never reach this function: once a 2xx is in hand
/// the response becomes the caller's [`EventStream`], and the retryable /
/// terminal decision for stream errors is the consumer's — streams are
/// not resumable, so a retry would be a brand-new request anyway.
pub(crate) async fn send_with_retry(
    client: &reqwest::Client,
    request: &reqwest::Request,
    retry: &Retry,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, LlmError> {
    let mut failed_attempts = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let attempt = request
            .try_clone()
            .expect("adapter request bodies are plain bytes — always cloneable");
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            result = client.execute(attempt) => result,
        };
        let (error, retry_after) = match outcome {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                let retry_after = retry_after(response.headers());
                let status = response.status();
                let body = body_excerpt(response, cancel).await?;
                (LlmError::HttpStatus { status, body }, retry_after)
            }
            Err(e) => (LlmError::Transport(e), None),
        };
        failed_attempts += 1;
        if !error.is_retryable() || failed_attempts >= retry.max_attempts {
            return Err(error);
        }
        let delay = retry.delay(failed_attempts - 1, retry_after, jitter_fraction());
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// `Retry-After` as a duration: both providers document whole seconds;
/// a float is accepted so injected delays can run sub-second, and the
/// rarer HTTP-date form (RFC 9110 §10.2.3) is parsed as a fallback.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(secs) = value.trim().parse::<f64>() {
        return (secs >= 0.0).then(|| Duration::from_secs_f64(secs));
    }
    httpdate::parse_http_date(value)
        .ok()?
        .duration_since(SystemTime::now())
        .ok()
}

/// A bounded excerpt of a non-2xx body for [`LlmError::HttpStatus`].
async fn body_excerpt(
    response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<String, LlmError> {
    let bytes = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(LlmError::Cancelled),
        body = response.bytes() => body.map_err(LlmError::Transport)?,
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut end = text.len().min(BODY_EXCERPT_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(text[..end].to_string())
}

/// Cheap non-crypto entropy for backoff jitter: a splitmix64 finalizer
/// over wall-clock nanos mixed with a counter so consecutive calls differ.
/// Jitter only decorrelates retry timing — it is never a secret.
fn jitter_fraction() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed));
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// What one SSE event (or end-of-input) decoded to.
pub(crate) struct Decoded<E> {
    /// Zero or more typed events — provider `error` payloads decode to
    /// `Ok(StreamEvent::Error)` items, machinery failures to `Err`.
    pub(crate) events: Vec<Result<E, LlmError>>,
    /// True once the schema's terminal marker was seen (`data: [DONE]`,
    /// `message_stop`, `response.completed`, …): the pump stops reading
    /// rather than waiting for the server to close the socket.
    pub(crate) finished: bool,
}

impl<E> Decoded<E> {
    /// Events decoded; the wire stream continues.
    pub(crate) fn more(events: Vec<Result<E, LlmError>>) -> Self {
        Self {
            events,
            finished: false,
        }
    }

    /// Final events; the wire stream is over even if the socket lingers.
    pub(crate) fn over(events: Vec<Result<E, LlmError>>) -> Self {
        Self {
            events,
            finished: true,
        }
    }
}

/// Turn a 2xx response into the adapter's [`EventStream`]: raw bytes →
/// `eventsource-stream` events → `decode` → typed events.
///
/// `decode` receives `Some(event)` per SSE frame and `None` when the body
/// ends, so decoders that hold back an event (the chat-completions
/// `Finish` waits for a trailing usage chunk) can flush it. `cancel`
/// wins mid-poll (us-30) and [`STREAM_IDLE_TIMEOUT`] fires when the
/// server goes quiet (us-33); there is deliberately no total deadline.
pub(crate) fn sse_stream<E, F>(
    response: reqwest::Response,
    cancel: CancellationToken,
    decode: F,
) -> EventStream<E>
where
    E: Send + 'static,
    F: FnMut(Option<Event>) -> Decoded<E> + Send + 'static,
{
    enum Step {
        Cancelled,
        TimedOut,
        Ended,
        Frame(Result<Event, EventStreamError<reqwest::Error>>),
    }

    let sse = response.bytes_stream().eventsource();
    // `decode` rides in the unfold state — a FnMut can't move a captured
    // variable into each future it produces.
    Box::pin(stream::unfold(
        (sse, cancel, VecDeque::new(), false, decode),
        |(mut sse, cancel, mut pending, mut closed, mut decode)| async move {
            loop {
                if let Some(item) = pending.pop_front() {
                    return Some((item, (sse, cancel, pending, closed, decode)));
                }
                if closed {
                    return None;
                }
                let step = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Step::Cancelled,
                    next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, sse.next()) => match next {
                        Err(_) => Step::TimedOut,
                        Ok(None) => Step::Ended,
                        Ok(Some(frame)) => Step::Frame(frame),
                    },
                };
                match step {
                    Step::Cancelled => {
                        closed = true;
                        pending.push_back(Err(LlmError::Cancelled));
                    }
                    Step::TimedOut => {
                        closed = true;
                        pending.push_back(Err(LlmError::StreamTimeout {
                            idle: STREAM_IDLE_TIMEOUT,
                        }));
                    }
                    Step::Ended => {
                        closed = true;
                        pending.extend(decode(None).events);
                    }
                    Step::Frame(Err(err)) => {
                        closed = true;
                        pending.push_back(Err(match err {
                            EventStreamError::Transport(e) => LlmError::Transport(e),
                            other => LlmError::Parse(other.to_string()),
                        }));
                    }
                    Step::Frame(Ok(event)) => {
                        let decoded = decode(Some(event));
                        pending.extend(decoded.events);
                        closed |= decoded.finished;
                    }
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_base_times_2_to_the_n() {
        let retry = Retry::default();
        assert_eq!(retry.delay(0, None, 0.0), Duration::from_millis(250));
        assert_eq!(retry.delay(1, None, 0.0), Duration::from_millis(500));
        assert_eq!(retry.delay(2, None, 0.0), Duration::from_millis(1000));
    }

    #[test]
    fn jitter_adds_up_to_one_base() {
        let retry = Retry::default();
        let d = retry.delay(0, None, 0.5);
        assert_eq!(d, Duration::from_millis(375));
        // Out-of-range fractions are clamped, not applied.
        assert_eq!(retry.delay(0, None, 9.0), Duration::from_millis(500));
    }

    #[test]
    fn retry_after_wins_over_backoff_and_is_capped() {
        let retry = Retry::default();
        assert_eq!(
            retry.delay(2, Some(Duration::from_secs(3)), 0.9),
            Duration::from_secs(3)
        );
        assert_eq!(
            retry.delay(2, Some(Duration::from_secs(60)), 0.9),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn retry_after_parses_seconds_and_http_dates() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "2".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(2)));
        headers.insert(reqwest::header::RETRY_AFTER, "0.05".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(50)));
        let date = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(7));
        headers.insert(reqwest::header::RETRY_AFTER, date.parse().unwrap());
        let parsed = retry_after(&headers).unwrap();
        assert!(parsed > Duration::from_secs(5) && parsed <= Duration::from_secs(8));
        headers.insert(reqwest::header::RETRY_AFTER, "garbage".parse().unwrap());
        assert_eq!(retry_after(&headers), None);
    }
}
