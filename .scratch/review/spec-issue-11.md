# ISSUE #11: Spec R02: TUI shell

Covers `REQUIREMENTS.md` §5.2 (TUI shell), building on the §5.1 core merged in Round 1. The §2 ratatui/crossterm row is already Decided. NFRs in §6 apply (render ≥30 fps under load, input never starved by output floods, panics restore the host terminal). Relevant standards: §10 items 6 (empty-state guard), 7 (keybindings before PTY input), 9 (frontend holds no business logic), 11 (tests at agreed seams), 12 (ship something usable). ADR-0003 (crate layout) and ADR-0004 (ait-term stays synchronous) govern placement.

## Problem Statement

Round 1 delivered a complete, tested terminal core — but there is no way to *use* it. `ait` today prints its version and exits: you cannot spawn your shell, see its styled output, type into it, scroll back, or run more than one session. AIT's vision (§1) is a terminal you live in — locally, inside tmux, or over SSH — and none of that exists until the TUI frontend lands.

## Solution

Running bare `ait` opens a full-screen TUI: your `$SHELL` runs in a PTY whose output renders through `ait-term` with full fidelity (colors, bold/italic/all underline styles, wide characters, cursor shapes). Multiple shell sessions live in tabs; a status line shows cwd, shell, model and session state. App keybindings are intercepted before PTY input, the mouse scrolls scrollback and selects text for copying, notifications surface as toasts, and quitting — or panicking — always restores the host terminal. The whole thing works inside tmux and over SSH, degrading gracefully where the host terminal lacks modern input features.

## User Stories

1. As a terminal user, I want `ait` to open a full-screen TUI running my `$SHELL`, so that it behaves like my normal shell.
2. As a terminal user, I want styled output rendered faithfully — ANSI/256/truecolor colors, bold, dim, italic, all underline variants, inverse, strikeout — so that TUIs and colored prompts look right.
3. As a terminal user, I want wide characters and emoji to occupy the correct cells, so that non-ASCII output doesn't corrupt the display.
4. As a terminal user, I want the application's cursor shape (block, beam, underline, hidden) honored, so that vim and similar apps show mode correctly.
5. As a terminal user, I want every keystroke to reach the shell encoded as the right PTY bytes (arrows, function keys, modifiers, application cursor/keypad modes), so that readline and full-screen apps work.
6. As a terminal user, I want app keybindings intercepted *before* anything reaches the PTY, so that a bound key never leaks through (e.g. Ctrl+S must not reach the PTY as XOFF when bound — §5.2, standard 7).
7. As a terminal user, I want keys that are *not* bound — including Ctrl+S and Ctrl+Q by default — to pass through to the shell unchanged, so that the terminal behaves predictably.
8. As a terminal user, I want new tabs to spawn a fresh `$SHELL` in my current tab's working directory (tracked via OSC 7), so that I don't have to `cd` back to where I was.
9. As a terminal user, I want to move between tabs (previous/next, direct jump, close), so that I can run several shells at once.
10. As a terminal user, I want a visible tab bar and status line (cwd, shell, model, session state), so that I always know where I am — the model slot shows `—` until Round 3 configures one.
11. As a terminal user, I want a tab whose shell exited to stay open showing `exited <code>`, so that I can read its final output before closing it.
12. As a terminal user, I want the mouse wheel to scroll through scrollback, so that I can review earlier output.
13. As a terminal user, I want new output *not* to move my view while I'm scrolled up, but any keypress to snap me back to the bottom, so that reading history is never interrupted.
14. As a terminal user, I want to drag-select text and have it copied to my clipboard automatically on release, so that copying is frictionless (wrapped lines join without a stray newline).
15. As a terminal user, I want to paste my last copied selection with Shift+Insert, so that I can reuse text I've selected (using bracketed paste when the application enables it).
16. As a terminal user, I want OSC 9/777 notification requests surfaced as a transient toast, so that a finished command can get my attention (completing §5.1's "surface as a TUI toast").
17. As a terminal user, I want window resizes forwarded to every session, so that apps reflow correctly.
18. As a terminal user, I want `ait` to restore my host terminal on exit *and on panic* — raw mode off, alternate screen left, cursor back, mouse released — so that a crash never leaves my terminal broken (§6).
19. As a terminal user, I want quitting `ait` to terminate the session's shells (SIGHUP, escalating per the core's drop behavior), so that no orphan processes survive.
20. As a terminal user, I want closing the last tab to exit `ait` cleanly, so that the app lifecycle follows my sessions.
21. As a terminal user inside tmux or over SSH, I want everything above to work unchanged, so that AIT's SSH-native promise (§1.1.3) holds.
22. As a terminal user on a host without the kitty keyboard protocol, I want `ait` to still work with a reduced keymap, so that I never lose control of the app.
23. As a terminal user, I want `ait` to stay responsive during output floods (`cat` of a huge file, `yes`), so that my keystrokes are never starved (§6, ≥30 fps).
24. As a terminal user, I want `ait --version` to keep working, so that the scaffold CLI surface doesn't regress.
25. As a reviewer, I want a scripted demo transcript (run under `script`, including inside tmux), so that I can verify the round's behavior without hand-testing.

## Implementation Decisions

- **Placement:** all frontend code lives in the `ait` binary crate (ADR-0003; standard 9 — no business logic, only presentation + event routing). `ait-term`'s public API is already sufficient; it gains at most a small frontend-agnostic accessor (e.g. reporting whether the child enabled bracketed-paste mode) if paste fidelity requires it.
- **Runtime: synchronous, no tokio.** ADR-0004's "tokio joins at the `ait` binary in Round 2" was anticipatory — tokio exists for the agent loop, HTTP and SSE, none of which exist until Round 3+. The event loop is a poll loop: crossterm `event::poll` with a ~8–16 ms timeout → handle all pending input events → `drain_events`/`try_recv_event` on every session → render only when damage or chrome state changed. Input is serviced before session drains each iteration so floods cannot starve the keyboard; the core's bounded output channel already backpressures `yes`-class floods (§6).
- **Rendering:** each frame, damaged sessions rebuild a ratatui buffer from `GridView`: `Color` (Default/Ansi/Indexed/Rgb) → crossterm colors, every `Style` flag and `Underline` variant mapped, `CellKind::WideSpacer` cells skipped (ratatui handles double-width), `CursorShape` → `SetCursorStyle`. Ratatui's double-buffer diff minimizes host writes. Chrome (tab bar + status line) is drawn outside the session grid; the PTY's winsize is the content area only.
- **Keymap — capability-aware.** On startup, request the kitty keyboard enhancement flags (crossterm `PushKeyboardEnhancementFlags`) and detect support. Default bindings live only on byte-distinguishable keys:
  - Scrollback: `Shift+PageUp/PageDown` (page), `Shift+Home/End` (top/bottom); mouse wheel.
  - Tabs: `Ctrl+PageUp/PageDown` prev/next, `Alt+1..9` jump to tab, `Alt+T` new tab, `Alt+W` close tab.
  - Paste own selection: `Shift+Insert`.
  - `Ctrl+Shift+T/W/Q` aliases are registered *only* when the enhancement is confirmed (on legacy input Ctrl+Shift+letter is byte-identical to Ctrl+letter — binding it would eat e.g. Ctrl+C/SIGINT).
  - `Ctrl+S`/`Ctrl+Q` stay unbound → pass through as XOFF/XON (§5.2 requires correct routing *when bound*).
  - Known collisions accepted for now: `Alt+digit` vs readline digit-argument; the keymap becomes configurable in Round 3 (§4.3 `[terminal] keybindings`).
- **Input routing:** translate crossterm `KeyEvent` → `ait_term::KeyEvent`; resolve against the app keymap first (standard 7); unbound events go through `Session::encode_key` → `send_key`.
- **Tabs:** a tab manager owns an ordered list of tabs, each holding one `LiveSession` plus its viewport state (scroll offset, selection, last OSC-7 cwd). New tab spawns `$SHELL` in the current tab's cwd (fallback: launch cwd). An `Exited` session keeps its tab: input is dropped, status shows `exited <code>`; the close binding removes it (no confirmation this round). Closing the last tab exits `ait` — guard the empty state before indexing (standard 6).
- **Status line:** `cwd · shell · model · state`; model renders `—` until Round 3; cwd from `CwdChanged` events, fallback spawn cwd; shell = basename of the spawned program. OSC 9/777 `Notification` events render as a transient toast on the status line (~3 s).
- **Mouse:** mouse capture enabled. Wheel scrolls scrollback 3 lines/tick; scrolled-up view is pinned (new output doesn't move it); any keypress snaps to bottom. Drag-select cells → release auto-copies the selected text to the *host* clipboard via OSC 52 (crossterm `CopyToClipboard`); wrapped rows join without a newline; wide-char spacers excluded via `Row::text`. Enabling mouse capture means the host's own selection needs Shift-bypass — host-dependent, documented in the PR.
- **Clipboard direction split:** copy goes *out* via host OSC 52 (no-op where the host denies it). OSC 52 requests *from child applications* stay off — the `SessionOptions::osc52` default — until Round 3 config exposes the opt-in (§5.1). The `ClipboardWrite` event is wired so flipping it later is one line. There is no host-clipboard *read* (unreliable); `Shift+Insert` pastes ait's own copied selection, wrapped in bracketed-paste markers when the child enabled mode 2004.
- **Lifecycle:** an RAII guard owns raw mode + alternate screen + mouse capture; drop restores everything. A panic hook restores the host terminal before propagating (§5.2 clean exit, §6). Exit paths: last-tab-close, `Alt+Q`/enhanced `Ctrl+Shift+Q` quit, stdin EOF. `LiveSession::drop` already kills children (SIGHUP → SIGKILL).
- **CLI:** bare `ait` launches the TUI; `ait --version` preserved.
- **Dependencies:** `ratatui` + `crossterm` in `ait` only — semver requirements are fine (Cargo.lock is committed) but versions must satisfy the ≥7-day release-age rule (ADR-0002 supply-chain policy).
- **Glossary:** add **Tab** — one session plus its viewport, scroll offset and selection state — to `GLOSSARY.md` in the round's PR.

## Testing Decisions

- **Good test = external behavior at an agreed seam (standard 11):** assert observable outputs — PTY bytes written, buffer contents, state transitions — never implementation internals. No TTY and no network required (§6).
- **Seams (all inside the `ait` crate, above `ait-term`'s stable API):**
  1. *Input translation:* crossterm `KeyEvent` → `ait_term::KeyEvent` mapping — pure function, table-driven cases (modifiers, keypad, function keys).
  2. *Keymap resolution:* given a key event + capability tier (enhancement on/off), does it bind to an app action or pass through? Covers the intercept-before-PTY rule and the legacy-collapse guard (Ctrl+Shift+T must not bind when enhancement is absent).
  3. *Tab manager:* spawn/switch/jump/close transitions, cwd inheritance, exited-session state, and the empty-state guard on last-tab-close (standard 6) — driven with `LiveSession` replaced by a lightweight fake implementing the same usage (or real `/bin/sh` sessions where cheap, per the R01 live-test tier).
  4. *Scroll & selection:* scroll-offset clamping (top/bottom bounds, pin-on-scroll, snap-on-keypress) and selection→text extraction across wrapped rows and wide-char spacers — pure functions over `GridView` fixtures built by feeding bytes to a real `Session` (the R01 byte-fed seam).
  5. *Rendering:* `GridView` → ratatui buffer snapshots on `ratatui::backend::TestBackend` — assert cell text/styles, wide chars, cursor shape — no TTY needed.
- **End-to-end (PR evidence, not CI):** scripted transcript under `script` showing a live session, styled output, tabs, scrollback, selection, a toast, and a panic-free exit — including a run inside tmux.
- **Prior art:** `term-demo` example and the `ait-term` test suites (byte-fed `Session` asserts, live `/bin/sh` spawns) — same style, same three CI gates.

## Out of Scope

Everything outside §5.2 that belongs to later rounds in §7:

- `settings.toml`, keybinding/scrollback/shell config, model display beyond the `—` placeholder (Round 3, §4/§5.3).
- Provider adapters, NL→command, context assembly, redaction (Rounds 3–4).
- Blocks UI, rc snippets, hook protocol, in-band exec, shell-native completions (Round 5, §5.5).
- Agent mode, permission engine, persistence, SSH executor tiers, MCP, skills, classifier, headless mode, packaging (Rounds 6–12).
- Splits/panes within a tab; sessions persisting across restarts; tab reordering/rename; confirm-on-close prompts.
- Child-application OSC 52 clipboard writes stay off (opt-in arrives with config in Round 3); no host-clipboard read.
- Kitty keyboard enhancement is opportunistic only — no workarounds beyond the reduced-legacy keymap.
- No `ait-term` changes beyond the optional bracketed-paste-mode accessor noted above.

## Further Notes

- Naming follows `GLOSSARY.md`: **Session**, **Grid**, plus the new **Tab** term above.
- ADR-0004 consequence holds: the sync→async bridge lives in `ait`, so if polling ever proves insufficient a pump thread can land here without touching `ait-term`.
- A future reader may wonder why Ctrl+Shift bindings are conditional: on legacy terminal input Ctrl+Shift+letter is indistinguishable from Ctrl+letter — worth an ADR if the implementation uncovers additional sharp edges.
- Graceful degradation (§1.1.6): no shell integration is required — cwd display and everything else simply do without OSC 7 on dumb shells.

