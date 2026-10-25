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

