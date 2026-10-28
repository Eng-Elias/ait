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

## Applying the release-age rule (added in the R03 retro)

The rule generalised to every crate in the tree: **every resolved
dependency release must be ≥7 days old at adoption time**, and every
`crates/*/Cargo.toml` entry documents its resolved version + release
date (`name X.Y.Z (YYYY-MM-DD)`) in its comment block — one shared block
may cover several entries. `scripts/check-dep-comments.py` enforces the
comment; CI's `check` job runs it.

Procedure when adding or upgrading a dependency:

1. Add a semver requirement (or an `=` pin where the ADR calls for one).
2. Build; check `Cargo.lock`'s resolved version and that release's date
   on crates.io (`https://crates.io/api/v1/crates/<name>/<version>`,
   `created_at`).
3. If the resolved release is younger than 7 days, roll back to the
   newest release that qualifies:
   `cargo update -p <crate> --precise <version>` — the requirement stays
   semver, the lockfile carries the older release. This happens in
   practice (tokio, notify and uuid all needed it in R03); it is the
   normal path, not an exception.
4. Write the resolved version + release date into the `Cargo.toml`
   comment block.
