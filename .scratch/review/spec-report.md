# SPEC-axis review — MVP diff (Rounds 1–4)

Base `696048d` → tip `888c41c`. Sources: Spec issues #3 (R01), #11 (R02), #27 (R03), #39 (R04); REQUIREMENTS.md §4, §5.1–5.4, §5.10.3, §7; ADRs 0002–0007.

**Verdict:** coverage is exceptionally high — nearly every user story and implementation decision in all four specs is present and correctly placed. Findings are few; the top one is a real secret-leak bug.

---

## Findings (by severity)

### 1. PEM private-key redaction can never fire in production — **high, implementation wrong**

Spec R04, Implementation Decisions → Redaction: *"private-key PEM blocks → replaced with `[REDACTED]`"*, and the testing decisions require *"a PEM block spanning lines"* as a case.

`ait-agent/src/redact.rs:44` defines the multi-line PEM pattern (`-----BEGIN …-----[\s\S]*?-----END …-----`), and `tests/redact.rs:135` proves it works — **on multi-line input**. But the only production caller, `ait-agent/src/context.rs:133`, runs `redact(line)` **per logical grid line**:

```rust
for line in tail {
    content.push_str(&redact(line));
    content.push('\n');
}
```

A PEM block in scrollback arrives as one `output_lines` entry per grid row; no single line contains both `BEGIN` and `END`, so the pattern never matches. `cat ~/.ssh/id_rsa` then `# what's on screen` sends the full private key to the provider. Fix: join the tail lines, redact once, then split — or redact the assembled `content` block. This is the one credential family in the §5.10.3 minimal set that silently doesn't work.

### 2. Child-application OSC 52 opt-in never landed — medium, spec drift / unreachable feature

Spec R02, Clipboard direction split: *"OSC 52 requests from child applications stay off — the `SessionOptions::osc52` default — **until Round 3 config exposes the opt-in** (§5.1). The `ClipboardWrite` event is wired so flipping it later is one line."*

Round 3 shipped no such key: `[terminal]` has only `shell`/`scrollback_lines` (`ait-config/src/settings.rs:106-114`), `deny_unknown_fields` rejects an `osc52` key, and `ait/src/config.rs:88-97` never sets `options.session.osc52`. Result: `SessionOptions::osc52` is `true` only in the testkit harness and `term-demo` — in the real app the `ClipboardWrite` event is dead code, and `app.rs:324-328`'s comment *"when Round 3's config flips the option, honor the request…"* is stale. The plumbing spec asked for exists; the user-facing opt-in R02 promised for Round 3 does not. Judgement call whether R03's spec intentionally dropped it — worth an explicit decision/backlog ticket either way.

### 3. OSC 133 prompt marks are emitted but never recorded — medium-low, partial

Spec R01 story 14: *"I want `OSC 133` A/B/C/D prompt marks recorded in the grid, so that Round 5 can assemble blocks without re-parsing output."*

`ait-term` emits `SessionEvent::PromptMark` (`events.rs:25`, tested in `tests/events.rs`), but nothing retains marks: `TabManager::drain_events` (`tabs.rs:467-472`) tracks only `CwdChanged`/`Exited`, and `app.rs` drops the rest into `_ => {}`. The spec's implementation decision (*"marks recorded on grid lines/events this round"*) arguably permits events-only, but the story's stated purpose — Round 5 assembling blocks *without re-parsing* — needs the marks persisted somewhere (grid lines or per-tab state). As delivered they are observable-and-discarded. Judgement call; likely just deferred plumbing, but flag it before Round 5.

### 4. Missing `default` profile makes every request fail rather than resolve — low, judgement call

Spec R03 story 23: *"I want unset roles to fall back to `base_model`, so that one configured model is enough to use everything."*

`Settings::resolve_model` (`ait-config/src/resolve.rs:112-117`) returns `Err("unknown execution profile")` when `[agents.execution_profiles.default]` is absent. A config declaring endpoints+models but no profile section — plausible for a hand-written file — makes every `# `/Alt+Enter request toast an error rather than falling back to anything. The error is clear and toasts correctly (R04 us-22 is satisfied), so this is arguably correct strictness — but a reasonable reading of "one configured model is enough" might expect an implicit default profile. Judgement call.

### 5. Minor items

- **PGP PEM variant missed** (`redact.rs:44`): `-----BEGIN PGP PRIVATE KEY BLOCK-----` ends in `KEY BLOCK`, not `PRIVATE KEY`, so it doesn't match the `-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----` anchor even if finding 1 is fixed. Spec says "private-key PEM blocks" generally.
- **`openai_chat_completions` lacks an error-status fixture**: spec R03 wants *"one recorded SSE transcript per schema per interaction shape (…error status incl. 429 with `Retry-After`)"*. `tests/fixtures/` has error events for anthropic and openai_responses only; shared `contract_http.rs` does cover 429/5xx/4xx classification once, so coverage exists but not per-schema per the letter.
- **`RegexSet` vs `Vec<Regex>`**: spec R04 says *"one `RegexSet` compiled once"*; `redact.rs` keeps per-pattern `Regex`es because `replace_all` needs spans. Intent preserved, letter deviated — fine.
- **`feed_bytes` event ordering** (`session.rs:164-174`): emulator events (OSC 52) are grouped before observer events within one feed, so a stream `OSC7 … OSC52` reports ClipboardWrite before CwdChanged. Documented in the doc-comment; harmless since nothing consumes both.
- **Shadow pessimism after paste+submit**: a `Shift+Insert` paste containing a newline marks the shadow stale until the *next* submission clears it, so the first `# ` line typed afterwards submits without firing. Conservative per the spec's desync rules ("no trigger, never a wrong trigger") — acceptable.

## Scope creep (b)

Essentially none. No `ait-shell` crate, rc snippets, block assembly, hook-protocol decode, tool registry, agent loop, permission engine, `[redaction]`/`[ssh]` tables, persistence, MCP, classifier, or packaging — all correctly deferred. The §5.7 execution-profile model is parsed-but-inert exactly as spec'd. Small un-asked-for extras, all in-spirit: `--help` text; last-tab exit status propagating to the process exit code (`app.rs:838`); `N:title` attribution on background-tab toasts (`status.rs:97`); control-byte stripping in `sanitize_reply` beyond the spec's truncate-at-newline (a hardening fix, desirable); `output_tail`/`set_output_mirror` (Spec E01 test support); the startup-probe watchdog exceeding "opportunistic only" — explicitly settled by ADR-0005.

## Verified good (spot-check summary)

R01: `$SHELL`→`/bin/sh` resolution, bounded 512 KiB output channel backpressure, Starting/Running/Exited(status), OSC 7/9/52(opt-in)/133/777 + `Private` passthrough incl. 7700, mode-aware key encoding, read-side `GridView` types with private alacritty types, byte-fed `feed_bytes` seam, `term-demo`.
R02: keymap-before-PTY with legacy-collapse guard + enhanced-only Ctrl+Shift aliases, probe watchdog (ADR-0005), tabs w/ OSC-7 cwd inheritance, `exited <code>` state, last-tab exit, status line `cwd · shell · model · state`, ~3 s toasts, wheel 3-line/tick + pin + snap-on-keypress, drag-select→host OSC 52 copy (wrapped joins, wide spacers skipped), Shift+Insert paste w/ mode-2004 bracketed envelope, resize→all tabs, panic hook + RAII restore, input-first 8 ms loop, scripted demo incl. tmux (`scripts/demo-tui.sh`).
R03: `from_toml` pure seam w/ `serde_path_to_error` key paths, `deny_unknown_fields`, dup `config_key` + dangling-role validation, env→keyring secrets (`ait/<key>`), missing-file defaults + invalid-file toast, 300 ms debounced watcher (parent-dir, atomic-save safe), last-good policy + `[terminal]`-new-sessions-only (ADR-0006), `--init`/`--force`/`--print-schema` w/ fresh UUID template, all three adapters w/ per-schema IRs, ToolCallDelta/Finish/Error events, Bearer vs `x-api-key`+`anthropic-version` auth (omitted when no secret), 3-attempt 250 ms×2ⁿ+jitter retry honoring `Retry-After` (10 s cap), 10 s connect/60 s idle/no deadline, cancellation incl. mid-backoff, per-schema contract tests + `chat` example.
R04: shadow+grid echo-agreement triggers (`# ` + Alt+Enter), ADR-0007 forward-as-comment, per-request `command_model`→`base_model` resolution w/ hot-reload pickup, context assembly (cwd via OSC7/`/proc`, shell, OS, last-100 redacted lines, verbatim request), streaming `Delta`s inside one 2004 envelope pinned to stable tab id, Esc→token+PASTE_END+Ctrl+U, Enter swallowed in flight, all failures → toast, tokio confined to `ait-agent`'s worker (ADR-0004 amendment), e2e coverage via in-process SSE stub.
