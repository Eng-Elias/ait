# Standards-axis review — ait MVP (Rounds 1–4)

Diff reviewed: `696048d…888c41c` (~157 files, +32k). Scope: all Rust under
`crates/` (`ait-term`, `ait`, `ait-llm`, `ait-config`, `ait-agent`,
`ait-testkit`) plus manifests, ADRs, scripts and docs. Rules applied:
`CODING_STANDARDS.md` §§1–14, `AGENTS.md`, ADR-0002–0007, then the Fowler
smell baseline (judgment calls, labelled). Tooling-enforced issues
(fmt/clippy/`check-dep-comments.py` — the latter passes) excluded.

**Verdict: one low-severity documented-rule breach, no code-level standard
violations.** The sensitive paths the standards exist for — secrets,
redaction, sanitization, key-before-PTY ordering, empty-state guards,
stable async identity — are implemented carefully and cited inline.

## Documented-standard findings

### 1. (Low) `REQUIREMENTS.md` Async row contradicts ADR-0004's amendment

`AGENTS.md` ground rule: "an ADR wins over `REQUIREMENTS.md`. Update
`REQUIREMENTS.md` in the same PR that adds a conflicting ADR."

ADR-0004's 2026-10-28 amendment records that tokio landed inside the
library crates (`ait-llm`, `ait-agent`) behind synchronous facades while
the `ait` binary stayed tokio-free. But `REQUIREMENTS.md` §2 still reads:

> `| Async | tokio | App runtime at the `ait` binary (agent loop, HTTP, SSE). … |`

`crates/ait/Cargo.toml:16-18` confirms the amendment ("ait never gains a
tokio dependency (ADR-0004, amended 2026-10-28)"). The row should have been
updated in the PR that landed the amendment. Docs-only drift; no code
impact.

## Fowler smell findings (judgment calls)

### 2. Duplicated Code — the config→llm schema bridge is verbatim in two places

`fn llm_schema` is character-identical in
`crates/ait-agent/src/suggester.rs:412-417` and
`crates/ait-llm/examples/chat.rs:233-239`. Deliberate (the comment explains
`ait-llm` must stay config-free), and a shared helper cannot live in either
crate without a new dependency edge — but a third copy will silently
diverge. Acceptable, worth a comment-level fix only.

### 3. Repeated Switches — per-schema three-arm matches multiplied

Four sites dispatch on the same three `EndpointSchema` variants with
near-identical arm structure: `ait-llm/src/endpoint.rs:92-148`
(construction, stream dispatch), `ait-agent/src/context.rs:70-113`
(`assemble_request`), `ait-agent/src/suggester.rs:374-398` (`extract`), and
`examples/chat.rs:244-397` (`build_request`, `render_event`). Standard §3
("keep per-schema IRs") mandates the separation, so the switches are the
endorsed cost — flagged per baseline, suppressed in priority. Adding a
fourth schema means editing all four sites (Shotgun Surgery), which the
standard accepts.

### 4. Speculative Generality — inert fields and events carried for later rounds

`ExecutionProfile` (`ait-config/src/settings.rs:183-235`) parses,
validates and schema-emits 11 permission fields (`mcp_permissions`,
`command_allowlist`, `directory_allowlist`, …) that nothing consumes
until Round 7 — doc says "inert". `SessionEvent::PromptMark`
(`ait-term/src/events.rs:24-31`) is likewise recorded but unused until
Round 5. Both are spec-sanctioned (§5.7 schema stability; §5.5 "record
the marks"), so this is a deliberate trade rather than creep — standard
12 tolerates it because each is tied to a written spec section.

### 5. Divergent Change — `app` and `suggest` carry several reasons to change

`crates/ait/src/app.rs` (~850 product lines) coordinates key dispatch,
mouse, tab transitions, config reload, suggester draining and toasts —
normal for a TUI root but the file has many axes of change.
`crates/ait/src/suggest.rs` (1088 lines) mixes `InputShadow` bookkeeping,
§5.4 context collection and the `SuggestionPin` state machine. Sharpest
edge: `cwd_for` (`suggest.rs:267-276`) reads `/proc/<pid>/cwd` — a
system-call context-collection concern sitting in the frontend crate,
while `ait-agent` owns the matching `assemble_request` seam. Borderline
standard-9 (frontend holds presentation+routing only); arguably the tab's
own data legitimately lives frontend-side. Judgment call.

### 6. Standard-8 borderline — heavyweight deps unconditional in `ait-config`

`keyring` (zbus Secret-Service stack on Linux), `notify` and `schemars`
are unconditional in `crates/ait-config/Cargo.toml:25-37`. Standard 8
wants heavyweight/optional deps behind features; `tiny_http` correctly is
(`ait-llm/Cargo.toml:40-46`). These three are spec-mandatory surfaces
(§4.1 secrets, stories 10–11 reload, `--print-schema`), so gating buys
little — but `keyring` is the heaviest tree in the workspace and every
`ait` binary builds it even though the status line resolves via the
`NoSecrets` stub (`ait/src/config.rs:25-31`). Judgment call; flag for the
Round-5 dependency audit rather than a fix now.

### 7. Informational — e2e evidence dumps bypass the redaction discipline

`ait-testkit/src/evidence.rs` writes `screen.txt` + `pty-tail.bin` (raw
PTY bytes, `live.rs` `OUTPUT_TAIL_BOUND`) to `target/e2e-failures/`, which
CI uploads unredacted. Standard 5's redaction applies to LLM context
assembly (done correctly at `context.rs:133`), not test artifacts — but a
credential reaching a test PTY would land in CI artifacts verbatim.
`docs/e2e-driver-protocol.md` already forbids secrets in tests; worth a
note in `docs/testing-toolkit.md` that dumps are raw.

## Verified clean — the load-bearing standards

- **§3** per-schema IRs: separate `Request`/`StreamEvent` types per
  adapter; `Endpoint::stream` rejects cross-schema pairing
  (`endpoint.rs:144-147`).
- **§4** secrets: only `api_key_env` names in TOML
  (`settings.rs:141-145`); `EndpointConfig`/`ResolvedModel` Debug impls
  redact (`endpoint.rs:61-71`, `resolve.rs:83-92`); resolution is
  env-then-keyring (`secrets.rs:47-60`).
- **§5** redaction inside context assembly, typed request verbatim
  (`context.rs:131-138`).
- **§6** empty-state guards: `active()`/`active_mut()` return `Option`,
  `close_active` double-guards and clamps (`tabs.rs:339-353`);
  `app.rs:346-353` adds a belt guard.
- **§7** keymap resolves before PTY input (`app.rs:445-548`); Ctrl+S/Q
  pass through (test `keymap.rs:430-444`).
- **§8** `tiny_http` behind `test-fixtures`; `ait-testkit` dev-only.
- **§9** `ait` holds no business logic beyond dispatch; model call in
  `ait-agent`.
- **§13** `ReplySanitizer` runs inside the suggester worker before the
  channel (`suggester.rs:329-348`); deltas typed into the pinned tab
  under one bracketed-paste envelope (`suggest.rs:519-530`); Enter
  swallowed on the pinned tab, Esc cancels with `Ctrl+U` wipe
  (`app.rs:570-585`).
- **§14** `SuggestionPin` pins `Tab::id` and re-resolves position per use
  (`suggest.rs:342-348`, `635-637`); `TabEvent.tab` is positional but
  documented "stable only within the drain" (`tabs.rs:213-219`) and used
  synchronously — compliant carve-out. ADR-0002 pin (`=0.26.0`),
  alacritty types absent from public API, dep-comment lint passes.
