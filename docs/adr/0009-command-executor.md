# 9. `CommandExecutor` — one trait, user-shell first

Date: 2026-11-03

## Status

Accepted

## Context

REQUIREMENTS.md §5.9 schedules four executor tiers behind one
`CommandExecutor` trait — in-band, ControlMaster, remote daemon, no-op —
and explicitly asks for the trait in Round 6 "so these upgrades need no
re-architecture". Round 6's `run_command` is its first caller, and it
forces the questions §5.9 leaves open:

1. **Where does a command run?** `run_command` could target a private,
   agent-owned PTY or the user's own live shell. The user's shell gives
   the agent her environment, aliases, functions and cwd — and leaves a
   visible trace in her scrollback, which §5.6's consent story requires:
   nothing the agent does may be invisible.
2. **How does the agent worker reach a session it cannot touch?** The
   worker runs on its own thread while `Tab` — PTY, grid, decoded
   events — lives on the app loop's thread. A trait object over `Tab`
   would tangle lifetimes, and CODING_STANDARDS §9's "the TUI holds no
   business logic" means the policy must live in `ait-agent` regardless.
3. **What does the signature promise?** ControlMaster and the remote
   daemon do their own I/O — they have no `Tab`, no shell integration,
   nothing to subscribe to. A signature mentioning sessions, prompt
   marks or OSC frames would not survive them (§8.2 risk: design the
   trait now or re-architect in Round 9).

## Decision

- **`CommandExecutor` is one operation** — `run(&str) ->
  {merged output, exit code | unknown}` — and nothing else. The command
  is model-produced shell text delivered verbatim (sanitization is the
  shell's own parsing, as it is for the user's own typing); the result
  merges stdout+stderr; `exit_code: None` reports an honest "unknown"
  (a payload-free `133;D`), never a guessed code. Every failure is an
  `ExecutorError` the loop renders as a tool result — executor errors
  are model input, never task failures. The call is blocking by design
  (the worker's runtime exists for the LLM stream) and honours a
  `CancellationToken` so Esc interrupts a parked run.

- **Round 6 ships `UserShellExecutor`, tiered internally on the tab's
  live `ShellIntegration` state**, re-read per call because an
  `exit_shell` hook can downgrade mid-task. `Integrated` runs §5.5.1
  in-band `__ait_run` — the request line stays visible while `7701`
  frames resolve output and exit code exactly. `MarksOnly` types the
  command at the prompt and resolves on `133;D` — grid rows `B..D` are
  the output, D's payload the exit code, absent → "unknown". `Absent`
  types `cmd; printf '__AIT_END_<id>=%d\n' $?` and scans the raw output
  tail for the sentinel. Both typed tiers leave everything they type in
  the user's grid — the trace guarantee every agent action inherits
  (Spec R06 us-16).

- **The worker reaches the shell through a `ShellBridge` channel
  proxy**, not a trait object over `Tab`: `integration_state`,
  `run_in_band`, `send_bytes`, `subscribe`, `grid_lines` and
  `output_tail` are requests the `ait` frontend services in its
  per-tick drain, answers travelling back on reply channels while the
  worker waits on its own thread. `Tab` implements the service side —
  the only app-side glue — and `ait-agent` keeps all policy. The proxy
  is what makes the bridge `Send`-able, borrow-free and un-wedgeable:
  a dropped tab is `BridgeError::Gone`, not a deadlock.

- **The §5.9 order stands: user-shell first, remote tiers later.**
  Round 6 proves the local end of the trait; Round 9's ControlMaster,
  remote-daemon and no-op implementations do their own I/O and never
  see a `ShellBridge` — which is exactly why the signature carries
  nothing user-shell-specific. A compile-checked sketch of those impls
  lives in `crates/ait-agent/tests/executor.rs`, and the no-op tier
  already ships as `DetachedExecutor` answering `Unbound` on
  bridge-less tasks.

## Consequences

- Adding a tier is additive: a new `CommandExecutor` impl plus the
  wiring that selects it — `run_command`, the approval gate and the
  agent loop are untouched. Round 9's SSH tiers slot in behind the
  trait instead of forcing a re-design.
- `run_command` in the user's shell means the agent acts with the
  user's own environment — accepted, and mitigated by the interim
  ask-everything gate (every tool call parks on an approve card until
  Round 7's §5.7 profiles land). The typed tiers' race with the user's
  own typing is the known §8.2 risk: a mark or sentinel the user
  produced is indistinguishable from the run's own, so waits are
  bounded (`TYPED_TIMEOUT`, sliding on signs of life) and resolve to
  honest errors, never hangs or invented results.
- The agent-owned PTY behind `write_to_pty` is deliberately *not* a
  `CommandExecutor` tier this round — `run_command` always aims at the
  user's shell. A `LocalPtyExecutor` remains a later-round option
  behind the same trait.
- `ait-agent` gains two core→core edges: `ait-term` (the lazy
  `LiveSession` the agent PTY drives) and `ait-shell` (the injection
  planner it spawns through, plus the `RunResult`/`ShellEvent` wire
  vocabulary). Core→core is permitted — cores never depend on the
  frontend. ADR-0003's crate table notes both.
- `REQUIREMENTS.md` needed no change: §5.9 fixed the tier list and
  asked for the trait early — this ADR records the trait shape and the
  Round-6 tier order it left undecided.
