---
name: e2e
description: End-to-end testing with ait-testkit — spawn the real ait binary in a PTY, drive it with keystrokes/mouse, assert on the rendered screen. Use after implementing a feature or spec, when asked for e2e/acceptance coverage, TOML scenarios, or to verify user-visible behavior ("Alt+T opens a tab") end to end.
---

# E2E testing with `ait-testkit`

The seam: `ait` spawned by `ait-term`'s `LiveSession` in a real PTY — keys/bytes/mouse/resize in, rendered grid/cursor/exit/events out. Tests never reach inside `ait`'s internals: if a fact isn't on the screen or the event stream, it isn't asserted. Vocabulary (GLOSSARY.md): **Scenario**, **Recording**, **Driver**, **Replay**, **Drift**.

Full reference: `docs/testing-toolkit.md`. Verb table: `crates/ait-testkit/src/scenario.rs` module docs. Wire protocol: `docs/e2e-driver-protocol.md`. Examples: `crates/ait/tests/scenarios/*.toml`, `crates/ait/tests/e2e.rs`.

## Pick the tier

- **TOML scenario** (default for acceptance criteria): a `.toml` file under `crates/ait/tests/scenarios/` — no Rust needed.
- **Code-first test** (engineering cases: weirder flows, loops, custom predicates): a `#[test]` in `crates/ait/tests/e2e.rs` using `E2e`.
- **`/bin/sh` self-test** (only when testing the *harness* itself): `crates/ait-testkit/tests/`.

## Recipe: map criteria → observable evidence

1. For each acceptance criterion, find what a *user* would observe: exact rendered text (tab bar ` 1:sh`, status `cwd · sh · — · running`), a `ClipboardWrite` event, an exit code, a cursor position. Read the feature's source for the exact strings — assert on those, not on approximations.
2. Choose inputs the way a user produces them: `keys` chords (`Alt+T`, `Ctrl+PageDown`, `Shift+PageUp`, `Alt+1..9`), `text`, `bytes` (hex escape hatch), `mouse` (SGR, 0-based `col`/`row`), `resize`.
3. Assert: `expect_text` / `expect_regex` (line-anchored `(?m)^…$` beats bare substrings that also match command echo), `expect_cursor`, `expect_exit`, `expect_clipboard`. Gate progress with `wait_text`. **Legacy tier only** — the spawned app never sees kitty keyboard, so `Ctrl+Shift+*` aliases are unreachable here (that coverage lives at the keymap seam).

## Recipe: write a scenario

```toml
feature = "tabs"        # required — the filter tag
size = [80, 24]         # optional (also: args, env, headed, slowmo)

[[steps]]
wait_text = "· sh · — · running"   # gate: wait until present

[[steps]]
keys = ["Alt+T"]

[[steps]]
expect_text = " 1:sh  2:sh"        # assert
```

One verb per `[[steps]]`. Drop the file in `crates/ait/tests/scenarios/` — the runner picks it up automatically under its file-stem name. Malformed files fail naming file + step.

## Recipe: agent steps (when a criterion needs natural language)

`act`/`assert` steps resolve through a `Driver`; committed `.jsonl` recordings replay them with zero model calls. Three ways to produce recordings:

1. `AIT_E2E_RECORD=1` + `AIT_E2E_DRIVER='./docs/examples/e2e-driver-stub.sh'` (or your own wrapper) — the driver resolves, recordings are rewritten.
2. Hand-author `tests/recordings/<test>/<step>.jsonl`: line 1 = header (`v`, `step`, `op`, `text`, `text_hash`, `program`, `screen`, `screen_hash`), then one Input per line (`{"key":"Alt+T"}` / `{"text":"…"}` / `{"bytes":"<hex>"}`). `text_hash` via `ait_testkit::text_hash`.
3. Ask the recording's author agent — recordings are authored assets, not generated output.

Replay asserts the recorded screen-state hash: a mismatch is **drift** — a real failure, not a warning. Re-record to update.

## Recipe: run

```bash
cargo build -p ait                              # the binary under test
cargo test -p ait -p ait-testkit                # the whole e2e surface
cargo test -p ait --test scenarios              # TOML scenarios only
cargo test -p ait --test e2e                    # code-first only
cargo test -p ait --test scenarios <name>       # filter by scenario
```

Watch it work: `AIT_E2E_HEADED=1 AIT_E2E_SLOWMO=400 … -- --test-threads=1`. Interactive driving: `cargo run -p ait-testkit --example drive`. Failures dump `screen.txt` + `pty-tail.bin` + `actions.log` under `target/e2e-failures/<test>/` — read those first.

## Recipe: the GUI (`ait-gui`, Spec R15 / ADR-0019)

Same harness, different seam: `GuiE2e` spawns the real `ait-gui` binary and talks to its **test-driver socket** — real `egui::Event` injection for keys/text/paste/wheel/resize, `RawInput` for pointer (the only seam upstream of egui's hit-test), and a versioned JSON `dump` of everything paint draws as the assertion surface. Scenarios are TOML under `crates/ait-gui/tests/scenarios/` (same `feature` + `[[steps]]` shape); code-first tests live in `crates/ait-gui/tests/scenarios.rs`.

```bash
cargo build -p ait-gui --features test-driver                     # required — release builds have no socket
cargo test -p ait-gui --features test-driver --test scenarios     # the GUI suite (skips loudly without a display)
cargo test -p ait-gui --features test-driver --test scenarios <name>
AIT_GUI_E2E_HEADED=1 cargo test -p ait-gui --features test-driver --test scenarios  # on your DISPLAY
```

- **Two binary legs**: `binary = "web"` (embedded webview) vs `"noweb"` (`--no-default-features` build, no webkit) — resolved via `AIT_GUI_E2E_BIN_WEB`/`_NOWEB`, else the cargo-built binary; unresolvable legs skip loudly.
- **Assert the dump, not pixels**: `expect_text`/`expect_pane`/`expect_scroll`/`expect_explorer`/`expect_browser`/`expect_detect`/`expect_status`/`expect_toast`/`expect_tabs`/`expect_suggesting`/`expect_find`/`expect_gate`/`expect_selection`/`expect_hit`. `expect_hit` gates the hit registry — click targets register a frame after model state; use it before `click`.
- **Click by name**: `click = "pill.scroll_bottom"` / `pointer = {event, target|x, y}` resolves the hit registry (`panes.button`, `gate.approve`, `explorer.entry.<name>`, `detect.url.<u>`); raw `x`/`y` is the escape hatch. `scroll` needs `raw = true` + a `target`/`x`/`y` hover for surfaces that own their paint (explorer listing).
- **`agent = {event="approve", …}`** stages a real gate card; **`stub_opener = true`** records `xdg-open`/`open` for `expect_open` — `ext ↗` and editor spawns without real apps.
- **Failures**: `target/e2e-failures/<test>/` holds `dump.json` + `stderr.txt` + `actions.log`.
- **Canary only**: `AIT_GUI_E2E_XTEST=1` runs the xdotool/Xvfb winit canary — non-authoritative, proves winit delivers real input, never a gate.
- Needs `xvfb` (or `DISPLAY`), `mesa-vulkan-drivers` (lavapipe; pin `WGPU_ADAPTER_NAME=llvmpipe`+`WGPU_BACKEND=vulkan`), and the webkit2gtk-4.1 stack for the `web` leg.

## Rules

- Bounded waits only (`wait_*`, `expect_*`) — never fixed sleeps in tests.
- Commit scenarios, recordings (`*.jsonl`) and snapshots (`*.snap`) — they're authored assets.
- Everything runs with no TTY and no network; spawned env is pinned (`SHELL`, `PS1`, temp `cwd`, `TERM`).
- Done when: the criterion's scenario/test exists, `cargo test --workspace` is green, and the new assets are committed.
