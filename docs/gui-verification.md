# GUI manual verification

`ait-gui` has no input-injection harness — its correctness surface is
winit/egui event behavior, which tests and code review have both missed
(R13 shipped a dead gate-card click layer, a touchpad scroll dead-zone,
an Alt-chord text leak, and resize jank; every one was caught by hands,
not by the gate). Run this checklist on any round that touches
`crates/ait-gui`, `ait-app`'s dispatch/input paths, or the keymap.

## Launch

```bash
cargo run -p ait-gui          # needs a display
xvfb-run -a ./target/debug/ait-gui   # headless smoke
```

## Checklist

Work top to bottom; each item is cheap once the window is up.

- **Chord leak** — press `Alt+A`, `Alt+T`, `Alt+W`, `Alt+P`, `Alt+R`,
  `Ctrl+Shift+F`, `Ctrl+W`, `Alt+Enter` one at a time, then type
  `echo MARKER`: the command line must be clean. Some platforms emit a
  `Text` twin for an Alt-chord — the leak shows as a stray char on the
  line (Alt+A → `a`, R13).
- **Touchpad scroll** — two-finger flick must scroll; wheel events
  arrive as sub-line pixel (`MouseScrollDelta::PixelDelta`) deltas, and
  truncating each event to whole lines drops them all (R13).
- **Scroll-to-bottom** — scroll up; the `⤓` pill appears bottom-right;
  clicking it snaps to live output.
- **Long scrollback** — `seq 1 5000`, then scroll: stays responsive
  (bounded scrollback + viewport-only paint; slowness is usually the
  scroll-delta path, not rendering).
- **Resize** — drag a window edge: the grid tracks during the drag and
  the winsize reflow fires on settle, not per frame.
- **Gate cards by mouse** — under an `ask` profile, trigger a
  permission card and *click* a verdict button; click a choice row on
  an `ask_user_question` card. Buttons painted but mouse-inert shipped
  once already (egui hit-test ordering — the panel-level `interact`
  must register before the card's).
- **Find chord** — `Ctrl+Shift+F` opens the find bar; `Ctrl+F` reaches
  the shell (readline forward-char — terminals never steal it).
- **Paste trigger** — OS-paste `# list files here` + Enter fires the
  suggestion path; a paste containing newline/tab/escape bytes stays
  inert; `ait ctl send` is never classified (by design).
- **Suggestion loader** — `# x` + Enter shows `⠋ suggesting` in the
  status line while the model call is in flight; it clears on
  complete/error/cancel.
- **ctl verbs** — from another terminal: `ait ctl open-tab`,
  `ait ctl send 'echo from-ctl'`, `ait ctl last-block` drive the window.
- **Zoom** — `Ctrl+±`/`Ctrl+0` change terminal text only (⌘ variants
  on macOS); chrome stays put; zoom is ephemeral.

## Scripted smoke (optional)

`xdotool`/`ydotool` can inject keys and pointer motion under `xvfb-run`
for a repeatable version of the checklist; `import`/`scrot` capture the
screenshots PR evidence uses. Neither is installed by default — keep it
best-effort, not a gate.
