//! Contract tests for the shared HTTP machinery (Spec R03 us-30..33):
//! bounded retry on 429/5xx/transport with `Retry-After` honored and
//! capped, terminal other-4xx, prompt cancellation mid-stream and
//! mid-backoff — all against the in-process fixture server.
//!
//! Backoff parameters are injected through the crate's `#[doc(hidden)]`
//! `testing::Retry` seam so the suite never sleeps real seconds. The
//! mechanics are adapter-agnostic; `openai_chat_completions` stands in.

mod support;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ait_llm::openai_chat_completions as occ;
use ait_llm::{testing::Retry, EndpointConfig, EndpointSchema, LlmError, LlmProvider};
use futures_util::StreamExt;
use support::{error_status, rate_limited, sse, Fixture, Script};
use tokio_util::sync::CancellationToken;

/// A provider pointed at the fixture with an injected retry policy.
fn provider(base_url: &str, retry: Retry) -> occ::Provider {
    occ::Provider::with_retry(
        EndpointConfig {
            base_url: base_url.into(),
            model: "gpt-6-astra".into(),
            secret: Some("sk-test".into()),
            schema: EndpointSchema::OpenAiChatCompletions,
        },
        retry,
    )
    .unwrap()
}

fn request() -> occ::Request {
    occ::Request {
        model: "gpt-6-astra".into(),
        messages: vec![occ::Message::User {
            content: "hi".into(),
        }],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        stream: true,
    }
}

/// Spec R03's production policy for reference-sized waits the tests
/// actually need: near-zero backoff, generous `Retry-After` cap.
fn fast_retry() -> Retry {
    Retry {
        max_attempts: 3,
        base: Duration::from_millis(1),
        retry_after_cap: Duration::from_secs(10),
    }
}

const BOUND: Duration = Duration::from_secs(10);
const OPENAI_429: &str =
    "{\"error\":{\"message\":\"Rate limit reached.\",\"type\":\"tokens\",\"code\":\"rate_limit_exceeded\"}}";

#[tokio::test]
async fn a_429_with_retry_after_is_retried_after_the_header_s_delay() {
    // 429 → 200. With the exponential base inflated to 10 s, only
    // `Retry-After: 0.15` can explain a sub-second retry — the header was
    // honored, not the backoff.
    let fixture = Fixture::serve(vec![
        rate_limited("0.15", OPENAI_429),
        sse(include_str!("fixtures/openai_chat_completions_text.sse")),
    ]);
    let provider = provider(
        &fixture.base_url,
        Retry {
            max_attempts: 3,
            base: Duration::from_secs(10),
            retry_after_cap: Duration::from_secs(10),
        },
    );

    let started = Instant::now();
    let stream = tokio::time::timeout(BOUND, provider.stream(request(), CancellationToken::new()))
        .await
        .expect("retry took longer than the Retry-After wait")
        .expect("stream should succeed after the 429");

    assert!(started.elapsed() >= Duration::from_millis(150));
    assert!(started.elapsed() < Duration::from_secs(5));
    let events: Vec<_> = stream.collect().await;
    assert!(matches!(
        events.first(),
        Some(Ok(occ::StreamEvent::TextDelta { .. }))
    ));
    fixture.captured();
    fixture.captured();
    fixture.assert_no_more_requests();
}

#[tokio::test]
async fn a_5xx_is_retried_with_exponential_backoff() {
    // 500 → 200. Base 30 ms: first backoff is 30–60 ms (plus jitter), so
    // elapsed ≥ 30 ms proves the wait happened and < 5 s proves it was
    // the injected base, not the 250 ms default.
    let fixture = Fixture::serve(vec![
        error_status(500, "{\"error\":{\"message\":\"boom\"}}"),
        sse(include_str!("fixtures/openai_chat_completions_text.sse")),
    ]);
    let provider = provider(
        &fixture.base_url,
        Retry {
            max_attempts: 3,
            base: Duration::from_millis(30),
            retry_after_cap: Duration::from_secs(10),
        },
    );

    let started = Instant::now();
    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("stream should succeed after the 500");
    let events: Vec<_> = stream.collect().await;
    assert!(matches!(events.first(), Some(Ok(_))));

    assert!(started.elapsed() >= Duration::from_millis(30));
    assert!(started.elapsed() < Duration::from_secs(5));
    fixture.captured();
    fixture.captured();
    fixture.assert_no_more_requests();
}

#[tokio::test]
async fn retry_after_is_capped() {
    // `Retry-After: 30` with a 200 ms cap: the retry waits ~200 ms, not
    // 30 s (and not the 10 s base either).
    let fixture = Fixture::serve(vec![
        rate_limited("30", OPENAI_429),
        sse(include_str!("fixtures/openai_chat_completions_text.sse")),
    ]);
    let provider = provider(
        &fixture.base_url,
        Retry {
            max_attempts: 3,
            base: Duration::from_secs(10),
            retry_after_cap: Duration::from_millis(200),
        },
    );

    let started = Instant::now();
    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("stream should succeed after the capped wait");
    let events: Vec<_> = stream.collect().await;
    assert!(matches!(events.first(), Some(Ok(_))));

    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn attempts_exhaust_after_three_tries_and_surface_the_last_error() {
    let fixture = Fixture::serve(vec![
        rate_limited("0", OPENAI_429),
        rate_limited("0", OPENAI_429),
        rate_limited("0", OPENAI_429),
    ]);
    let provider = provider(&fixture.base_url, fast_retry());

    // `unwrap_err` needs the Ok side to be Debug — EventStream isn't, so
    // go through `err()`.
    let err = provider
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("three 429s should fail");

    // us-31: at most 3 attempts total; the last failure is what surfaces.
    match &err {
        LlmError::HttpStatus { status, body } => {
            assert_eq!(*status, reqwest::StatusCode::TOO_MANY_REQUESTS);
            assert!(body.contains("Rate limit reached"), "{body}");
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(err.is_retryable(), "429 stays classified as transient");
    fixture.captured();
    fixture.captured();
    fixture.captured();
    fixture.assert_no_more_requests();
}

#[tokio::test]
async fn other_4xx_fail_immediately_without_retry() {
    // us-32: a bad key or model name surfaces fast — one request only.
    let fixture = Fixture::serve(vec![error_status(
        400,
        "{\"error\":{\"message\":\"'model' is required\",\"type\":\"invalid_request_error\"}}",
    )]);
    let provider = provider(&fixture.base_url, fast_retry());

    let err = provider
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a 400 should fail");

    match err {
        LlmError::HttpStatus { status, body } => {
            assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
            assert!(body.contains("'model' is required"), "{body}");
        }
        other => panic!("unexpected error: {other}"),
    }
    fixture.captured();
    fixture.assert_no_more_requests();
}

#[tokio::test]
async fn transport_errors_are_retried_then_surface() {
    // Nothing listens on this port — connection refused is a transport
    // error (us-31's third retried class).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = listener.local_addr().unwrap().port();
    drop(listener);
    let provider = provider(&format!("http://127.0.0.1:{dead_port}"), fast_retry());

    let err = tokio::time::timeout(BOUND, provider.stream(request(), CancellationToken::new()))
        .await
        .expect("transport retry loop hung")
        .err()
        .expect("a dead port should fail");

    assert!(matches!(err, LlmError::Transport(_)), "{err:?}");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn cancellation_mid_stream_terminates_promptly() {
    // A drip-fed SSE body stalls after the first event; cancelling while
    // the pump waits must end the stream with `Cancelled` — us-30.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let fixture = Fixture::serve(vec![Script::DripSse(rx)]);
    let provider = provider(&fixture.base_url, fast_retry());
    let token = CancellationToken::new();

    // The payload must be sent before `stream()` — tiny_http only
    // flushes the 200's headers once body bytes flow, so awaiting the
    // stream before sending would deadlock. And it has to exceed the
    // ~8 KiB of response buffering or it never reaches the client (see
    // `Script::DripSse`): the real event goes first, a spec-legal
    // `:`-comment line pads the write past the buffers, and the parser's
    // incomplete-line handling holds the truncated comment silently.
    let mut first_payload =
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n"
            .to_vec();
    first_payload.extend_from_slice(format!(": {}\n", "x".repeat(9 * 1024)).as_bytes());
    tx.send(first_payload).unwrap();

    let mut stream = provider.stream(request(), token.clone()).await.unwrap();

    let first = tokio::time::timeout(BOUND, stream.next())
        .await
        .expect("first event never arrived")
        .expect("stream ended early")
        .unwrap();
    assert!(matches!(first, occ::StreamEvent::TextDelta { text } if text == "Hel"));

    token.cancel();
    let cancelled = tokio::time::timeout(BOUND, stream.next())
        .await
        .expect("cancellation did not terminate the stream")
        .expect("cancelled stream should yield one last item");
    assert!(matches!(cancelled, Err(LlmError::Cancelled)));
    // And then it's over — no lingering tail.
    assert!(tokio::time::timeout(BOUND, stream.next())
        .await
        .expect("stream did not end after Cancelled")
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn a_stream_silent_past_the_idle_timeout_fails() {
    // us-33: 60 s between events is the wedge bound. `start_paused` makes
    // tokio's clock jump straight to the deadline — real time spent is ms.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let fixture = Fixture::serve(vec![Script::DripSse(rx)]);
    let provider = provider(&fixture.base_url, fast_retry());

    // Same trick as the cancellation test: the first payload must exceed
    // tiny_http's ~8 KiB response buffering to reach the client at all.
    let mut first_payload =
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n"
            .to_vec();
    first_payload.extend_from_slice(format!(": {}\n", "x".repeat(9 * 1024)).as_bytes());
    tx.send(first_payload).unwrap();

    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    // Let the socket write land before the clock can jump past the data.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let first = stream.next().await.expect("stream ended early").unwrap();
    assert!(matches!(first, occ::StreamEvent::TextDelta { text } if text == "Hel"));

    // The server never sends again: the pump's idle deadline is the only
    // pending timer, so the paused clock jumps to it.
    let timed_out = stream.next().await.expect("stream ended early");
    match timed_out {
        Err(LlmError::StreamTimeout { idle }) => {
            assert_eq!(idle, Duration::from_secs(60))
        }
        other => panic!("expected StreamTimeout, got {other:?}"),
    }
}

#[tokio::test]
async fn cancellation_mid_backoff_terminates_promptly() {
    // A lone 429 puts the adapter into a 5 s backoff; cancelling ~100 ms
    // in must abort the sleep, not ride it out — us-30.
    let fixture = Fixture::serve(vec![rate_limited("0", OPENAI_429)]);
    let provider = provider(
        &fixture.base_url,
        Retry {
            max_attempts: 3,
            base: Duration::from_secs(5),
            retry_after_cap: Duration::from_secs(10),
        },
    );
    let token = CancellationToken::new();

    let request = provider.stream(request(), token.clone());
    tokio::pin!(request);
    // Drive the future until the request is out and the 429 has landed —
    // 200 ms of polling puts the adapter inside its 5 s backoff sleep.
    tokio::select! {
        _ = &mut request => panic!("stream returned before backoff"),
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
    }
    fixture.captured();
    token.cancel();

    let started = Instant::now();
    let err = tokio::time::timeout(BOUND, &mut request)
        .await
        .expect("cancellation did not interrupt the backoff");
    assert!(matches!(err, Err(LlmError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(2));
}
