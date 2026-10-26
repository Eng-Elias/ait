//! The [`LlmProvider`] trait — one abstraction over the three endpoint
//! schemas (Spec R03 us-26).

use std::future::Future;
use std::pin::Pin;

use futures_util::Stream;
use tokio_util::sync::CancellationToken;

use crate::{EndpointSchema, LlmError};

/// A boxed stream of typed provider events (§5.3: "streaming (SSE) for all
/// three, including streamed tool calls").
///
/// `Err` items are machinery failures — a dropped connection, an
/// undecodable SSE frame, mid-stream cancellation. Provider-reported error
/// *events* are data: they arrive as `Ok(StreamEvent::Error { .. })`.
pub type EventStream<E> = Pin<Box<dyn Stream<Item = Result<E, LlmError>> + Send>>;

/// One abstraction over the three endpoint schemas (us-26).
///
/// Each adapter keeps its own [`Request`](LlmProvider::Request) and
/// [`StreamEvent`](LlmProvider::StreamEvent) IR — the wire shapes differ
/// and are not unified (§5.3's "do not unify prematurely", CODING_STANDARDS
/// §3). Consumers that don't want generics use [`Endpoint`](crate::Endpoint),
/// which wraps all three adapters behind one enum.
pub trait LlmProvider {
    /// The schema-specific request IR.
    type Request;
    /// One typed event of the streamed reply (us-28).
    type StreamEvent;

    /// The endpoint schema this provider speaks.
    fn schema(&self) -> EndpointSchema;

    /// Start a request and return its stream of typed events (us-28).
    ///
    /// Failures before the first event — transport, non-2xx status — come
    /// back as `Err`; mid-stream failures surface as `Err` items. `cancel`
    /// aborts promptly, including mid-backoff (us-30).
    fn stream(
        &self,
        request: Self::Request,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<EventStream<Self::StreamEvent>, LlmError>> + Send;
}
