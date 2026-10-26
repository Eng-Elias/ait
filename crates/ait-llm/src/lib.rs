//! `ait-llm` — AIT's provider abstraction layer (REQUIREMENTS.md §5.3).
//!
//! One [`LlmProvider`] trait fronts three endpoint-schema adapters —
//! [`openai_chat_completions`], [`openai_responses`] and
//! [`anthropic_messages`] — each keeping its own message IR: request,
//! messages, tool calls and typed stream events stay per-schema and are not
//! unified (§5.3's "do not unify prematurely", CODING_STANDARDS §3).
//!
//! Consumers usually go through [`Endpoint`]: [`Endpoint::new`] is the
//! single construction path from an [`EndpointConfig`] (base URL, wire
//! model name, optional secret, schema — owned here, never from
//! `ait-config`), and [`Endpoint::stream`] dispatches a tagged
//! [`EndpointRequest`] to the matching adapter. Cancellation rides a
//! [`tokio_util::sync::CancellationToken`] per request (us-30).
//!
//! `tokio` stays inside this crate — `ait` and `ait-term` remain
//! synchronous (ADR-0004, Spec R03). The adapters run real HTTP+SSE:
//! `reqwest` requests with per-schema auth (omitted entirely without a
//! secret, us-21), bounded retry on 429/5xx/transport errors honoring
//! `Retry-After` (us-31), a 10 s connect timeout plus a 60 s stream
//! idle-timeout with no whole-request deadline (us-33), and cancellation
//! via a per-request [`tokio_util::sync::CancellationToken`] (us-30).

pub mod anthropic_messages;
mod endpoint;
mod error;
mod http;
pub mod openai_chat_completions;
pub mod openai_responses;
mod provider;

pub use endpoint::{
    Endpoint, EndpointConfig, EndpointRequest, EndpointSchema, EndpointStream, EndpointStreamEvent,
};
pub use error::LlmError;
pub use provider::{EventStream, LlmProvider};

/// Internal seams exposed for the crate's contract tests — not public
/// API and subject to change without notice.
#[doc(hidden)]
pub mod testing {
    pub use crate::http::Retry;
}
