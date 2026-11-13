# 19. GUI e2e: in-app event injection, a structured dump, a second socket, and ait-testkit as harness home

Date: 2026-11-14

## Status

Accepted

## Context

`ait-gui`'s correctness surface is winit/egui event behavior, and
Round 13 shipped four regressions tests and review both missed: a
dead gate-card click layer, a touchpad-scroll dead-zone, an Alt-chord
text twin leak and resize jank — every one caught by hands
(`docs/gui-verification.md`'s opening says it plainly). Spec R15
(§5.12's testing round) turns that manual checklist into an
executable suite. Four seam decisions were settled in the grilling
and land here together:

1. **Where input enters.** An honest e2e drives input the way a
   user's arrives. The honest-enough seams: external injection over
   XTest/xdotool (the whole winit→egui→app stack, but blind to app
   state and hostage to whatever display stack the runner has — a
   leaked `WAYLAND_DISPLAY` alone sends the window to the real
   compositor even with `WINIT_UNIX_BACKEND=x11`); in-app injection
   of real `egui::Event`s (everything downstream is production code,
   only winit's own delivery goes un-driven); a re-implemented event
   loop or headless window (would re-test the test code).
2. **What scenarios assert on.** Options: pixel diffs/screenshot
   comparison (rejected in the spec — fonts, llvmpipe and timing make
   it a flake farm), OCR, or a structured dump of what paint draws.
3. **How the harness talks to the app.** The production ctl socket
   (ADR-0014) already carries one-line JSON requests — but it is a
   shipped surface `ait ctl` users drive, and input injection must
   never exist on it. A second socket keeps ctl's request enum and
   CLI untouched.
4. **Where the harness lives.** `ait-testkit` already owns the e2e
   machinery — bounded waits, `target/e2e-failures/` evidence, the
   TOML scenario runner — and is dev-dependency-only. A new crate
   would fork that machinery for a boundary that buys nothing.

## Decision

- **In-app `egui::Event` injection is the primary input seam; XTest
  is a canary only.** Driver verbs queue real `egui::Event`s into
  `Shared::injected`, consumed at the top of `dispatch_events` each
  frame — `Event::Key` press/release pairs carrying the chord's
  *source* modifier bits (with `twin` reproducing the twin-`Text`
  shape some platforms emit for `Alt+<letter>` — the R13 leak),
  `Event::Text`, `Event::Paste`, `Event::MouseWheel` in `line`,
  `pixel` (sub-line touchpad deltas) and `page` units, and viewport
  resize as a real `ViewportCommand::InnerSize`. Everything below is
  production input code: `translate_key`/`translate_text`, the
  keymap, overlay swallowing, the pane host and the pump. Pointer
  gestures ride a different seam — `DriverInputPlugin`'s
  `input_hook` injects `RawInput` events — because `dispatch_events`
  runs mid-pass, after `InputState::begin_pass` has already folded
  the frame's events into `PointerState`; only `RawInput` injection
  reaches egui's hit-test, so hover, press, click and drag update
  exactly as they do for winit-delivered input. The one layer left
  un-driven — winit's delivery into egui — gets a single env- and
  tool-gated canary (`tests/winit_canary.rs`, `AIT_GUI_E2E_XTEST=1`,
  own `Xvfb`, `xdotool`): it proves delivery and nothing more,
  loud-skips without the tools, and is never a gate for checklist
  behavior.
- **The assertion seam is a structured, versioned `dump` —
  screenshots are evidence, never diffs.** A `dump` request answers
  a JSON snapshot (`DUMP_VERSION = 1`; additive fields stay v1,
  shape changes bump it) drawn from the same `Shared` state `ui`'s
  paint pass reads: visible grid rows + cursor, cell metrics, scroll
  position and selection, the pane selection, the browser pane's
  surface verdict/address entry/history depth/webview-bound load
  tally/staged rect, the explorer's listing/selection/preview, the
  find bar, the suggestion arm, the agent panel's open/focus/gate
  state, the Ctrl-hover `detect` affordance, the toast, the status
  strip's composed text, the tab strip, the window's content size —
  and `hits`, the hit-target registry paint fills every frame
  (`gate.approve`, `pill.scroll_bottom`, `panes.chip.<name>`,
  `explorer.entry.<name>`, `detect.url.<u>`, …). Scenarios assert
  `expect_*` predicates over dump fields — the things a user's eye
  reads — and `click`/`scroll`/`pointer` resolve a registry name to
  its rect's center; raw `x`/`y` stays the escape hatch.
  `ViewportCommand::Screenshot` PNGs land in failure evidence for
  humans; pixel-diff and OCR gating are rejected.
- **A separate, feature-gated test-driver socket — ctl untouched.**
  A second Unix socket, bound only when `AIT_TEST_DRIVER_SOCKET`
  names a path (the harness picks the path inside its throwaway dir,
  `TestDriver::bind` chmods the parent `0700`), speaks the ctl
  socket's wire conventions exactly: nonblocking `UnixListener`, one
  single-line JSON request/response per connection, buffered
  pending connections with the same `PENDING_TTL`, socket `0600`,
  unlinked on drop, drained in the pump loop beside ctl. The whole
  module compiles only under the off-by-default `test-driver` cargo
  feature — release artifacts carry none of it, and even a
  `test-driver` build binds nothing without the env var. The
  control socket's request enum and `ait ctl` are untouched;
  `ctl_and_driver_sockets_coexist` proves both sockets service one
  window at once.
- **`ait-testkit` is the harness's home — no new crate.** Its `gui`
  module carries `GuiE2e`/`GuiSpawnConfig`/`GuiBinary`
  (`any`/`web`/`noweb`)/`GuiScenario`/`GuiStep`: spawn the real
  `ait-gui` binary under `xvfb-run` (or `AIT_GUI_E2E_HEADED=1` on
  the developer's display, or `GuiSpawnConfig::display` for tests
  that own the X server), connect the socket, verb methods, bounded
  `wait_for_dump` polling plus the `wait_dump` input barrier
  (`input_done` catching `input_seq` — see Consequences), the
  recording opener stubs (`install_opener_stubs` — every spawn's
  `stub-bin/` shadows `xdg-open`/`open` and appends invocations to
  `AIT_GUI_E2E_STUB_LOG`), and
  `dump.json`+`stderr.txt`+`actions.log`+`screenshot.png` evidence
  under `target/e2e-failures/<test>/` on panic. It stays
  dev-dependency-only; `ait-gui` adds `ait-testkit` beside
  `tempfile`/`ait-store` as a dev-dependency.
- **The suite is a dual-binary matrix.** `cargo build -p ait-gui
  --features test-driver` covers the `web` surface;
  `--no-default-features --features test-driver` covers the
  external-open fallback with no webkit packages. A scenario
  declares `binary = "web"`/`"noweb"` (absent = any); legs resolve
  through `AIT_GUI_E2E_BIN_WEB`/`_NOWEB`/`AIT_GUI_E2E_BIN` first,
  else the cargo-built binary when its feature set matches —
  unresolvable legs skip loudly, never fail.
- **Rejected.** XTest/xdotool as the primary seam (display-env
  fragility, no state assertions, slow); a headless or
  re-implemented event loop (the spec's real-binary rule — the
  harness spawns the same artifact users run); pixel-diff/OCR
  assertions; ctl-verb injection (a shipped surface must never grow
  an input-injection reach); a `ait-gui-testkit` crate (forks the
  evidence/wait/scenario machinery); AccessKit/AT-SPI as the test
  seam (the a11y tree stays a user surface, not a probe).

## Consequences

- `docs/gui-verification.md`'s checklist is mostly executable: 18
  TOML scenarios under `crates/ait-gui/tests/scenarios/` plus
  code-first tests cover it; the items left manual are feel
  (touchpad flick, WM edge-drag), rendering throughput (webview
  video, long-scrollback pacing), the remote-session gate and the
  `last-block` happy path — each marked with why.
- `cargo test --workspace` and `scripts/check.sh` stay
  display-free: the GUI suite needs `--features test-driver` and a
  display, and skips loudly without either. CI runs it once on the
  main-only promotion pipeline's `gui-e2e` job (Xvfb,
  `mesa-vulkan-drivers` pinned to lavapipe via
  `WGPU_ADAPTER_NAME=llvmpipe` + `WGPU_BACKEND=vulkan`, `xdotool`
  for the canary, the webkit2gtk-4.1 stack for the `web` leg) —
  the PR-to-`dev` minutes budget is untouched.
- The suite already earned its keep: WebKitGTK navigation reports
  arrive asynchronously over IPC, and the browser scenarios caught
  our own `load_url` echoes racing real in-page nav reports into
  history — T5 added the echo ledger to keep them distinct. A
  product bug the manual checklist never reached.
- The review pass hardened the harness itself (Spec R15 follow-up):
  the ctl socket's transport half — `Pending`'s buffered
  request/flush lifecycle, `bind_socket`, `set_mode` — is exported
  from `ait_app::ctl` and the test driver *uses the same code*, the
  spec's "ctl's conventions exactly" made literal rather than a
  twin implementation; hit-target names became lazy thunks so
  release builds never pay the `format!`s the registry drops; the
  `screenshot` verb landed end to end
  (`Context::request_screenshot` → post-render PNG, staged via a
  `.part` rename); the dual stub-opener mechanisms were unified on
  `pinned_env`'s always-on `stub-bin`/`open.log`; the explorer
  OSC-7 fixtures now run real integrated `/bin/bash` sessions (the
  `ait-shell` snippet's precmd emits the report — no printf'd
  stand-ins); and `scripts/check.sh` gained a
  `--features test-driver` clippy leg since the default build never
  lints `test_driver.rs`.
- **`wait_dump` exists because `dump` alone is not a barrier.** The
  pump answers the socket off the UI thread, so a `dump` can return
  before the queued input's frame folded — the R15 review found a
  300 ms sleep covering exactly that gap in a negative assertion.
  `dispatch` bumps `Shared::input_seq` per request that queues
  frame-folded input; `ui` sets `input_done` to it at frame end
  once `injected` is drained AND the pointer plugin's queue
  emptied. The dump carries both; `input_done >= input_seq` IS
  "everything the socket accepted ran through a completed frame" —
  the bounded barrier negative assertions now wait on.
- Display pinning is env-scrubbing, not flag-setting: the harness
  `env_remove`s `WAYLAND_DISPLAY`/`DISPLAY` it doesn't own —
  setting `WINIT_UNIX_BACKEND=x11` is not enough.
- `[workspace.metadata.dist]` still has no `dependencies.apt`
  section: when `ait-gui` first ships, the glibc `web` release legs
  will need the webkit2gtk-4.1/gtk/soup dev packages installed in
  `.github/build-setup.yml` (or a `--no-default-features`
  decision). Flagged for release readiness, not fixed here.
- New flake surface is registered per the testkit ladder:
  `ait-gui scenarios` (the whole TOML runner) is in
  `scripts/flaky-tests.txt`/`docs/agents/flaky-tests.md` after the
  `scroll-pill-click` cold-boot sighting.
- `dump` fields are a contract with tests, not paint internals:
  additive changes stay `v` 1; anything renaming or reshaping bumps
  `DUMP_VERSION` and the testkit's decode in lockstep.
