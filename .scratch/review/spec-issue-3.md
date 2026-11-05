# ISSUE #3: Spec R01: Terminal core

Covers `REQUIREMENTS.md` §5.1 (terminal core), plus the two Round-1 decisions: §2 (VT emulation, marked Proposed) and §3.1 (crate layout, marked Proposed). NFRs in §6 apply (testability, performance, graceful degradation).

## Problem Statement

AIT is a TUI terminal that must spawn the user's real shell, render its output faithfully (colors, styles, cursor movement, alt-screen apps like vim and htop), and observe out-of-band control sequences (working directory, prompt boundaries, clipboard, notifications) without breaking rendering. Today the repo is a scaffold with no terminal code: there is no way to run a shell session, no way to test terminal behavior, and no crate to put it in.

## Solution

A new `ait-term` crate provides a frontend-agnostic terminal core. It spawns `$SHELL` (fallback `/bin/sh`) in a PTY, feeds output through a real VT emulator into a styled cell grid with scrollback, tracks the session lifecycle (starting → running → exited with status), forwards resizes, encodes a frontend-neutral `Key` type into PTY bytes, and emits events for the OSC sequences the product cares about — all through a synchronous public API that tests can drive by feeding bytes and asserting on the grid, with no real terminal required.

## User Stories

1. As a terminal user, I want AIT to spawn my `$SHELL` in a PTY, so that my session behaves like my normal shell (aliases, rc files, interactive behavior).
2. As a terminal user, I want `/bin/sh` as a fallback when `$SHELL` is unset, so that AIT still works in minimal environments.
3. As a terminal user, I want full-screen apps (vim, htop, less) to render correctly, so that AIT is a real terminal and not a line-based shell.
4. As a terminal user, I want styled output (colors, bold, italic, underline) rendered into the grid, so that TUI apps and colored prompts look right.
5. As a terminal user, I want wide characters and emoji to occupy correct cells, so that non-ASCII output doesn't corrupt the grid.
6. As a terminal user, I want scrollback above the viewport, so that I can review output that has scrolled off (configurable length, 10,000-line default).
7. As a terminal user, I want resizing my window forwarded to the shell, so that apps like vim reflow correctly.
8. As a terminal user, I want the session to report when the shell exits (with its exit status), so that the UI can reflect a dead session instead of a frozen screen.
9. As a terminal user, I want keystrokes translated into the right PTY bytes (arrows, function keys, modifiers, application cursor-key mode), so that readline and TUI apps work.
10. As a frontend developer, I want a single `Key` type so the TUI (and any future frontend) never deals with raw escape sequences.
11. As a frontend developer, I want a `Session` lifecycle API (starting, running, exited + status), so that the status line can show session state.
12. As a frontend developer, I want a channel of session events (output ready, cwd changed, clipboard request, notification, exited), so that the TUI can react without polling internals.
13. As a shell user, I want `OSC 7` to update the session's reported cwd, so that later features (context assembly, status line) know where I am.
14. As a shell user, I want `OSC 133` A/B/C/D prompt marks recorded in the grid, so that Round 5 can assemble blocks without re-parsing output.
15. As a shell user, I want `OSC 52` clipboard writes honored only when I opt in, so that remote or malicious output cannot silently overwrite my clipboard.
16. As a shell user, I want `OSC 9`/`OSC 777` delivered as notification events, so that the TUI can toast when a command finishes.
17. As a shell user, I want unrecognized or AIT-private OSC sequences (including the future 7700-series hook channel) passed through the event layer, so that Round 5's hook protocol needs no parser changes.
18. As a contributor, I want to drive the terminal by feeding raw bytes in a unit test, so that core logic is testable without a TTY (§6).
19. As a contributor, I want `cat` of a 100 MB file and `yes` floods to not block input processing, so that the terminal stays responsive under load (§6).
20. As a reviewer, I want a runnable example that spawns a shell and dumps the grid as text, so that I can smoke-test the round without a TUI.

## Implementation Decisions

- **VT emulation (settles §2's Proposed row — record as ADR-0002):** use `alacritty_terminal` 0.26.x (`Term` + `Grid` + styled `Cell`s + scrollback). Do not hand-write an emulator (§2). `alacritty_terminal`'s own `tty`/`event_loop` modules are unused — the PTY stays `portable-pty` (Decided, §2).
- **OSC interception seam:** `alacritty_terminal` owns rendering state; a second `vte::Parser` (the version re-exported by `alacritty_terminal`, so no extra dep surface) runs in parallel over the same byte stream with a minimal `Perform` that only implements `osc_dispatch`, emitting our own `OscEvent`s: `CwdChanged`, `PromptMark` (OSC 133 A/B/C/D + payload), `ClipboardWrite`, `Notification`, and `Unknown`/`Private` for everything else. Observation only — nothing is stripped from the stream, so alacritty_terminal's own handling of OSC 7/52/133 is unaffected.
- **Crate layout (settles §3.1 — record as ADR-0003):** adopt the six-crate layout unchanged (`ait-term`, `ait-config`, `ait-llm`, `ait-shell`, `ait-agent`, `ait` binary). Round 1 creates only `ait-term`; the other crates are added in their own rounds. Fill the AGENTS.md crate map listing all six with exists/planned status.
- **`ait-term` internal shape:** a pure `Session` core (parser + `Term` + event queue, `feed_bytes(&[u8])` → events) plus a thin live driver: `portable-pty` spawn, `std::thread` reader, `std::sync::mpsc` channel, `drain_events()`/`try_recv()`. No async runtime inside `ait-term`; `tokio` joins at the `ait` binary in Round 2 (§2's tokio decision is about the app runtime, not this crate).
- **Grid read API:** `ait-term` exposes its own minimal read-side types (rows/cells/styles/cursor/damage) and keeps `alacritty_terminal` types private, so the TUI and tests are insulated if the emulator is ever swapped.
- **Key model:** one `Key` enum + modifier bitflags covering crossterm's input space; `ait-term` encodes `(Key, modifiers)` → PTY bytes honoring terminal modes (application cursor keys, keypad modes). Frontends never emit escape sequences.
- **Session lifecycle:** `Starting → Running → Exited(exit_status)`; exit observed via the PTY child handle and surfaced as an event.
- **OSC 52 opt-in:** off by default; a session option gates it (also enforceable via `alacritty_terminal`'s `Osc52` config). A clipboard *request* event is emitted — the frontend performs the actual clipboard write.
- **OSC 133:** marks recorded on grid lines/events this round; block assembly (Block = command + output + exit code) is deferred to Round 5 (§5.5).
- **Config surface:** constructor options only (shell path, scrollback length, osc52, initial size). `settings.toml` wiring arrives in Round 3.
- **Deps in `ait-term`:** `alacritty_terminal`, `portable-pty`, `vte` (matching re-export), `thiserror`, `tracing` (file logging, never TUI — §2).
- **Same-PR doc updates (per §1 reading rules and ADR-0001):** the PR landing these decisions adds `docs/adr/0002-*.md` + `0003-*.md`, updates the §2 VT row and §3.1 in `REQUIREMENTS.md` to Decided, and fills the AGENTS.md crate map.

## Testing Decisions

- **Single seam: the public `Session` API.** Feed bytes in (`feed_bytes`), assert on the grid and the emitted events. This is the highest seam available — no mocks of `alacritty_terminal`, no TTY, no PTY (§6 testability).
- **Good test = external behavior only:** assert cell contents/styles at grid positions, scrollback growth, cursor state, event sequence (`CwdChanged`, `PromptMark`, `Notification`, `Exited`), resize behavior, and `Key` → byte encoding under different terminal modes. Never assert internal parser state.
- **Prior art:** none in-repo (first real crate). Tests follow existing CI gates: `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace`. Fixtures are byte-string inputs (e.g. recorded `ls --color`/`vim` output); no network, no TTY.
- **Live-PTY tests** (spawn a real shell, e.g. `sh -c 'printf hi; exit 3'`) are a small second tier kept to spawn/resize/exit-path coverage only; everything else is byte-fed.

## Out of Scope

Everything outside §5.1 that belongs to later rounds in §7:

- Ratatui/crossterm rendering, keybinding routing, tabs, mouse, host-terminal restore (Round 2, §5.2).
- `settings.toml`, model roles, provider adapters (Round 3, §4/§5.3).
- NL→command, context assembly, redaction (Round 4, §5.4).
- Block assembly, rc snippets, hook protocol decode, in-band exec (Round 5, §5.5) — Round 1 only records OSC marks/events.
- Agent mode, permission engine, persistence, SSH tiers, MCP, classifier, packaging (Rounds 6–12).
- Multi-session/tab management beyond a single-session API.
- The `ait` binary gains no user-facing terminal this round.

## Further Notes

- Naming follows `GLOSSARY.md`: **Session** (one PTY + shell + grid), **Grid** (styled cell matrix + scrollback). The parallel-OSC observer doesn't strip sequences, so AIT-private 7700-series sequences (§5.5) arrive intact in Round 5 with zero parser work.
- `alacritty_terminal` pins an exact version (supply-chain rule: released ≥7 days); upgrades are deliberate.
- Graceful degradation (§1.1.6, §8.1): no shell integration exists yet and nothing here requires it — sessions work with zero OSC support from the shell.
- If the parallel `vte` observer proves to fight `alacritty_terminal`'s internal state, the fallback is wrapping `Term`'s `Handler` impl — decide at implementation, not a spec blocker.
