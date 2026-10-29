//! `ait-agent` — AIT's agent core (REQUIREMENTS.md §3.1, ADR-0003).
//!
//! Round 4 lands the crate's pure seams (CODING_STANDARDS.md §11 —
//! each is testable without a PTY, model or TTY):
//!
//! - [`redact`] — the §5.10.3 credential-redaction filter guarding
//!   terminal output before it reaches a provider (Spec R04, user
//!   stories 18, 26).
//! - [`assemble_request`] + [`SuggestContext`] — §5.4 context
//!   assembly into a schema-matched `ait_llm::EndpointRequest` the
//!   suggester streams unchanged (user stories 15–17, 19, 27).
//! - [`sanitize_reply`] + [`ReplySanitizer`] — §5.4 reply sanitization,
//!   the never-execute guarantee, batch and streaming forms (user
//!   stories 5, 6, 28).
//! - [`CommandSuggester`] — the synchronous channel API that owns the
//!   endpoint call on a worker-thread tokio runtime (`ait` stays
//!   tokio-free, ADR-0004), streaming already-sanitized command text
//!   (user stories 20–23, 25).
//!
//! The tool registry, agent loop and permission engine build on these
//! in later tickets.

mod context;
mod redact;
mod sanitize;
mod suggester;

pub use context::{assemble_request, SuggestContext, OUTPUT_LINE_BUDGET};
pub use redact::redact;
pub use sanitize::{sanitize_reply, ReplySanitizer};
pub use suggester::{CommandSuggester, SuggestError, SuggestEvent, SuggestRequest};
