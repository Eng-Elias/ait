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
