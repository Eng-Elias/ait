# Running the GUI test suite

How to run the `ait-gui` end-to-end suite — fast, or slowed down so you
can watch each scenario play on your real screen. For the full harness
reference see `docs/using-the-testkit.md`; this page is the cheat sheet.

## Prerequisites

- The suite needs the `test-driver` feature: always pass
  `--features test-driver` to `cargo test`.
- Headless runs need `xvfb-run` (installed by default on most distros).
- Headed (watch) runs need a real X11/XWayland display — your normal
  desktop session works (`$DISPLAY` must be set).
- The webview scenarios need the `webkit2gtk-4.1` stack; without it
  those legs skip (use the no-web binary, below, to skip them
  deliberately).

## Fast run (the default, what CI-style gates use)

```bash
cargo test -p ait-gui --features test-driver --test scenarios
```

Runs under Xvfb — no windows appear, ~35s. This is the normal
development run; nothing else is needed.

## Watch mode — real windows, slowed down

```bash
AIT_GUI_E2E_HEADED=1 AIT_GUI_E2E_SLOWMO=600 \
  cargo test -p ait-gui --features test-driver --test scenarios
```

- `AIT_GUI_E2E_HEADED=1` — spawn on your `$DISPLAY` instead of Xvfb.
- `AIT_GUI_E2E_SLOWMO=<ms>` — fixed delay before each input (key,
  click, scroll…). 400–800ms is a comfortable watch pace. A scenario
  can pin its own pace with a `slowmo = 400` TOML header.
- Waits are still predicate-based — a paced run observes the same
  sequence, just slower. Omit `SLOWMO` for full-speed headed runs.

## One window at a time

The TOML scenarios already run back-to-back inside the `scenarios`
test; what runs in parallel are the `#[test]` functions themselves.
Serialize the whole binary for a strict one-window-at-a-time watch:

```bash
AIT_GUI_E2E_HEADED=1 AIT_GUI_E2E_SLOWMO=600 \
  cargo test -p ait-gui --features test-driver \
  --test scenarios -- --test-threads=1
```

## Run one (or a few) scenarios

`AIT_GUI_E2E_ONLY=<text>` keeps only the TOML legs whose file stem
contains it — a miss fails listing every available stem:

```bash
# just explorer-browse:
AIT_GUI_E2E_ONLY=explorer-browse \
  cargo test -p ait-gui --features test-driver --test scenarios scenarios

# every scenario with "browser" in the name:
AIT_GUI_E2E_ONLY=browser \
  cargo test -p ait-gui --features test-driver --test scenarios scenarios
```

The trailing `scenarios` filters the test function so the code-first
tests don't run too. To run a code-first test by name instead:

```bash
cargo test -p ait-gui --features test-driver \
  --test scenarios resize_settle
```

## The no-web leg

A second binary built without webview dependencies proves the
no-webview scenarios still behave (browser pane falls back to
external-open). Build it once, then point the suite at it:

```bash
cargo build -p ait-gui --no-default-features --features test-driver \
  --target-dir target/gui-noweb
AIT_GUI_E2E_BIN_NOWEB=$PWD/target/gui-noweb/debug/ait-gui \
  cargo test -p ait-gui --features test-driver --test scenarios
```

## Environment variables

| Variable | Effect |
| --- | --- |
| `AIT_GUI_E2E_HEADED=1` | spawn on your `$DISPLAY` instead of Xvfb |
| `AIT_GUI_E2E_SLOWMO=<ms>` | delay before each input — watch pacing |
| `AIT_GUI_E2E_ONLY=<text>` | run only TOML scenarios whose stem matches |
| `AIT_GUI_E2E_BIN_WEB=<path>` | override the web-enabled binary path |
| `AIT_GUI_E2E_BIN_NOWEB=<path>` | no-web binary for the second leg |
| `AIT_GUI_E2E_XTEST=1` | non-authoritative XTest canary (never a gate) |

`-- --test-threads=1` (a cargo flag, not an env var) serializes the
test functions.

## When it fails

Every failure leaves evidence under `target/e2e-failures/<name>/`:
`dump.json` (last observed state), `stderr.txt`, `actions.log`, and a
best-effort `screenshot.png`.
