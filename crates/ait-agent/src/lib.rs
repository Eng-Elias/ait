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
//! Round 6 adds the §5.6 agent mode on the same precedent:
//!
//! - [`AgentSession`] — the synchronous channel handle whose worker
//!   runs the model↔tools loop: `coding_model` resolution (§4.2),
//!   system prompt assembly (cwd/OS/shell, nearest `AGENTS.md`,
//!   redacted output tail), streamed `TextDelta`s, cancellation,
//!   steering and the [`STEP_BUDGET`] model-turn cap.
//! - the §5.6 tool pipeline: a neutral `ToolSpec` registry
//!   translated per schema, streamed tool-call accumulation, and the
//!   read-only tools (`read_files`, `grep`, `glob`) confined to the
//!   bound session's cwd.
//! - the interim permission gate (Spec R06's ask-everything rule):
//!   every call suspends on an
//!   [`AgentEvent::ApprovalRequest`] until
//!   [`AgentSession::approve`] answers an [`ApprovalVerdict`] —
//!   results, denials included, re-enter the conversation redacted.

mod agent_context;
mod context;
mod conversation;
mod endpoint_bridge;
mod files;
mod redact;
mod sanitize;
mod session;
mod suggester;
mod tool_call;
mod tools;

pub use agent_context::AgentContext;
pub use context::{assemble_request, SuggestContext, OUTPUT_LINE_BUDGET};
pub use redact::redact;
pub use sanitize::{sanitize_reply, ReplySanitizer};
pub use session::{
    AgentError, AgentEvent, AgentRequest, AgentSession, ApprovalVerdict, STEP_BUDGET,
};
pub use suggester::{CommandSuggester, SuggestError, SuggestEvent, SuggestRequest};
pub use tool_call::{ToolCall, ToolStatus};
