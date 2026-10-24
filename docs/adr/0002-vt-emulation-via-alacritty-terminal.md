# 2. VT emulation via alacritty_terminal

Date: 2026-10-24

## Status

Accepted

## Context

`REQUIREMENTS.md` §2 marked the VT emulation layer **Proposed**: use
`alacritty_terminal` (grid + `vte` parser), with `vte` + a hand-written grid
as the alternative. §2 also says "do not hand-write a full emulator". Round 1
(§5.1) needs a styled cell grid with scrollback, colors, text attributes,
wide characters and reflow-on-resize — exactly the surface a mature emulator
already provides.

## Decision

We will use `alacritty_terminal` for VT emulation, pinned to an exact version
(`=0.26.0`) that was released at least 7 days before adoption; upgrades are
deliberate.

- `ait-term` owns the dependency. `alacritty_terminal` types never appear on
  `ait-term`'s public API — sessions expose our own read-side types
  (rows/cells/styles/cursor/damage) so the TUI and tests are insulated if the
  emulator is ever swapped.
- `alacritty_terminal`'s `tty` and `event_loop` modules are unused: the PTY
  stays `portable-pty` (Decided, §2) and the live driver is our own thin
  wrapper around the pure `Session` core.
- We reuse `alacritty_terminal`'s `vte` re-export (`alacritty_terminal::vte`)
  instead of depending on `vte` directly, so parser versions can never drift
  apart.
- A hand-written emulator remains rejected: §5.5's AIT-private OSC sequences
  are observed through a parallel parser (or `Term`'s `Handler` impl), not by
  forking emulation.

## Consequences

- Correctness of rendering, scrollback, reflow, wide chars and alt-screen is
  inherited from a battle-tested emulator instead of written and maintained
  in-house.
- The read-side types in `ait-term` are the only API contract; a swap of the
  emulator costs one crate, not the codebase.
- Dependency hygiene: exact pin, release-age rule, and we take no
  `alacritty_terminal` features we do not need.
- `portable-pty` remains the PTY layer; no conflict since the emulator's own
  I/O modules are unused.
