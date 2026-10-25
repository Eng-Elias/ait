# Using `ait-testkit` — quick guide

One-line pitch: drive the **real `ait` binary** in a real PTY with
keystrokes and mouse, assert on the rendered screen. Deep docs:
[testing-toolkit.md](testing-toolkit.md) (architecture, record/replay,
dumps). Agent skill: `.devin/skills/e2e/SKILL.md`.

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

## Watch it run

```bash
AIT_E2E_HEADED=1 AIT_E2E_SLOWMO=400 \
  cargo test -p ait --test scenarios -- --test-threads=1
cargo run -p ait-testkit --example drive   # interactive manual driving
```

## When it fails

`target/e2e-failures/<test>/` → `screen.txt`, `pty-tail.bin`,
`actions.log`. A stuck terminal after a headed run: `reset`.

## For AI agents

Paste this into the agent's prompt:

> This repo has `ait-testkit` for e2e testing: it spawns the real `ait`
> binary in a PTY and asserts on the rendered screen. Read
> `.devin/skills/e2e/SKILL.md` for the workflow and
> `docs/testing-toolkit.md` for reference. To cover my feature: write a
> TOML scenario in `crates/ait/tests/scenarios/` (see the existing ones
> and the verb table in `crates/ait-testkit/src/scenario.rs`), or a
> code-first test in `crates/ait/tests/e2e.rs`. Run with
> `cargo test -p ait -p ait-testkit`. Commit the scenario/test plus any
> `tests/recordings/*.jsonl` and `tests/snapshots/*.snap` it produces.
