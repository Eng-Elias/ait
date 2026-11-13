# GUI manual verification

`ait-gui`'s correctness surface is winit/egui event behavior, which
tests and code review have both missed (R13 shipped a dead gate-card
click layer, a touchpad scroll dead-zone, an Alt-chord text leak, and
resize jank; every one was caught by hands, not by the gate). Run this
checklist on any round that touches `crates/ait-gui`, `ait-app`'s
dispatch/input paths, or the keymap.

Items marked *(scenario)* are executable coverage — TOML under
`crates/ait-gui/tests/scenarios/`, run with
`cargo test -p ait-gui --features test-driver --test scenarios` — not
manual legs; check them anyway when the scenario set can't reach the
behavior (headed rendering, webview, real touchpads).

## Launch

```bash
cargo run -p ait-gui          # needs a display
xvfb-run -a ./target/debug/ait-gui   # headless smoke
```

## Checklist

Work top to bottom; each item is cheap once the window is up.

- **Chord leak** *(scenario: chord-leak, paste-inert)* — press `Alt+A`, `Alt+T`, `Alt+W`, `Alt+P`, `Alt+R`,
  `Ctrl+Shift+F`, `Ctrl+W`, `Alt+Enter` one at a time, then type
  `echo MARKER`: the command line must be clean. Some platforms emit a
  `Text` twin for an Alt-chord — the leak shows as a stray char on the
  line (Alt+A → `a`, R13).
- **Touchpad scroll** — two-finger flick must scroll; wheel events
  arrive as sub-line pixel (`MouseScrollDelta::PixelDelta`) deltas, and
  truncating each event to whole lines drops them all (R13).
- **Scroll-to-bottom** *(scenario: scroll-pill-click, wheel-scroll)* — scroll up; the `⤓` pill appears bottom-right;
  clicking it snaps to live output.
- **Long scrollback** — `seq 1 5000`, then scroll: stays responsive
  (bounded scrollback + viewport-only paint; slowness is usually the
  scroll-delta path, not rendering).
- **Resize** *(scenario: resize_settle test)* — drag a window edge: the grid tracks during the drag and
  the winsize reflow fires on settle, not per frame.
- **Gate cards by mouse** *(scenario: gate-card-click)* — under an `ask` profile, trigger a
  permission card and *click* a verdict button; click a choice row on
  an `ask_user_question` card. Buttons painted but mouse-inert shipped
  once already (egui hit-test ordering — the panel-level `interact`
  must register before the card's).
- **Find chord** *(scenario: find-chord)* — `Ctrl+Shift+F` opens the find bar; `Ctrl+F` reaches
  the shell (readline forward-char — terminals never steal it).
- **Pane chords** *(scenario: pane-chords-overlay, explorer)* — `Ctrl+Shift+E` and `Ctrl+Shift+B` toggle the
  explorer and browser panes: the switcher strip atop the region
  reverses the shown chip, a second press of the shown pane's chord
  releases it, and plain `Esc` while a pane shows returns focus to
  the grid. Bare `Ctrl+E`/`Ctrl+B` and the `Alt` variants must still
  reach the shell (readline) — type `echo MARKER` after each chord:
  no stray chars on the line. While the find bar or a picker is up,
  the chords hold — a `Ctrl+Shift+E` mid-search or mid-pick is
  swallowed, never toggles the region out from under the overlay.
- **Pane click affordance** *(scenario: panes-click)* — the `panes` button anchored at the tab
  bar's right edge opens the host with the mouse (explorer on a
  fresh window, the last selection after): a click while the region
  is closed opens it, a click while that pane shows toggles it off.
  `..` in the explorer stops at the session root — it never climbs
  to `/`.
- **Explorer cwd tracking** — `Ctrl+Shift+E` opens the explorer rooted
  at the shell's cwd: `cd elsewhere` (an OSC-7-reporting shell)
  re-roots the listing, and switching tabs re-roots to that tab's cwd.
- **Explorer browsing** — `↓`/`↑` move the highlight, `Enter`/`→`
  descend into a dir, `←`/`⌫` walk back up (the dir you left stays
  selected), `..` is the same walk by mouse, double-click descends,
  `.` toggles dotfiles, `r` refreshes, the wheel scrolls.
- **Explorer preview** — selecting a text file previews its head in
  the bottom split; a file past the cap flags `…`; a binary file shows
  `binary file — no preview`; `o`/`Enter` on a file spawns the system
  editor detached (`[gui].editor` → `$VISUAL` → `$EDITOR` →
  `open`/`xdg-open`), and a failure toasts.
- **Explorer remote gate** — inside an `ssh()` session the pane shows
  `remote session — browsing unavailable` and never lists the local
  disk under the remote path.
- **Browser webview** — on X11 or macOS, `Ctrl+Shift+B` embeds a
  `wry` child over the pane body: typing `example.com` + Enter loads
  it in place (a bare `not a url` turns amber and fetches nothing),
  `‹`/`›` or `Alt+←`/`Alt+→` walk history, `⟳`/`Ctrl+R` reloads,
  `ext ↗` hands the page to the system browser, and the child tracks
  the body rect on window resize and hides when the pane closes.
  A link click inside the page syncs the address entry and pushes
  history (the wry navigation handler's report), so `‹`/`Alt+←`
  returns to the pre-click page rather than the last typed URL —
  mid-load redirect hops can lag the entry one step on webkit.
- **Browser external surface** — where the embed verdict isn't
  `Embedded` (Wayland, a `web`-less build, a failed init) the body
  names the reason (`embedded webviews aren't supported on
  Wayland`, `this build has no webview runtime`, `the webview
  runtime failed to initialize`) and keeps the whole chrome: Enter
  on a URL still navigates — via the system browser — and the
  surface is never a blank rect.
- **Missing-webkit degradation** — `libwebkit2gtk-4.1.so.0` is a
  hard `DT_NEEDED` (ADR-0018): `ldd` on a `web`-feature binary
  lists it, and a webkit-less system kills that binary in the ELF
  loader, before the fallback can run. The mitigation is
  `cargo build -p ait-gui --no-default-features`: that build needs
  no webkit/gtk packages and the pane reports `this build has no
  webview runtime`.
- **Webview frame discipline** — the webview shares the UI thread
  (§6): while a live webview is staged the sync schedules a bounded
  ~33 ms repaint so the glib pump and page animation keep
  progressing between egui frames — an embedded page visibly plays
  video/animation rather than freezing on the last painted frame.
  With the pane closed (no staged surface, or no host) nothing is
  scheduled and the idle window still paints zero frames; and
  typing/grid output stays responsive while a
  heavy page loads — a stall there wedges input, which is the one
  risk wry leaves us no off-thread answer for.
- **Ctrl+click affordance** *(scenario: detect-click, ctrl-drag-selection)* — hold Ctrl over an `https://`/`file://`
  URL and an existing path in output: detected tokens underline and
  the cursor turns to the pointer; non-tokens, nonexistent paths and
  scrollback rows behave the same way. Ctrl+click on a URL opens the
  browser pane at it (or hands it to the system browser on the
  external surface), on a file previews it in the explorer, on a
  directory navigates the explorer to it. Inside an `ssh()` session
  only URLs afford — paths never resolve under a remote cwd. Ordinary
  click-drag selection is untouched (a Ctrl+press never starts one).
- **Pane/agent precedence** *(scenario: pane-agent-precedence)* — with the agent panel open, a pane chord
  switches the region to that pane (the panel's conversation survives —
  `Alt+A` brings it back); with a pane showing, `Alt+A` opens the panel
  over the live selection. Chord leak: type `echo MARKER` after each —
  no stray chars on the line.
- **Paste trigger** — OS-paste `# list files here` + Enter fires the
  suggestion path; a paste containing newline/tab/escape bytes stays
  inert; `ait ctl send` is never classified (by design — the
  `ctl_coexistence` test's send leg lands verbatim on the grid).
- **Suggestion loader** *(scenario: suggest_triggers test)* — `# x` + Enter shows `⠋ suggesting` in the
  status line while the model call is in flight; it clears on
  complete/error/cancel.
- **ctl verbs** *(scenario: ctl_coexistence test)* — from another terminal: `ait ctl open-tab`,
  `ait ctl send 'echo from-ctl'`, `ait ctl last-block` drive the window.
  (The test's `last-block` leg only proves the round trip — an
  interactive `sh` seals no blocks, so the happy-path readout stays
  a manual leg.)
- **Zoom** — `Ctrl+±`/`Ctrl+0` change terminal text only (⌘ variants
  on macOS); chrome stays put; zoom is ephemeral.

## Scripted smoke (optional)

`xdotool`/`ydotool` can inject keys and pointer motion under `xvfb-run`
for a repeatable version of the checklist; `import`/`scrot`/`xwd`
capture the screenshots PR evidence uses. None is installed by
default — keep it best-effort, not a gate.

The recipe that ran the checklist end to end (R14):

```bash
# Scrub the real session's display vars — an inherited
# WAYLAND_DISPLAY opens the window on your actual desktop.
env -u WAYLAND_DISPLAY -u DISPLAY Xvfb :99 -screen 0 1400x900x24 &
DISPLAY=:99 ./target/debug/ait-gui &

# Inject input over XTest (xdotool, or a tiny XTest helper for
# chords xdotool spells awkwardly).
DISPLAY=:99 xdotool key ctrl+shift+e            # explorer pane
DISPLAY=:99 xdotool type 'cd docs' Return key Return
DISPLAY=:99 xdotool key ctrl+shift+b            # browser pane

# Grab the framebuffer for evidence.
DISPLAY=:99 xwd -root -silent | convert xwd:- shot.png
# or: DISPLAY=:99 import -window root shot.png
```

Notes from that run:

- The embedded webview spawns real `WebKitWebProcess` /
  `WebKitNetworkProcess` children — confirm with `ps` that the page
  embedded rather than silently falling back.
- A missing `libwebkit2gtk-4.1.so.0` kills the binary at exec
  (DT_NEEDED), not at init — build `--no-default-features` for the
  missing-runtime leg.
- First WebKit init under llvmpipe can wedge input for ~20 s; it
  resolves — don't mistake it for a deadlock.
