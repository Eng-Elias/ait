# ISSUE #27: Spec R03: Configuration and providers

Round 3 of REQUIREMENTS.md §7. Covers **§4 (Configuration model)** and **§5.3 (Configuration & providers)**.

## Problem Statement

The user has no way to tell `ait` which LLM endpoints and models to use, and there is no code that can talk to one. The terminal runs, but the model slot in the status line is a `—` placeholder, no `settings.toml` exists, and no HTTP/SSE machinery is in place. Round 4 (NL→command) needs a validated configuration and a streaming provider layer to call.

## Solution

Two new crates land per the ADR-0003 layout:

- `ait-config` loads, validates and hot-reloads `~/.config/ait/settings.toml` (respecting `$XDG_CONFIG_HOME`), resolves secrets from the environment or OS keyring, emits a JSON Schema of the settings model, and hands the rest of the app a typed `Settings` value with errors that name the offending key.
- `ait-llm` provides one `LlmProvider` trait with three endpoint-schema adapters — `openai_chat_completions`, `openai_responses`, `anthropic_messages` — each keeping its own message IR and streaming tool calls over SSE, with timeouts, cancellation and bounded retry on 429/5xx.

The `ait` binary gains `--init` (writes a commented default `settings.toml`) and `--print-schema` (emits the JSON Schema), consumes `[terminal]` shell/scrollback settings for new sessions, and shows the resolved `base_model` alias in the status line.

## User Stories

1. As a user, I want `ait` to read `~/.config/ait/settings.toml` (or `$XDG_CONFIG_HOME/ait/settings.toml`), so that my configuration lives in the conventional place.
2. As a user, I want `ait --init` to write a commented default `settings.toml` I can edit, so that I never have to write config from scratch.
3. As a user, I want `--init` to refuse to overwrite an existing `settings.toml` unless I pass a force flag, so that I cannot accidentally destroy my configuration.
4. As a user, I want `--init` to generate a real UUID for the example model's `config_key`, so that the emitted template is immediately valid.
5. As a user, I want `ait --print-schema` to print the JSON Schema of the settings model, so that my editor can validate and autocomplete `settings.toml`.
6. As a user, I want a missing `settings.toml` to silently produce built-in defaults on first run, so that `ait` works before I configure anything.
7. As a user, I want an invalid `settings.toml` to produce an error naming the offending key path, so that I can fix it without guessing.
8. As a user, I want the terminal to still start with defaults when my config is invalid, showing the error on the status line, so that a typo never costs me my terminal (§6 reliability).
9. As a user, I want unknown keys rejected with a clear error, so that typos in key names cannot pass silently.
10. As a user, I want edits to `settings.toml` picked up while `ait` is running, so that I can switch models without restarting.
11. As a user, I want a broken edit during hot-reload to keep my last good configuration and show the error, so that an in-flight save never degrades my session.
12. As a user, I want `[terminal] shell` to override which program new sessions spawn, so that I can run a different shell than `$SHELL`.
13. As a user, I want `[terminal] scrollback_lines` to size the scrollback of new sessions, so that I control memory vs history.
14. As a user, I want existing sessions to keep their spawn-time shell and scrollback after a reload, so that a config change cannot disturb a live session.
15. As a user, I want to declare named endpoints under `[agents.custom_endpoints.<key>]` with `name`, `base_url` and `schema`, so that any OpenAI- or Anthropic-compatible server works — local or hosted.
16. As a user, I want each endpoint to list models with a wire `name`, a display `alias`, and a stable `config_key`, so that I can rename a model without breaking references.
17. As a user, I want duplicate `config_key`s rejected at validation time, so that model-role references are never ambiguous.
18. As a user, I want a model role referencing an unknown `config_key` rejected with an error naming the role, so that misconfiguration surfaces immediately.
19. As a user, I want secrets to come only from the env var named by `api_key_env` or the OS keyring entry `ait/<endpoint-key>`, never from the TOML file, so that secrets stay out of config and logs.
20. As a user, I want the env var checked before the keyring, so that a per-shell override wins over the stored secret.
21. As a user, I want endpoints without a key (e.g. local Ollama) to send no `Authorization` header at all, so that `api_key_env` can be omitted.
22. As a user, I want model roles (`base_model`, `coding_model`, `command_model`) on the `default` execution profile to resolve to an endpoint + model + secret, so that later rounds can pick a model per purpose.
23. As a user, I want unset roles to fall back to `base_model`, so that one configured model is enough to use everything.
24. As a user, I want the status line to show the resolved `base_model`'s alias (falling back to its wire name, `—` when unset), so that I always see which model is active.
25. As a user, I want `context_window_limit` on a profile and an optional `context_window` on a model entry, so that later rounds can bound context; `0` means the model's own limit.
26. As a developer, I want one `LlmProvider` trait implemented by all three endpoint schemas, so that consumers code against one abstraction.
27. As a developer, I want each adapter to keep its own message IR — request, message, tool-call and stream-event types — so that schema differences are preserved rather than unified prematurely (§5.3, §8.3).
28. As a developer, I want chat responses streamed as typed events (text deltas, tool-call deltas, finish, error) per schema, so that consumers can render incrementally.
29. As a developer, I want streamed tool calls surfaced incrementally — id, name and argument fragments — so that agent rounds can consume them (§5.3).
30. As a developer, I want every adapter to take a cancellation token and to stop promptly when cancelled, so that Esc can abort an in-flight request once the TUI issues requests.
31. As a developer, I want bounded retry with exponential backoff and jitter on 429 and 5xx and transport errors — honoring `Retry-After` — so that transient failures heal themselves.
32. As a developer, I want other 4xx responses (401/403/404/…) to fail immediately without retry, so that a bad key or model name surfaces fast.
33. As a developer, I want a connect timeout and a stream idle-timeout but no whole-request deadline, so that long generations are not killed while a wedged connection still is.
34. As a developer, I want contract tests per schema replaying recorded fixtures with zero external network, so that CI is deterministic.
35. As a developer, I want a runnable example (`cargo run -p ait-llm --example chat`) that loads `settings.toml`, resolves a model and streams a reply to stdout, so that the provider stack can be smoke-tested by hand before Round 4 wires it into the TUI.
36. As a user, I want `ait` to keep working — terminal, tabs, scrollback — when every endpoint is unreachable, so that LLM trouble never breaks the terminal (§6).
37. As a maintainer, I want the dependency policy honored — semver requirements noted with release dates, all releases ≥7 days old — so that the supply-chain rule (ADR-0002) keeps holding.

## Implementation Decisions

### `ait-config` (new crate)

- Pure parse seam: `Settings::from_toml(&str) -> Result<Settings, ConfigError>`; `ConfigError` carries a message whose TOML span is rendered as a key path (`agents.custom_endpoints.local.models[0].config_key: …`). The file layer (`load`, `watch`) stays thin over it.
- Serde model, `deny_unknown_fields` everywhere for typo-catching errors (grill Q-rec: strictness serves §5.3's "errors point at the offending key"):
  - `[terminal]`: `shell: Option<String>` (program for new sessions, falling back to `$SHELL` then `/bin/sh` per §5.1), `scrollback_lines: Option<usize>`.
  - `[agents.custom_endpoints.<key>]`: `name`, `base_url`, `schema` (`openai_chat_completions | openai_responses | anthropic_messages`), `api_key_env: Option<String>`, `[[models]]` with `name`, `alias`, `config_key`, optional `context_window`.
  - `[agents.execution_profiles.<name>]`: the **full §5.7 shape** — the four permission levels per tool, `command_allowlist`, `command_denylist`, `directory_allowlist`, `mcp_allowlist`, `mcp_denylist`, the three model roles, `context_window_limit` — parsed and validated but inert (no permission engine consumes it until Round 7). Modeling it now keeps the emitted JSON Schema stable.
- `config_key` is validated as a non-empty string unique across all endpoints' models; UUID format is not enforced — `--init` emits a fresh v4 UUID but hand-written keys are allowed.
- Secret resolution: `api_key_env` env var first; if unset/empty, keyring lookup with service `"ait"` and account `"<endpoint-key>"` via the `keyring` crate. No ait-side secret setter — users populate the keyring with platform tools. Missing secret is **not** a validation error (local endpoints have none); it surfaces at request time.
- Hot reload: `notify` watcher on the settings file, debounced ~300 ms, pushing `Result<Settings, ConfigError>` events on a channel. Consumer policy: keep last-good on error. `[terminal]` fields apply to new sessions only; everything else applies live.
- `schemars` derives on the settings model produce the JSON Schema string that `--print-schema` prints; `ait-config` also owns the commented template text that `--init` writes.

### `ait-llm` (new crate)

- Async on `tokio` + `reqwest` + `eventsource-stream` (§2 decided stack). `tokio` is a dependency of `ait-llm`, not of `ait` — the TUI's synchronous loop (Spec R02) is untouched this round; the example binary runs its own current-thread runtime.
- `LlmProvider` trait with associated `Request` and `StreamEvent` types; a top-level `Endpoint` enum wraps the three adapters so consumers get one entry point and can `match` on schema — the IRs stay distinct per §5.3's "do not unify prematurely".
- Per-schema IRs model messages (system/user/assistant/tool), tool calls (id, name, JSON arguments), and stream events (text delta, tool-call delta, finish with stop reason + usage where the schema reports it, error).
- HTTP details: `Authorization: Bearer <key>` for the OpenAI schemas, `x-api-key` + `anthropic-version` for Anthropic; auth headers omitted entirely when the endpoint has no secret.
- Retry: at most 3 attempts total; exponential backoff 250 ms × 2ⁿ with jitter; `Retry-After` honored, capped at 10 s; retried on 429, 5xx and transport errors; all other failures are terminal. Cancellation (`tokio_util::sync::CancellationToken`) aborts immediately, including mid-backoff.
- Timeouts: 10 s connect timeout; 60 s idle timeout between stream events; no total deadline (long generations must not be killed).
- Error type via `thiserror` distinguishing: transport, HTTP status (with body excerpt), SSE/parse, and cancelled — so callers can decide what is retryable without re-inspecting internals.
- Smoke vehicle: `examples/chat.rs` — loads `settings.toml` via `ait-config`, resolves a role (flag-selectable, default `base_model`), sends a prompt, prints streamed deltas. This is the manual smoke-test vehicle; the real endpoint to exercise is chosen by the maintainer (named during grilling).

### `ait` binary (frontend wiring only)

- Startup: `ait-config::load` → on success apply `[terminal]` fields when spawning sessions; on error run with defaults and toast the `ConfigError` message.
- Watcher events feed the existing app loop: success swaps the live `Settings` (status line re-renders with the new model); failure toasts the error and keeps last-good.
- Status-line model slot: resolved `base_model` alias → wire name → `—`.
- CLI: `--init` (errors if the file exists, `--force` overrides), `--print-schema`; both are thin shells over `ait-config` output — no business logic in the binary (CODING_STANDARDS §9).

## Testing Decisions

Good tests assert external behavior at a seam, not internals — same rule as Rounds 1–2 (standard 11: testable without TTY or network).

- **ait-config parse/validate**: table-driven unit tests on `Settings::from_toml` — valid full/empty/minimal documents, unknown keys, duplicate `config_key`, dangling role references, bad enum values — asserting the error text names the key. Prior art: `ait-term`'s byte-fed unit tests (feed input at a pure seam, assert output).
- **Watcher**: integration test in a tempdir writing `settings.toml` variants and awaiting reloaded `Settings`/`ConfigError` on the watch channel. Real `notify`, real filesystem — no TTY, no network.
- **ait-llm contract tests**: one recorded SSE transcript per schema per interaction shape (text-only reply; streamed tool call; error status incl. 429 with `Retry-After`), replayed by an in-process HTTP server fixture (dev-dependency) that the adapter's `reqwest` client is pointed at — the real HTTP+SSE path is exercised without external network. Prior art: the repo's "no network in CI" requirement (§6); fixtures committed under the crate's `tests/` tree. Cancellation test: start a stream, cancel mid-flight, assert prompt termination. Retry test: 429 → 200 sequence asserting backoff was honored.
- **TUI wiring**: e2e via `ait-testkit` (Spec E01 seam): scenario spawning the real binary with `XDG_CONFIG_HOME` pointed at a temp `settings.toml`, asserting the status row shows the resolved model alias; a scenario for `[terminal]` shell/scrollback behavior where assertable. `--init`/`--print-schema` covered by CLI-level tests (assert output, no TTY needed).
- **Manual smoke (not CI)**: `ait-llm`'s `chat` example against the maintainer-chosen real endpoint, run by hand before the PR.

## Out of Scope

- Round 4+: any LLM call issued by the TUI (`# ` → command), Esc-cancel wiring, secret redaction before context assembly, `[redaction]` settings.
- Round 7: permission enforcement, profile switching in the TUI, consuming any profile other than `default`'s model roles.
- `[ssh]` settings (Round 9), MCP/skills config (Round 10), persistence (Round 8).
- Configurable keybinding remapping — deferred until a dedicated decision; no dead schema field.
- An `ait` CLI for writing secrets to the keyring; packaging-level polish (`--help` man-page style, install scripts) stays Round 12.
- GUI, telemetry, accounts — §11.

## Further Notes

- Keeps `ait-term` untouched and synchronous (ADR-0004); `ait` stays synchronous — `tokio` enters the tree only under `ait-llm` + its example.
- ADR-0003's crate map is exercised for the first time beyond `ait-term`/`ait`; AGENTS.md's crate table gets its `exists` flags updated in the same PR.
- New vocabulary for GLOSSARY.md if terms crystallize: none expected — `Endpoint`, `Endpoint schema`, `Model role`, `Execution profile`, `config_key` already exist.
- An ADR is likely warranted for the hot-reload policy (last-good + toast, `[terminal]` = new sessions only) and possibly for strict unknown-key rejection — both hard-ish to reverse and trade-off-driven; decided at spec/ticket time.

