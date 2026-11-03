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
//!   file tools (`read_files`, `grep`, `glob`, `edit_files` — the
//!   last with an internal unified diff the approve card renders)
//!   confined to the bound session's cwd.
//! - the interim permission gate (Spec R06's ask-everything rule):
//!   every call suspends on an
//!   [`AgentEvent::ApprovalRequest`] until
//!   [`AgentSession::approve`] answers an [`ApprovalVerdict`] —
//!   results, denials included, re-enter the conversation redacted.
//!   `ask_user_question` is exempt by design (us-10): its
//!   [`AgentEvent::AskRequest`] card IS the ask, answered through
//!   [`AgentSession::answer`].
//! - `write_to_pty` and the agent-owned PTY (R06-T7): a lazily
//!   spawned [`ait_term::LiveSession`] injected through `ait-shell`'s
//!   planner, driven write↔settle-read from the worker thread for
//!   REPLs, `ssh`, `sudo` and installers.
//! - the §5.9 [`CommandExecutor`] seam and the
//!   [`UserShellExecutor`]: `run_command` in the user's live shell
//!   through the [`ShellBridge`] the frontend implements per tab —
//!   `Integrated` sessions ride §5.5.1's in-band `__ait_run`
//!   channel; `MarksOnly`/`Absent` type the command at the prompt
//!   and detect completion from `133;D` or the `__AIT_END` sentinel
//!   (R06-T5).

mod agent_context;
mod context;
mod conversation;
mod diff;
mod endpoint_bridge;
mod executor;
mod files;
mod pty;
mod redact;
mod sanitize;
mod session;
mod suggester;
mod tool_call;
mod tools;

pub use agent_context::AgentContext;
pub use context::{assemble_request, SuggestContext, OUTPUT_LINE_BUDGET};
pub use executor::{
    BridgeError, BridgeEvent, CommandExecutor, CommandResult, DetachedExecutor, ExecutorError,
    ShellBridge, ShellIntegration, UserShellExecutor,
};
pub use redact::redact;
pub use sanitize::{sanitize_reply, ReplySanitizer};
pub use session::{
    AgentError, AgentEvent, AgentRequest, AgentSession, ApprovalVerdict, STEP_BUDGET,
};
pub use suggester::{CommandSuggester, SuggestError, SuggestEvent, SuggestRequest};
pub use tool_call::{ToolCall, ToolStatus};
