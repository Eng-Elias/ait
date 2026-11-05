# ISSUE #39: Spec R04: Natural language to command

Round 4 of REQUIREMENTS.md §7 — the MVP-closing round. Covers **§5.4 (Natural language → command)** and the **minimal credential-redaction slice of §5.10.3**. Everything else in §7 belongs to other rounds.

## Problem Statement

The user can configure an endpoint (Round 3) but has no way to use it from the terminal: to turn intent into a shell command they must leave `ait`, ask a model elsewhere, and copy the result back. The provider stack, the config model and the PTY insertion machinery all exist — nothing connects them, and nothing stops terminal secrets from leaking into a prompt if it did.

## Solution

The new `ait-agent` crate (§3.1, ADR-0003) owns the feature: a channel-based `CommandSuggester` that assembles context, redacts it, calls the resolved `command_model` endpoint, and streams command text back. The `ait` frontend contributes only detection and insertion: typing `# <request>` and pressing Enter — or pressing `Alt+Enter` on any input line — fires a request whose reply streams live into the PTY input line like typing, sanitized so it can never self-execute. The user reviews the typed command and presses Enter themselves; Esc cancels the stream and wipes the partial text. Terminal output lines sent as context pass through one compiled credential-regex set first; anything that fails — model unconfigured, endpoint down — degrades to a status-line toast while the terminal keeps working (§6).

## User Stories

1. As a user, I want to type `# list files larger than 1GB` and press Enter, so that a natural-language request becomes a shell command without leaving the terminal.
2. As a user, I want my `# …` line to be submitted to the shell as a normal comment, so that it lands in my shell history and costs nothing when the request fails.
3. As a user, I want the suggestion to type itself into my input line as it streams, so that I watch the command form instead of waiting for a finished blob.
4. As a user, I want the suggestion inserted **only** into my input line — never executed — so that a generated command can never run without me pressing Enter (§5.4).
5. As a user, I want a multi-line or markdown-fenced model reply collapsed to a single command line, so that a chatty endpoint cannot inject a newline that would execute mid-stream.
6. As a user, I want leading `$ ` and code fences stripped from the suggestion, so that pasted-style replies arrive as runnable commands.
7. As a user, I want the streamed text wrapped in one bracketed-paste unit when my shell enabled mode 2004, so that the suggestion is one undoable, non-executing insertion (§5.2).
8. As a user, I want Esc during the stream to cancel the request and wipe the partial command, so that I can abort a bad suggestion instantly (§5.4).
9. As a user, I want Enter ignored while a suggestion is still streaming, so that I cannot accidentally run a half-written command.
10. As a user, I want Esc and Enter to behave normally whenever no suggestion is in flight, so that the feature never changes my terminal's feel.
11. As a user, I want to press `Alt+Enter` on an already-typed line like `how do I undo a commit`, so that any input line — not just `# ` lines — can become a request.
12. As a user, I want the `Alt+Enter` path to clear my line before the suggestion streams in, so that the request text and the command never interleave.
13. As a user, I want a request triggered by accident (history recall, heredoc, REPL input that merely looks like `# …`) to not fire, so that only a real shell-echoed request line triggers the model.
14. As a user, I want the request pinned to the tab I typed it in, so that switching tabs mid-stream still types the command into the right session.
15. As a user, I want the model to see my current working directory, so that "list files here" means here (§5.4).
16. As a user, I want the model to see my recent terminal output, so that requests like "retry that with sudo" have context (§5.4).
17. As a user, I want the model told my shell and OS, so that suggestions are `bash`-on-Linux commands, not PowerShell (§5.4).
18. As a user, I want API keys, tokens, JWTs and private keys in my scrollback replaced with `[REDACTED]` before anything reaches the provider, so that secrets on screen cannot leak into a prompt (§5.4, §5.10.3).
19. As a user, I want my typed request itself sent verbatim, so that redaction never mangles what I actually asked.
20. As a user, I want `command_model` used for these requests when I configured it, so that I can point a cheap/fast model at command generation while `base_model` stays for heavier work (§4.2).
21. As a user, I want `command_model` to fall back to `base_model` when unset, so that one configured model is enough (§4.2).
22. As a user, I want a clear toast when no model is configured or the endpoint fails, so that a failed request never wedges or crashes my terminal (§6).
23. As a user, I want requests to keep working after a config hot-reload, so that swapping `command_model` in `settings.toml` takes effect on the next request.
24. As a developer, I want the feature's logic in a new `ait-agent` crate per ADR-0003, so that `ait` holds only detection and insertion glue (CODING_STANDARDS §9).
25. As a developer, I want `ait-agent` to expose a synchronous channel API while owning its tokio runtime on a worker thread, so that `ait`'s synchronous loop stays tokio-free (ADR-0004, Spec R03 precedent).
26. As a developer, I want redaction exposed as a pure `&str → String` seam, so that every pattern family is testable without a PTY, model or TTY (standard 11).
27. As a developer, I want context assembly as a pure function (cwd + shell + OS + recent lines + request → provider request), so that prompt shape and redaction ordering are unit-testable.
28. As a developer, I want model-reply sanitization as a pure function, so that the never-execute guarantee is enforced at a testable seam rather than by convention.
29. As a developer, I want trigger detection (input shadow vs grid echo agreement) unit-testable, so that desync cases are covered without a live shell.
30. As a maintainer, I want the crate-map status of `ait-agent` flipped in this round's PR, so that AGENTS.md and ADR-0003 stay truthful.

## Implementation Decisions

### `ait-agent` (new crate — ADR-0003 §3.1)

- **`CommandSuggester`** — channel-based API mirroring the config-watcher precedent: the frontend sends a request value on one `mpsc` channel and drains `Delta(String)` / `Done` / `Error(String)` events on another during the 8 ms tick. Internally it owns a tokio current-thread runtime on a worker thread; `tokio` never appears in `ait`'s dependency tree.
- **Endpoint call** — the suggester resolves `Settings::resolve_model("default", ModelRole::Command, &OsKeyring)` per request (so hot-reloaded settings apply immediately) and calls `Endpoint::stream(request, CancellationToken)`; `command_model` falls back to `base_model` per §4.2. Resolution or stream failure maps to an `Error` event → status toast.
- **Context assembly (pure)** — inputs: cwd, shell name, OS, the last N logical lines of terminal output, the typed request. Output: a single-turn provider request (system prompt stating "reply with one shell command, no markdown, no explanation" plus the assembled context). N is a fixed line-budget constant in `ait-agent`, tuned during implementation; blocks replace grid lines as the line source in Round 5 through this same seam.
- **Redaction (pure)** — one `RegexSet` compiled once covering the §5.10.3 **credential** patterns only: AWS `AKIA…`, GitHub (`ghp_`/`gho_`/`ghu_`/`ghs_`/`github_pat_`), OpenAI `sk-…`, Anthropic `sk-ant-…`, Stripe `sk_live_`/`sk_test_`, Slack `xox…`/`xapp-`, JWTs, private-key PEM blocks → replaced with `[REDACTED]`. Runs at assembly time on the extracted output lines only — not continuously on the PTY stream. The typed request is never redacted. Optional PII patterns, user-added patterns and per-pattern toggles wait for Round 10's `[redaction]` table (ADR-0006 strict parsing already rejects a premature `[redaction]` block with a clean error).
- **Sanitization (pure)** — model text → command bytes: strip code fences and a leading `$ `, trim, then **truncate at the first `\n` or `\r`**. Cooked-mode line discipline makes a newline an execution — this is the hard guarantee behind user story 5.

### `ait` binary (detection + insertion glue only)

- **Input shadow** — `ait` already sees every keystroke before translating it to PTY bytes (`keymap` → `translate` → `send_bytes`); it keeps a per-tab shadow of the printable input since the last line submission.
- **`# ` trigger** — on Enter: if the shadow starts with `# ` **and** the grid's current logical line (cursor row walked up over `wrapped()` rows) ends with the shadow — i.e. the shell really echoed the request — the request fires. Enter is **forwarded normally**: `# …` is a comment no-op in bash/zsh/fish, lands in history, and the suggestion streams into the fresh prompt. A desynced shadow (history recall, heredoc, REPL) fails the `ends_with` check → no trigger.
- **`Alt+Enter` trigger** — new `AppAction` in the keymap meaning "treat the current input line as the request": same shadow+grid verification, then the line is erased via backspaces and the same request path runs. `Alt+Enter` is byte-distinguishable from Enter in both keymap tiers (`ESC CR` vs `CR`; ADR-0005) and unbound in default readline.
- **Streaming insertion** — each sanitized delta goes to `LiveSession::send_bytes`; when `Session::bracketed_paste()` is true the whole stream rides inside one `\x1b[200~`…`\x1b[201~` envelope (`encode_paste` precedent). No newline byte is ever written, so nothing can execute — the "never runs without Enter" guarantee is structural, not policed.
- **In-flight keys** — Esc → `CancellationToken` + close the paste envelope + `Ctrl+U` to wipe the partial line; Enter → swallowed. Both pass through normally when no request is in flight. The request is pinned to the originating tab's session.
- **cwd** — `Tab::cwd()` (OSC 7 with spawn-cwd fallback); when OSC 7 is silent, `/proc/<pid>/cwd` via `LiveSession::child_pid()` covers vanilla shells.
- **Failure surface** — every failure mode (unresolvable model, transport error, cancelled stream) is a status-line `Toast`; nothing changes about session lifecycle (§6 degrade-gracefully).

## Testing Decisions

Good tests assert external behavior at a seam, not internals — same rule as Rounds 1–3 (standard 11: testable without TTY or network).

- **Redaction**: table-driven unit tests on `redact` — one positive and one non-secret pass-through case per pattern family, plus a multi-secret line and a PEM block spanning lines. Prior art: `ait-config`'s table-driven parse tests.
- **Sanitization**: table-driven cases — fenced reply, `$`-prefixed, multi-line (assert first-line truncation), whitespace-padded, already-clean. Pure seam.
- **Context assembly**: given fixed (cwd, shell, OS, lines, request) assert the assembled request shape and that output lines — but not the request — arrive redacted. Pure seam.
- **`CommandSuggester`**: integration test against the in-process SSE fixture server from `ait-llm`'s contract tests — request in, assert ordered deltas then `Done`; cancel mid-stream asserts prompt termination; fixture error maps to `Error`. No external network (§6).
- **Trigger detection**: unit tests over shadow+grid pairs — agreeing `# ` line fires; history-recalled/heredoc/REPL desyncs do not; `Alt+Enter` path fires on a bare line after verification.
- **e2e** (`crates/ait/tests/e2e.rs`, code-first via `SpawnConfig::env` — the known scenario-interpolation gap, backlog #37): a minimal in-process stub speaking one endpoint schema's SSE on `127.0.0.1:<ephemeral>` plus a fixture `settings.toml`; assert `# …` + Enter streams the command onto the input line, `Alt+Enter` works on a bare line, Esc wipes mid-stream, and an unconfigured model toasts. Prior art: the Round-3 config e2e tests.
- **Manual smoke (not CI)**: a real-endpoint run by the maintainer before the PR — type a `# ` request, watch it stream, execute the result.

## Out of Scope

- Everything else in §7's Round-4 row already landed in Rounds 1–3. For this spec, out of scope means:
- **Round 5**: shell integration, blocks, OSC 133/7700 — context stays "last N grid lines" until then.
- **Round 6**: agent mode, tool calls, `write_to_pty`, agent-owned PTYs.
- **Round 7**: permission engine, non-`default` profiles, `context_window_limit` enforcement.
- **Round 10**: the `[redaction]` settings table, optional PII patterns, user-added patterns, per-pattern disables; MCP, skills.
- **Round 11**: the input classifier (auto-detecting NL without a trigger), headless mode, `ait ctl`.
- **Round 12**: packaging.
- As-you-type suggestions, a dedicated ait-owned request editor, and multi-line generated commands — all rejected during grilling as non-MVP.
- GUI, telemetry, accounts — §11.

## Further Notes

- `ait-agent` lands a crate early (ADR-0003 assigns it Round 6); AGENTS.md's crate map and ADR-0003's table flip in the same PR per the workflow rule.
- The feature deliberately has no new settings surface this round — trigger (`# `), keybinding (`Alt+Enter`) and context budget are fixed; configurability decisions ride with the rounds that need them.
- Possible ADR: the `# `+Enter forwarding trick (comment no-op instead of erase-and-swallow) is a user-visible, hard-ish-to-reverse interaction decision — decide at ticket time whether it warrants one, same bar as Rounds 2–3.

