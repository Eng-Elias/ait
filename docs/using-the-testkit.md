# Using `ait-testkit` — quick guide

One-line pitch: drive the **real `ait` binary** in a real PTY with
keystrokes and mouse, assert on the rendered screen. Deep docs:
[testing-toolkit.md](testing-toolkit.md) (architecture, record/replay,
dumps). Agent skill: `.devin/skills/e2e/SKILL.md`.
GUI frontend (`ait-gui`) has its own harness (`ait_testkit::gui`,
Spec R15): TOML scenarios under `crates/ait-gui/tests/scenarios/`
drive the real binary under Xvfb (or the developer's `DISPLAY` with
`AIT_GUI_E2E_HEADED=1`) over a feature-gated socket. Build it with
`cargo build -p ait-gui --features test-driver` and run
`cargo test -p ait-gui --features test-driver --test scenarios`.

The suite is a dual-binary matrix (R15-T5): the `web`+`test-driver`
build covers the embedded-webview surface, the
`--no-default-features`+`test-driver` build (no webkit packages
needed) covers the external-open fallback. A scenario declares its
leg with `binary = "web"` / `"noweb"` (absent = any binary). Each
leg resolves through `AIT_GUI_E2E_BIN_WEB` / `AIT_GUI_E2E_BIN_NOWEB`
first, else the cargo-built binary when its feature set matches —
so `cargo test -p ait-gui --features test-driver` runs the `web`
leg and skips `noweb` scenarios loudly, `--no-default-features
--features test-driver` does the inverse, and setting the other
leg's env var to a separately built binary runs both in one pass:

```bash
cargo build -p ait-gui --features test-driver                          # web leg
cargo build -p ait-gui --no-default-features --features test-driver \
  --target-dir target/gui-noweb                                      # noweb leg
AIT_GUI_E2E_BIN_NOWEB=$PWD/target/gui-noweb/debug/ait-gui \
  cargo test -p ait-gui --features test-driver --test scenarios
```

(The env paths are absolute — the harness spawns each app in a
throwaway cwd, where a relative path would dissolve.)

Every spawn gets recording `xdg-open`/`open` stubs first on the
app's `PATH` (no flag — `pinned_env` always installs `stub-bin/`)
— `expect_open = "<url substring>"` then waits on the stub's log,
so `ext ↗`, external Enter navigations and editor spawns assert
without real applications.

A third display route exists for tests that own the X server
themselves: `GuiSpawnConfig::display` pins the app to a named
`DISPLAY` — no ambient display or `xvfb-run` needed — and
`GuiE2e::pid()` hands back the spawned pid so `_NET_WM_PID` window
lookup works. That is the winit canary's seam
(`crates/ait-gui/tests/winit_canary.rs`, R15-T7): with
`AIT_GUI_E2E_XTEST=1` and `xdotool`+`Xvfb` on `PATH` it raises a
private `Xvfb :N` and injects a chord and a click *externally* over
XTest, asserting through the dump that winit delivered them — the
one layer socket injection skips. Canary only, non-authoritative:
it loud-skips when the env flag or the tools are absent, and it
never uses a shared display (a synthetic click would land on real
windows). CI runs it on the promotion pipeline's `gui-e2e` job.

Verbs: `key`/`text`/`paste`/`scroll`/`resize` (input through the
app's own dispatch — `scroll` adds `target`/`x`/`y` aiming and
`raw = true` for a real `MouseWheel` batch, the only delivery a
pane-own-paint surface like the explorer's listing answers),
`pointer = {event, target|x, y, …, modifiers?}` and
`click = "hit.name"` (real egui pointer events aimed at the hit
registry — `panes.button`, `panes.chip.<name>`, `gate.approve`,
`pill.scroll_bottom`, `browser.back`/`browser.forward`/
`browser.reload`/`browser.external`, `detect.url.<u>`,
`detect.path.<p>`, `detect.<n>`, `explorer.up`,
`explorer.entry.<name>`, `explorer.row.<n>`),
`agent = {event="approve", name, summary}` (stages a real gate
card through `AgentPanel::apply`), plus waits/asserts
`wait_text`/`wait_dump` (the input barrier — the dump's
`input_done` catching `input_seq` proves every accepted request
folded through a completed frame; the sync point a negative
assertion needs since the pump answers `dump` off the UI thread)/
`expect_text`/`expect_pane`/`expect_find`/
`expect_scroll`/`expect_suggesting`/`expect_gate`/
`expect_selection`/`expect_hit` (the registry gate — use it when a
model expectation like `expect_pane` can land a frame before the
affordance's paint), `expect_browser = {surface, reason, entry,
entry_contains, bad_entry, history, history_min, current,
current_contains, back, forward, loads, loads_min, webview}`
(every present key must hold — `loads` counts webview-bound
fetches, so a reload or a rejected URL is provable),
`expect_explorer` (the explorer model — `dir`/`root` as path
suffixes, `entries`/`has`/`lacks` over the visible listing,
`selected`, `hidden`, `scroll`, `preview` kind/head/`truncated`),
`expect_detect` (the Ctrl-hover affordance — `"none"`,
`"url:<u>"`/`"path:<p>"` or `{ctrl, token}`; pair `ctrl = true`
with `token = "none"` to prove a non-affording hover frame was
processed — combine with `wait_dump` first so the hover batch is
provably folded), `expect_open` (the stub-opener log),
`expect_pane_selected` (the selection *under* the agent claim —
`expect_pane` asserts the shown region instead), `expect_status`
(the status strip's composed text — toast while live, else the
session summary plus a `suggesting` segment while a request is in
flight), `expect_toast` (`true`/`false`/substring on a *live*
toast) and `expect_tabs` (the tab strip's count), and `quit`. The
dump's `browser` object carries the surface verdict
(`unknown`/`embedded`/`external` + the external reason line), the
address entry and its amber flag, history depth/position, the
webview-bound load tally and the staged webview rect (`null` is
the hide signal); it also carries
`explorer`/`detect`/`agent`/`toast`/`status`/`tabs` objects for
code-first legs — e.g. `ctl_and_driver_sockets_coexist` runs the
real `ait ctl` client against the spawned app's pinned
`XDG_RUNTIME_DIR` (`GuiE2e::runtime_dir`) while the driver socket
stays bound. `env = {K = "v"}` layers extra variables after the
harness pins; `stub-bin/` shadows `xdg-open`/`open` first on
`PATH` and records spawns to `$AIT_GUI_E2E_STUB_LOG`
(`GuiE2e::stub_log()` in code-first tests). The manual checklist
stays in [gui-verification.md](gui-verification.md).

A GUI scenario is the same TOML shape with the driver's verbs —
drop a file in `crates/ait-gui/tests/scenarios/`:

```toml
feature = "scroll"                # required tag; `binary = "noweb"` pins a leg

[[steps]]
wait_text = "$"                   # gate progress on the grid, not a sleep

[[steps]]
text = "seq 1 200 | sed 's/^/X/'"

[[steps]]
key = "Enter"

[[steps]]
scroll = { unit = "line", dy = 3.0 }    # injects egui::Event::MouseWheel

[[steps]]
expect_scroll = 3                 # dump assert: the model's scroll offset

[[steps]]
expect_hit = "pill.scroll_bottom" # the registry gate — the pill painted

[[steps]]
click = "pill.scroll_bottom"      # a real egui click on the named rect

[[steps]]
expect_scroll = 0

[[steps]]
quit = true
```

## Run

```bash
cargo build -p ait                       # build the binary under test
cargo test -p ait -p ait-testkit         # the whole e2e suite
cargo test -p ait --test scenarios       # TOML scenarios only
cargo test -p ait --test e2e             # code-first tests only
```

## Write a scenario (acceptance coverage, no Rust)

Drop a file in `crates/ait/tests/scenarios/` — it runs automatically:

```toml
feature = "tabs"          # required tag
size = [80, 24]

[[steps]]
wait_text = "running"     # gate progress

[[steps]]
keys = ["Alt+T"]          # chords: Alt+T/W/Q, Ctrl+PageUp/Down,
                          # Alt+1..9, Shift+PageUp/Down/Home/End, Shift+Insert

[[steps]]
expect_text = " 1:sh  2:sh"
```

Other verbs: `text`, `bytes` (hex), `mouse = {event, col, row}`,
`resize`, `expect_regex`/`expect_cursor`/`expect_exit`/
`expect_clipboard`, plus `act`/`assert` agent steps (recorded as
`.jsonl` under `tests/recordings/` — regenerate with
`AIT_E2E_RECORD=1 AIT_E2E_DRIVER=<cmd>`; replay needs no model).

## Wait on execution, not echo

`wait_text` and `expect_*` see everything the PTY renders — including
the shell's echo of the command you just typed. Gate on a marker only
a *running* command can produce, or the wait fires early and the test
asserts on a screen from before the command ran:

- `wait_text = "FLOOD-DONE"` after `text = "echo FLOOD-DONE\n"`
  matches the echo, not the output. Use shell expansion to separate
  them: send `echo FD$((100+24))`, wait for `FD124`.
- The same trap lives in Rust-side waits (`drain_until`,
  `wait_for_screen`) and in prompt detection itself: a wait that
  matches the cursor's current row fires early when the prompt ends
  exactly at the right margin — check for any rendered non-empty row
  instead.

Code-first tests share `ait_testkit`'s probes rather than re-rolling
these gates: `wait_fresh_prompt` (every prompt char, `#` included for
uid-0 shells whose PS1 `\$` renders `#`), `terminal_half` /
`panel_has_line` (the `│` split), `sentinel_result` (the
`__AIT_END_<id>=<exit>` result row — never the echo's `=%d`) and
`json_string` (`src/probes.rs`).

## Watch it run

```bash
AIT_E2E_HEADED=1 AIT_E2E_SLOWMO=400 \
  cargo test -p ait --test scenarios -- --test-threads=1
cargo run -p ait-testkit --example drive   # interactive manual driving

# GUI headed mode — spawns on your real DISPLAY instead of xvfb;
# AIT_GUI_E2E_SLOWMO adds a fixed delay after each input request
# (a scenario's `slowmo = <ms>` header does the same per file):
AIT_GUI_E2E_HEADED=1 AIT_GUI_E2E_SLOWMO=400 \
  cargo test -p ait-gui --features test-driver \
  --test scenarios scroll-pill-click
```

## When it fails

`target/e2e-failures/<test>/` → for the PTY suite `screen.txt`,
`pty-tail.bin`, `actions.log`; for the GUI suite `dump.json` (the
last full `dump` — grid text, pane/browser/explorer state, the whole
hit registry), `stderr.txt` (the app's stderr tail — wgpu adapter
pick, webkit errors), `screenshot.png` (the app's real painted
frame — a `screenshot` request queues egui's
`ViewportCommand::Screenshot`, the post-render callback encodes the
`ColorImage` under a `.part` sibling and renames it into place;
best-effort, evidence for humans only — never a pixel diff) and
the same `actions.log` (one step per line, `screen=<hash>` marking
drift between dumps). The `gui-e2e` CI job uploads the directory
as an artifact on failure. A stuck terminal after a headed run:
`reset`.

## Driving the agent panel and remote sessions

Three traps from the R09 ssh transcript test — each costs a debug
cycle when learned from a timeout dump instead:

- **The task input is ~30 columns.** `type_text` into a focused panel
  scrolls long input — a `contains` wait on your task text never
  fires (only the tail stays visible). Gate on what the panel does
  next — the `approve <tool>` card, a `✓ <tool>` result, the `done`
  status via `panel_has_line` — or probe the input's *tail*
  (`contains("remote-marker.txt")` for a task ending in that name,
  marked `sentinel-ok` since it keys on echo by design).
- **`Esc` unfocus races queued input.** `Esc` hands focus back to the
  terminal, but bytes typed in the same breath can still land in the
  panel's input (`exit` became `xit` mid-flight). `Alt+A` toggles the
  panel itself — on a focused panel it closes it (the documented
  three-state cycle) — a deterministic return of keystrokes to the
  PTY, and a full-width terminal for any `terminal_half` probes after
  it.
- **Remote confinement roots at the *remote* cwd.** With remote
  context open, the agent's file tools resolve through the remote
  shell's cwd — `/root` on a loopback sshd — not the spawn's local
  tempdir; `fs::write(app.cwd(), …)` seeds a file the remote read
  can't see. Seed fixtures over the remote channel itself (an
  `echo MARKER > file` block run inside the wrapped session) so the
  agent's read exercises the real path.

## For AI agents

Two ready prompts — paste either into an agent session that has the
`e2e` skill (or point it at `.devin/skills/e2e/SKILL.md`).

**Cover a feature** — fill in `<FEATURE>` (e.g. "tab close",
"session-exit status"):

```prompt
Add e2e coverage for <FEATURE> with ait-testkit. Read .devin/skills/e2e/SKILL.md
and docs/testing-toolkit.md. (1) Find the feature's user-visible surface by
reading its source: exact screen text, emitted events, exit paths — assert on
those, not approximations. (2) Write one TOML scenario per acceptance criterion
in crates/ait/tests/scenarios/ (feature tag "<FEATURE>"), or a code-first test
in crates/ait/tests/e2e.rs where TOML can't express the flow. (3) Run
cargo build -p ait && cargo test -p ait -p ait-testkit until green — on failure
inspect target/e2e-failures/<test>/ first; fix the scenario or the product code,
never weaken an assertion to hide a real bug. (4) Commit scenarios/tests plus
any tests/recordings/*.jsonl and tests/snapshots/*.snap produced.
```

**Audit coverage + repair after a change:**

```prompt
Audit e2e coverage project-wide and fix broken tests. Read
.devin/skills/e2e/SKILL.md and docs/testing-toolkit.md. (1) List user-visible
features (REQUIREMENTS.md §5.2 stories, the keymap bindings, rendered screens)
and map each to scenarios/tests under crates/ait/tests/ — report the gaps
before writing anything. (2) Write TOML scenarios for the missing ones. (3) Run
cargo build -p ait && cargo test -p ait -p ait-testkit; for every failure,
diagnose via target/e2e-failures/<test>/: if the behavior change was intended,
update the test or re-record (AIT_E2E_RECORD=1 AIT_E2E_DRIVER=<cmd>); if not,
fix the product code. Never weaken an assertion to silence drift. (4) Keep
running until green and commit.
```


## Flaky tests

Timing-bound tests (real PTYs, settle windows, parallel load) flake;
the fix ladder is ordered. (1) Deterministic seam first — gate waits
on output only execution produces (standard 15) and on protocol marks
in a real emitter's order (standard 16); a scripted-but-fictional
order is a test bug, not coverage. (2) Bound, not guess — a settle
window or deadline is the last resort, sized for the slowest CI box
and named as a constant the next ticket can tighten. (3) Retry is the
honest last resort — mark the retry in the test name or comment so
flakes surface in the dump instead of hiding; never weaken an
assertion to silence drift.

The canonical flake list is `docs/agents/flaky-tests.md` (with
`scripts/flaky-tests.txt` as the machine-readable copy check.sh
consumes) — add a name there when a new one surfaces, and file the
real fix on the ladder above.
