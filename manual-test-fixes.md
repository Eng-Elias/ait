# Fix tracker — manual tour findings

Tracks fixes for issues noted in `notes-after-maual-testing.md`
(manual tour: `manual-tour.md`). Branch: `fix-manual-test-issues`
→ PR to `dev`.

Legend: ✅ fixed · 🚧 in progress · ⏳ pending · ❓ answered (not a
code bug) · ⏭️ deferred (needs manual/hardware retest)

## Input & clipboard

| # | Issue | Status | Fix |
|---|-------|--------|-----|
| 1 | Ctrl+C copies selection & clears it; Ctrl+C with no selection sends SIGINT; Ctrl+Shift+C always copies; Ctrl+V pastes | ✅ | Both frontends intercept `Ctrl+C`/`Ctrl+Shift+C` before pass-through: a live selection copies (GUI `copy_selection` → `ctx.copy_text` + toast; TUI → OSC-52 + toast) and clears; nothing selected → Ctrl+C encodes as SIGINT like before; Ctrl+Shift+C is always the copy chord. |
| 2 | No scrollbar for fast scrollback navigation | ✅ | GUI grid now paints a right-edge scrollbar whenever scrollback exists — thumb size = viewport share of the buffer, position = distance from bottom; click/drag scrolls directly (`Tab::scroll_to_offset`). TUI: n/a (real terminal; wheel + PgUp/PgDn/Home/End already scroll). |
| 3 | Copy-on-select should show a "copied" notification | ✅ | Drag-release copy in the GUI and `copy_range` in the TUI both raise the status strip's `copied` toast; Ctrl+C/⌘C copies toast too. |
| 4 | Selection doesn't clear on plain click | ✅ | GUI: a click (press+release, no drag) clears the highlight; TUI: `begin` already cleared — plus `Selection::finish` now drops a collapsed (one-cell) range instead of leaving a phantom 1-cell selection that would fool Ctrl+C into copying instead of interrupting. |
| 5 | `#` suggestion trigger collides with bash comments when pasting multi-line scripts | ✅ | New `input.suggest_trigger` setting (default `"# "`, kept for spec compatibility §5.4): the same prefix now drives request detection AND the classifier rewrite, so every stage honors it. Validation rejects empty/control-char triggers. Suggested alternatives in the emitted config comment: `"## "` (keeps the no-op-on-idle property, rarely a real comment), `"? "`, `"! "`. Recommendation: set `suggest_trigger = "## "` — an unanswered `## ` line is still a shell comment, so a misfire stays harmless exactly like `# `, but real single-`#` comments never arm it. |
| 6 | Escaped chars / cursor movement feel slow | ❓ | Investigated — no artificial delay found: GUI input dispatches the same frame (egui repaints on input events), the pump drains at 5 ms, TUI input poll is 8 ms. The perceived lag was most likely the *input routing* bugs fixed in #7/#11/#12 (keystrokes silently swallowed by a focused-but-invisible pane read as "laggy" typing). Retest after the focus fixes; if it persists, report which frontend and which key sequence. |

## Agent panel & panes (GUI)

| # | Issue | Status | Fix |
|---|-------|--------|-----|
| 7 | Can't paste/type into agent panel — input always goes to terminal; needs visible focus cursor | ✅ | `Event::Paste` now routes to the focused agent panel (`Tab::agent_paste` — newlines flatten to spaces on the one-line input) instead of the PTY; composed `Event::Text` reaches the panel through the existing `agent_key` path. Focus: panel title reverses + blinking beam caret while focused (repaints scheduled), clicking the panel body focuses it, clicking the grid unfocuses without cancelling a running task (`Tab::unfocus_agent` / `focus_agent`, `AgentPanel::focus`). |
| 8 | `__ait_run 0 <hex>` leaks into the terminal while the agent runs | ❓ by design | Spec §5.5.1 (ADR-0008): the agent's command execution is intentionally visible in-band for transparency — the user sees exactly what the agent runs. Not a leak; no change. |
| 9 | Panes-button + Alt+A toggle semantics confusing (agent↔explorer swap instead of show/hide) | ❓ | Current semantics kept (Spec R14): the side region is *claimed*, not owned — the agent panel holds the claim while open; selecting an aux pane yields the region to it; closing the agent returns the region to the last-selected pane rather than to nothing. The chip strip is the chords' mouse form (agent chip = Alt+A cycle open→focus→close; pane chip = its toggle). New focus border + click-to-focus should make the claim visible — retest. |
| 10 | Pane chords inconsistent: Ctrl+Shift+E / Ctrl+Shift+B vs Alt+letter scheme | ❓ | Deliberate: `Alt+B` is already Select-Block (Spec R05 §5.5) and `Alt+letter` chords are readline/emacs territory (Alt+B = backward-word, Alt+E is free but pairing Explorer/Browser on Alt would split semantics and collide with the block navigation chord). `Ctrl+Shift+letter` is the GUI-local pane chord space, consistent with Ctrl+Shift+F (find). No change — document the convention. |
| 11 | Browser pane open → can't type in terminal; no focus cursor; paste/Ctrl+A act in browser | ✅ | Focus model added: a pane owns unbound keys/text/paste only while *focused*; clicking the grid hands input back to the shell while the pane stays open (spectator mode), clicking the pane body (or its chord) refocuses. The region's border brightens while focused; the browser entry caret now blinks only while focused. Paste still lands on the entry while focused (`accepts_paste`). Ctrl+A select-all in the entry: not implemented — the entry is append-only single-line; noted as a possible follow-up. |
| 12 | Explorer open → can't type in terminal | ✅ | Same focus model — an open-but-unfocused explorer spectates; typing lands in the terminal. |

## Explorer / browser detail

| # | Issue | Status | Fix |
|---|-------|--------|-----|
| 13 | No way to see full cwd when the window is small (status line truncates) | ✅ | Status strip now shows the full status line (incl. full cwd) in a hover tooltip when it doesn't fit. |
| 14 | Can't scroll the file preview in explorer | ✅ | The preview keeps its own `preview_scroll` offset — a wheel flick over the preview area pages the text (clamped), never the listing; it resets when the selection changes. |
| 15 | Embedded webview unsupported on Wayland → links open in Chrome instead | ✅ | With the `web` feature built in, a Wayland session now forces winit's X11 backend (`with_x11()` via eframe's `event_loop_builder`) so the window runs on XWayland and `wry::build_as_child` gets the Xcb handle it needs — the embedded webview works. Sessions without `DISPLAY` (no XWayland) and feature-off builds keep the external-open fallback. Needs a manual retest on Wayland. |
| 16 | Ctrl+click on a wrapped path only detects one line | ✅ | Detection now scans the *logical* line: `detect::logical_cells` joins the soft-wrap chain (`Row::wrapped`) and `detect_in_logical` maps each char to its (line, column) cell — a path wrapped mid-token resolves whole. Hover underline covers the hovered row's share of the token; the test-driver `detect.*` hit rects are wrap-aware too. |

## Questions / untestable (answered, not code changes)

| # | Question | Status | Answer |
|---|----------|--------|--------|
| 17 | How to create multiple profiles for Alt+P? | ❓ | Profiles live in `settings.toml`: add `[[profiles]]` blocks (name, model role, etc. — see `ait --init` template / §5.7). `Alt+P` cycles the picker; each profile binds model+permissions presets per session. |
| 18 | Alt+Enter couldn't be tested | ❓ | `Alt+Enter` (Spec R04 us-11/12) sends the *current input line* as the suggestion request — type a natural-language line without `# `, press Alt+Enter, the line is erased and the suggestion streams in. Try `Alt+Enter` with "list all files larger than 1gb" typed at the prompt. |
| 19 | ssh() session browsing-unavailable couldn't be tested | ❓ | Needs a live SSH session: inside an `ssh()`-integrated tab the explorer correctly refuses to list local paths (`remote session — browsing unavailable`) because the cwd is remote. Run `scripts/ssh-e2e.sh run` for the docker SSH e2e, or `ssh localhost` inside a tab to see the marker. |
| 20 | `detect.path./nonexistent` test point unclear | ❓ | That test asserts Ctrl+hover shows NO affordance on a path that doesn't exist (detection only offers files that resolve) and that inside an `ssh()` session only URLs afford (remote paths can't be resolved against the local fs). To test: type `./nonexistent` at the prompt, Ctrl+hover → no underline; `ssh localhost`, Ctrl+hover a filename → no path affordance, a URL still underlines. |

## Automation added for these fixes

- `crates/ait/src/app.rs` — `ctrl_c_copies_a_selection_and_sigints_without_one`:
  selection → Ctrl+C copies+clears via OSC 52; no selection → real SIGINT
  (a `sleep` dies and the next command echoes).
- `crates/ait-gui/src/app.rs` — `ctrl_c_copies_a_live_selection_and_sigints_without_one`
  (raw-PTY capture sees the literal `\x03`) and `an_unfocused_pane_spectates`
  (unfocused pane lets `Event::Text` reach the shell while staying open).
- `crates/ait-gui/tests/scenarios/ctrl-c-copy.toml` — the same flow end to
  end through the test-driver socket: drag-select → Ctrl+C → `copied`
  toast + cleared selection → SIGINT on a `sleep` → explorer open → grid
  click → typing reaches the shell while the pane stays up.
- `crates/ait-gui/src/pane.rs` — `focus_tracks_the_click_flow`: select
  arrives focused, unfocus spectates, refocus re-arms, clear resets.
- `crates/ait-gui/src/explorer.rs` — preview-scroll clamp/rewind in
  `scroll_tracks_the_selection_into_the_window`.
- `crates/ait-gui/src/detect.rs` — `a_wrapped_path_resolves_across_the_soft_wrap`:
  a path split mid-token across a soft wrap resolves whole; the underline
  is row-local on both halves; remote tabs still gate to URLs.
- `crates/ait-app/src/tabs.rs` — `scroll_to_offset_lands_exactly_and_clamps`:
  the scrollbar's drag target in both directions plus the depth clamp.
- `crates/ait-config/tests/from_toml.rs` — `suggest_trigger` parsing,
  the `"# "` default, and empty/control-char load errors naming the key.

## Notes for reviewers

- `Selection::finish` dropping collapsed ranges is the shared piece both
  frontends rely on for click-clears + Ctrl+C sanity.
- The pane *focus* model (shown vs focused) is the behavioral core of
  #7/#11/#12: keys/text/paste route only to a *focused* pane; the grid
  click is the focus-release gesture; the focus border + blinking
  carets are the visibility half.
- `input.suggest_trigger` default remains `"# "` per §5.4; `## ` is the
  recommended setting for the paste-conflict concern in #5.
