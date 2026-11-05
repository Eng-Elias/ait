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
//! panic (§5.5 graceful degrade). Nothing here touches a TTY or PTY —
//! the only filesystem access is [`Injection::materialize_in`], which
//! writes snippet assets into a fresh per-session `0700` temp dir and
//! never anywhere else (CODING_STANDARDS.md 11).
//!
//! Round 11 adds [`builtins_for`] — the per-shell builtin/keyword
//! tables (shared POSIX set + bash/zsh/fish extras, keyed by shell
//! name) the §5.10.4 input classifier merges into its program index
//! (Spec R11-T1).

mod builtins;
mod codec;
mod inject;
mod remote;
mod run;
mod snippets;

pub use builtins::{builtins_for, is_known_shell};
pub use codec::{
    decode, session_id, HookName, ShellEvent, OSC_HOOK_CODE, OSC_RAW_BUDGET, OSC_RUN_CODE,
};
pub use inject::{plan, plan_with_zdotdir, Injection, Materialized, ShellKind, ENV_SESSION_ID};
pub use remote::{
    ait_runtime_dir, bootstrap_script, policy_env, ssh_control_dir, SshPolicy,
    DEFAULT_CONTROL_PERSIST,
    ENV_SSH_ALLOW, ENV_SSH_BOOTSTRAP, ENV_SSH_CONTROL_DIR, ENV_SSH_CONTROL_PERSIST, ENV_SSH_DENY,
    ENV_SSH_ENABLED, ENV_SSH_HOP, ENV_SSH_OFF, ENV_SSH_REUSE_CM, SSH_PROGRAM,
};
pub use run::{
    chunk_run_payload, run_begin_sequence, run_end_sequence, run_request, RunAccumulator, RunResult,
};
