//! [`LlmError`]: why a request or its stream failed (Spec R03).

use crate::EndpointSchema;

/// Why an LLM request or stream failed.
///
/// The variants are the failure classes callers act on (Spec R03):
/// [`Transport`](LlmError::Transport), [`HttpStatus`](LlmError::HttpStatus)
/// with a body excerpt, [`Parse`](LlmError::Parse) for SSE/JSON decode
/// failures, and [`Cancelled`](LlmError::Cancelled) — so retryability is a
/// `match`, not a dive into internals.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LlmError {
    /// Transport-level failure: connect/TLS/reset errors and timeouts.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// The endpoint answered with a non-2xx status.
    #[error("endpoint returned HTTP {status}: {body}")]
    HttpStatus {
        /// The response status code.
        status: reqwest::StatusCode,
        /// A bounded excerpt of the response body.
        body: String,
    },
    /// An SSE frame or JSON payload could not be decoded.
    #[error("failed to decode the provider event stream: {0}")]
    Parse(String),
    /// The request IR's schema does not match the endpoint's schema — the
    /// caller paired an [`EndpointRequest`](crate::EndpointRequest) variant
    /// with the wrong [`Endpoint`](crate::Endpoint).
    #[error("request schema {request} does not match endpoint schema {endpoint}")]
    SchemaMismatch {
        /// The endpoint's schema.
        endpoint: EndpointSchema,
        /// The request's schema.
        request: EndpointSchema,
    },
    /// The request was aborted through its
    /// [`CancellationToken`](tokio_util::sync::CancellationToken) (us-30).
    #[error("request cancelled")]
    Cancelled,
    /// The adapter's HTTP/SSE machinery is not implemented yet — the
    /// R03-T5 stub sentinel.
    #[error("the {schema} adapter is not implemented yet")]
    NotImplemented {
        /// The schema whose adapter was called.
        schema: EndpointSchema,
    },
}

impl LlmError {
    /// Whether retrying the request might succeed (us-31/32): transport
    /// failures and HTTP 429/5xx are transient; everything else is
    /// terminal.
    ///
    /// This classifies an already-surfaced error — the adapter's own
    /// bounded retry (R03-T5) has already consumed its attempts by then.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::HttpStatus { status, .. } => {
                *status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
            }
            Self::Parse(_)
            | Self::SchemaMismatch { .. }
            | Self::Cancelled
            | Self::NotImplemented { .. } => false,
        }
    }
}
