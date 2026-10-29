//! Internal seams exposed for contract and integration tests — not
//! public API and subject to change without notice.
//!
//! With the `test-fixtures` feature (dev-dependency wiring only — it
//! pulls in `tiny_http`), this also re-exports the in-process HTTP/SSE
//! fixture server shared by every crate's tests.

pub use crate::http::Retry;

#[cfg(feature = "test-fixtures")]
mod fixture;

#[cfg(feature = "test-fixtures")]
pub use fixture::*;
