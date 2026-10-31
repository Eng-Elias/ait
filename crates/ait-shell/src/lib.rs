//! `ait-shell` — AIT's shell-integration crate (REQUIREMENTS.md §5.5).
//!
//! Owns everything that makes the terminal *know what ran*: the bash/zsh/
//! fish rc snippets, the injection planner, and the pure codec for the
//! AIT-private OSC 7700-series channel. `ait-term` stays transport-agnostic —
//! it surfaces every unrecognized OSC as `SessionEvent::Private{code,params}`;
//! [`decode`] here is the single place those raw bytes gain protocol meaning:
//!
//! - `OSC 7700 ; <hex({"hook":…,"value":…})>` — hook events
//!   (`init_shell`/`bootstrapped`/`precmd`/`preexec`/`exit_shell`), decoded to
//!   [`ShellEvent::Hook`].
//! - `OSC 7701` — the in-band `__ait_run` channel (§5.5.1): `A` begins a
//!   response, repeatable `R;<len>;<hex>` frames carry it, `B` ends it.
//!   [`RunAccumulator`] reassembles the frames into a [`RunResult`].
//! - `OSC 7702`/`7703` and every other `770x` — protocol-reserved, surfacing
//!   as [`ShellEvent::Reserved`]; nothing consumes them this round.
//!
//! Malformed, truncated or over-long sequences decode to `None`, never
//! panic (§5.5 graceful degrade). Nothing here touches a TTY, PTY or the
//! filesystem — every function is a pure seam (CODING_STANDARDS.md 11).

mod codec;
mod run;

pub use codec::{decode, HookName, ShellEvent, OSC_HOOK_CODE, OSC_RAW_BUDGET, OSC_RUN_CODE};
pub use run::{
    chunk_run_payload, run_begin_sequence, run_end_sequence, run_request, RunAccumulator, RunResult,
};
