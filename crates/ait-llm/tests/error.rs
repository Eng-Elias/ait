//! Tests at the `LlmError` seam (Spec R03 acceptance: "variants distinguish
//! transport / HTTP-status+body / SSE-parse / cancelled — so callers can
//! decide retryability without re-inspecting internals").

use ait_llm::{EndpointSchema, LlmError};

#[tokio::test]
async fn transport_errors_are_retryable() {
    // A malformed URL fails inside reqwest without touching the network.
    let err = reqwest::Client::new().get("not a url").build().unwrap_err();
    let err = LlmError::from(err);
    assert!(matches!(err, LlmError::Transport(_)), "{err:?}");
    assert!(err.is_retryable());
}

#[test]
fn http_status_carries_the_code_and_a_body_excerpt() {
    let err = LlmError::HttpStatus {
        status: reqwest::StatusCode::TOO_MANY_REQUESTS,
        body: "rate limited, try later".into(),
    };
    let message = err.to_string();
    assert!(message.contains("429"), "{message}");
    assert!(message.contains("rate limited"), "{message}");
    // us-31: 429/5xx are transient; us-32: other 4xx fail fast.
    assert!(err.is_retryable());
    for status in [
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        reqwest::StatusCode::BAD_GATEWAY,
    ] {
        let err = LlmError::HttpStatus {
            status,
            body: String::new(),
        };
        assert!(err.is_retryable(), "{status} should retry");
    }
    for status in [
        reqwest::StatusCode::UNAUTHORIZED,
        reqwest::StatusCode::FORBIDDEN,
        reqwest::StatusCode::NOT_FOUND,
    ] {
        let err = LlmError::HttpStatus {
            status,
            body: String::new(),
        };
        assert!(!err.is_retryable(), "{status} should be terminal");
    }
}

#[test]
fn stream_timeout_is_retryable_but_parse_and_cancelled_are_terminal() {
    // us-33's idle-timeout is a wedged connection — transient like a
    // transport error; the caller may re-issue.
    assert!(LlmError::StreamTimeout {
        idle: std::time::Duration::from_secs(60),
    }
    .is_retryable());
    assert!(!LlmError::Parse("bad frame".into()).is_retryable());
    assert!(!LlmError::Cancelled.is_retryable());
    assert!(!LlmError::SchemaMismatch {
        endpoint: EndpointSchema::OpenAiResponses,
        request: EndpointSchema::OpenAiChatCompletions,
    }
    .is_retryable());
}
