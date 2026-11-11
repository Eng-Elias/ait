# 16. ait-app: the shared session model, and the two-binary GUI shape

Date: 2026-11-12

## Status

Accepted

## Context

Round 13 (Spec R13, REQUIREMENTS.md §5.12) ships a second frontend —
the `ait-gui` desktop binary — that must behave identically to the TUI:
same tabs, blocks, agent conversation, permission gates, pickers,
status line, ctl surface. Everything a frontend renders today lives in
the `ait` binary's `app` module: `Tab`/`TabManager`, the keymap, the
agent panel's state, the ctl server, the headless driver — ~9k lines
of logic that has nothing to do with crossterm or ratatui beyond two
thin seams (the key-event shim and the render helpers).

The alternatives were all worse:

- **Copy the app layer into `ait-gui`.** Every session bug gets fixed
  twice and the frontends drift on the first missed backport — the
  exact failure the spec's "same session model" wording rules out.
- **Grow `ait` into a multi-frontend crate.** `cargo build -p ait` is
  the §5.11 release unit and the musl static leg; GUI dependencies in
  its tree — even optional — put the toolkit one feature-resolution
  accident away from the lean TUI install (spec us-23).
- **Keep the app layer in `ait` and let `ait-gui` depend on the TUI
  crate.** Same tree problem, plus the GUI would link crossterm and
  ratatui it never calls.

ADR-0003 reserves this call: layout growth gets its own ADR.

## Decision

- **`ait-app` is the frontend-agnostic session model.** A new lib
  crate holding everything both frontends render: `Tab`/`TabManager`/
  `SessionTemplate`, `BlockAssembler`, `RemoteContext`/`SshSpawn`,
  `TabBridge`/`BridgeRequest`, the ctl server/codec/dispatch, the
  headless driver, `InputShadow` and the suggestion triggers,
  `Selection`/`ScrollState`, scrollback search, `StatusLine`/`Toast`,
  `StoreHandle`, `Transcript`, `LiveSettings`, `LiveSkills`, the
  profile and conversation picker state, `InBandExec`,
  `IntegrationState`, `AgentPanel`'s state half, and the keymap —
  `BINDINGS` + `resolve` over `ait_term::KeyEvent` gated on a
  per-frontend `CapabilityTier` (the GUI is always `Enhanced`). Two
  decoupling edits make `Tab` frontend-neutral: `agent_key` is
  parameterized on `ait_term::KeyEvent`, and `resize_active`/
  `resize_all` take a frontend-computed `Size` instead of calling a
  render helper — each frontend injects its own panel layout math.
- **Frontends stay thin in both directions.** `ait` keeps the TUI run
  loop, the crossterm→`KeyEvent` shim and the ratatui `paint_*` code;
  `ait-gui` keeps the egui window, its own key shim and the grid/
  chrome painters (ADR-0017). No GUI type crosses into `ait-app` or
  `ait-term`; no business logic lives in either frontend
  (CODING_STANDARDS.md §9 applies symmetrically now).
- **Two binaries, one in-process seam.** `ait-gui` is a thin `main`
  over the `ait-gui` lib — its CLI surface is run + `--version` +
  `--help`; `--init`, `--print-schema`, `--headless` and the ctl
  client stay `ait`-only. For local hacking `ait` gains
  `gui = ["dep:ait-gui"]`, default off: `ait gui` / `ait --gui`
  launches `ait_gui::run` in-process, and without the feature the
  subcommand explains itself and exits. Release builds never enable
  it — dist ships `ait` and `ait-gui` as separate packages, so
  `cargo build -p ait` carries zero GUI dependencies (us-23) and the
  musl leg stays GUI-free (ADR-0015 amendment).

## Consequences

- A session bug is fixed once, in `ait-app`; `headless.rs` — which
  drove a real `Tab` with no renderer long before the GUI existed —
  moved there unchanged as the standing proof that a frontend needs
  only `drain` + `observe` + `service_bridge`.
- §3.1's crate table, the AGENTS.md crate map and §5.12's
  Proposed→Decided flip land in the same PR (the ADR-wins rule:
  REQUIREMENTS.md is updated alongside, not left contradicting).
- The `gui` feature is a dev convenience, not a distribution channel —
  reviewers should treat any diff enabling it in dist config or
  default features as a bug.
- ctl needs no new verbs: the GUI registers the same
  `${XDG_RUNTIME_DIR}/ait/<pid>.sock` convention and answers existing
  verbs through `TabManager` dispatch, so `ait ctl` addresses a GUI
  instance identically.
