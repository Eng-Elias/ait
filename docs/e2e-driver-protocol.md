# E2E driver protocol (v1)

Spec E01, ticket E01-T3. `ait-testkit`'s `agent().act("…")` /
`agent().assert("…")` steps are resolved by a **driver**. The default
`Authored` driver is replay-only — committed `*.jsonl` recordings are
the authored answer. To *resolve* steps at record time
(`AIT_E2E_RECORD=1`) with a real model, point `AIT_E2E_DRIVER` at any
command that speaks the protocol below — `E2e::spawn` installs the
`Command` backing in place of `Authored` when the variable is set.

```bash
export MY_API_KEY=…                        # secrets live in the environment
export AIT_E2E_DRIVER='./scripts/my-driver.sh'
AIT_E2E_RECORD=1 cargo test                # agent steps resolve + re-record
cargo test                                 # replay: no driver, no model
```

No new machinery is involved: the backing is `std::process` only — no
HTTP client, no async runtime. Any endpoint works by wrapping it in a
CLI script.

## How the command runs

- The `AIT_E2E_DRIVER` value executes via `sh -c`, so a plain program
  path works and so does shell syntax (`my-driver --model fast`,
  `python3 ~/driver.py`, …).
- **One fresh subprocess per step** — the protocol is stateless. A
  wrapper that needs memory keeps it in a file.
- The request JSON goes to the child's **stdin**, followed by EOF. The
  response JSON is read from **stdout**. Anything the wrapper logs goes
  to **stderr** — captured verbatim into the failure message and the
  failure-evidence dump when a step fails.
- The child inherits the test process's environment. **Secrets reach it
  only this way** (CODING_STANDARDS §4): export the key in the shell,
  never put it in the command string, a test, a recording, or the repo.
- A step is bounded at **60 s**; a still-running child is killed and the
  step fails.

## Request (stdin)

One JSON object:

```json
{
  "v": 1,
  "kind": "act",
  "goal": "open a new tab",
  "screen": { "columns": 80, "lines": 24, "rows": ["$ "], "cursor": { "line": 0, "column": 2, "shape": "Block" } }
}
```

- `v` — protocol version, always `1` here.
- `kind` — `"act"` or `"assert"`.
- `goal` — present for `"act"`: the natural-language goal to achieve.
- `claim` — present for `"assert"` instead of `goal`: the claim to judge.
- `screen` — the settled `ScreenState` the step is resolved against:
  `columns`/`lines` (grid size), `rows` (visible rows, top→bottom,
  trailing blanks trimmed, the spawned program's cwd redacted to
  `[cwd]`), `cursor` (`line`, `column`, `shape`).

## Response (stdout)

For `act`, the semantic inputs to send, in order — the same three shapes
recordings store:

```json
{ "v": 1, "inputs": [ { "key": "Alt+T" }, { "text": "echo hi" }, { "bytes": "1b5b353e" } ] }
```

- `key` — a named chord (`"Alt+T"`, `"Ctrl+PageDown"`, `"Enter"`), sent
  through the harness's real legacy-terminal encoding.
- `text` — literal UTF-8 text to the program's stdin.
- `bytes` — raw bytes, lowercase hex — the escape hatch (SGR mouse
  reports, bracketed paste, exotic sequences).

For `assert`, the verdict — the same serde shape recordings store:

```json
{ "v": 1, "verdict": "pass" }
{ "v": 1, "verdict": { "fail": { "reason": "there is no second tab" } } }
```

## Versioning

Both sides carry `v`. `ait-testkit` accepts only `v: 1` and fails the
step loudly on any other value — a protocol change bumps the number
rather than silently misparsing an old wrapper.

## Failure semantics

Every deviation fails the step (a test failure, not a crash) with the
child's captured stderr in the message — which lands in the standard
failure dump under `target/e2e-failures/<test>/`:

- the command can't be spawned,
- non-zero exit,
- stdout that isn't valid protocol JSON,
- a `v` other than `1`, or a body that doesn't match the schema,
- still running after 60 s (the child is killed).

## Example wrapper

[`examples/e2e-driver-stub.sh`](examples/e2e-driver-stub.sh) is a
working stub — it answers `act` with canned inputs and `assert` with a
`pass`, kind-detecting by grep rather than parsing. To adapt it to a
real endpoint: read the request (`$(cat)`), POST `goal`/`claim` +
`screen.rows` to your model with `$MY_API_KEY` from the environment,
translate the model's answer into the response shape above, print it to
stdout, exit 0.
