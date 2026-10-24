# 5. The tiered keymap and the startup-probe watchdog

Date: 2026-10-24

## Status

Accepted

## Context

Spec R02 asks for two things that turned out to have sharp edges:

- `Ctrl+Shift+T/W/Q` aliases for the tab/quit bindings, gated on the
  host's keyboard-capability answer, with detection described as
  "opportunistic" — use the kitty keyboard enhancement when the host
  offers it, carry on silently when not (§5.2: ait must run correctly
  inside tmux and over SSH, hosts that may never answer the probe).

Two uncovered sharp edges shaped the implementation (R02-T2/T6):

1. **The legacy byte-collapse.** Without the enhancement, Ctrl+Shift+letter
   arrives byte-identical to Ctrl+letter — the host cannot report the
   difference. Binding `Ctrl+Shift+Q` unconditionally would eat every
   plain Ctrl+Q (XON) a legacy host sends, plus Ctrl+W/Ctrl+T in
   shells and readline. A binding that cannot be distinguished from
   input the shell expects must not exist.
2. **The probe hang.** crossterm's `supports_keyboard_enhancement`
   ends with an *unbounded* wait for a primary-DA reply once the
   kitty-flags query matched. A host that answers `CSI ? u` but
   swallows `CSI c` wedges the call forever — and the wedged internal
   reader lock would starve every later `event::poll`, leaving an
   input-dead TUI that cannot even quit.

## Decision

- The keymap is tiered on the probe result (`CapabilityTier`):
  `Ctrl+Shift+T/W/Q` bind only under `Enhanced`, where their bytes are
  distinct from Ctrl+letter. Under `Legacy` the same keypresses pass to
  the shell unchanged — including Ctrl+Shift+letter, which there is
  indistinguishable from plain Ctrl+letter anyway, so nothing usable is
  lost.
- The probe runs under a watchdog (`detect_tier` on a worker thread,
  bounded at 3 s — comfortably past crossterm's own ~2 s query window).
  An unanswered probe reports `Hung` and `ait` refuses to start with a
  clean error rather than entering an input-dead loop. This deliberately
  exceeds the spec's "opportunistic only" letter: it lands under §6's
  hardening mandate — a TUI whose input path is dead on arrival is worse
  than no TUI. The hung worker keeps crossterm's reader lock for the
  life of the process; it is abandoned, never re-queried, and the panic
  hook/`TerminalGuard` still restore the host on the way out.

## Consequences

- Legacy hosts (tmux ≤ 3.5a, SSH hops, dumb terminals) lose nothing they
  could have used: Ctrl+Shift+letter was never distinguishable there.
- Enhanced hosts get the Ctrl+Shift aliases exactly as spec'd.
- A wedged probe fails loudly at startup — host restored, error printed —
  instead of hanging silently with dead input.
- The watchdog is a one-shot startup cost, not a per-event one.
- A related limit stays documented rather than solved: crossterm's unix
  event source spins internally on a read error or EOF from the tty fd
  itself, so `event::poll` can hang in a way no public API can interrupt.
  What *is* reachable — persistent `poll`/`read` errors, including the
  no-input-source case — exits cleanly via the bounded retry in `app`
  (the spec's stdin-EOF exit path).
