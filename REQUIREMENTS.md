# AIT — Project Requirements

AIT (AI Terminal) is a local-first, provider-agnostic, SSH-native AI terminal
written in Rust. This document is the single source of truth for what we are
building. Every round of work starts from a section of this file.

How to read it:

- Section numbers (§5.3, §8 …) are stable. Prompts, specs and tickets cite them.
- **Decided** items are requirements. **Proposed** items are defaults that the
  round's grilling session may confirm or change — record the outcome as an ADR.
- When this file and a later ADR disagree, the ADR wins. Update this file in the
  same PR that adds the ADR.

---

## 1. Vision

A terminal you can run anywhere — locally, inside tmux, or on a remote host over
SSH — that understands natural language, can act as a tool-using agent inside
your real shell, and talks to **any** LLM endpoint you configure. No account, no
cloud dependency, no vendor lock-in.

### 1.1 Product principles (Decided)

1. **Local-first.** Works fully offline except for the LLM calls you configure.
   No telemetry, no sign-in.
2. **Provider-agnostic.** Any OpenAI-compatible or Anthropic-compatible endpoint,
   including local servers (Ollama, LM Studio, vLLM, llama.cpp).
3. **SSH-native.** The same features work on remote shells. A TUI is SSH-native
   by construction: run `ait` on the remote host and everything works.
4. **TUI first.** The TUI ships v1; a GUI frontend follows in Release 2
   (§5.12), built on the same core. The agent core never depends on a
   frontend.
5. **The user stays in control.** Every agent action goes through a permission
   profile (§5.7). No global "yolo" switch.
6. **Degrade gracefully.** Missing shell integration, a dumb remote shell, or an
   unreachable LLM must never break the terminal itself.

### 1.2 Target users & core use cases

- Developers and operators who live in the terminal and want AI help without
  leaving it.
- "Turn this sentence into a command" (§5.4).
- "Do this multi-step task in my shell, asking me before anything risky" (§5.6).
- "Same thing, but on that server I'm SSH'd into" (§5.9).

---

## 2. Technology stack (Decided unless marked)

| Layer | Choice | Notes |
|---|---|---|
| Language | Rust (stable, edition 2021+) | Cargo workspace |
| UI | `ratatui` + `crossterm` | TUI frontend |
| GUI | TBD in Round 13 (Proposed: GPU-accelerated Rust stack, e.g. `wgpu`-based; fallbacks: `egui`, webview) | Release 2, §5.12; separate crate + binary |
| PTY | `portable-pty` | Own the master side; spawn `$SHELL` |
| VT emulation | `alacritty_terminal` (grid + `vte` parser), exact-pinned | Do not hand-write a full emulator (ADR-0002) |
| Async | `tokio` | Lives in the library crates that need it (`ait-llm` HTTP/SSE, `ait-agent` worker), each behind a synchronous facade — the `ait` binary stays tokio-free (ADR-0004, amended 2026-10-28). The PTY reader runs on `std::thread` inside `ait-term`; the core stays synchronous |
| HTTP / streaming | `reqwest` + `eventsource-stream` (SSE) | Streaming for every provider schema |
| Config | `toml` + `serde` + `schemars` | `settings.toml` + emitted JSON Schema |
| Secrets | env vars + `keyring` crate | Never in `settings.toml` |
| Storage | `rusqlite` (`bundled`) | Conversations, blocks, plans (Round 8, ADR-0011) |
| MCP | `rmcp` | Round 10 |
| Search (later) | `tantivy` | Optional, post-v1 |
| Errors / logs | `thiserror` in libs, `anyhow` in binaries, `tracing` | Logs to file, never to the TUI |

Heavyweight or platform-specific dependencies go behind cargo features.

---

## 3. Architecture

```
┌──────────────────────────────────── ait ────────────────────────────────────┐
│  TUI frontend (ratatui)                                                      │
│   ├─ terminal view (rendered from the session grid)   ├─ agent conversation  │
│   ├─ block list (OSC 133-aware)                        └─ permission prompts │
│                                                                              │
│  Session manager ──► PtySession { pty master, VT parser, grid, blocks }      │
│                         ▲                     ▲                              │
│              CommandExecutor trait ───────────┘                              │
│               ├─ LocalPtyExecutor                                            │
│               ├─ InBandExecutor        (writes the command into the PTY)     │
│               ├─ ControlMasterExecutor (ssh -S <socket> host -- cmd)         │
│               └─ RemoteDaemonExecutor  (optional helper on host, later)      │
│                                                                              │
│  Agent core (tokio, frontend-agnostic)                                       │
│   ├─ LlmProvider trait: openai_chat_completions | openai_responses |         │
│   │                     anthropic_messages                                   │
│   ├─ Tool registry: run_command, read_files, edit_files, write_to_pty,       │
│   │                 grep/glob, ask_user_question, update_plan, mcp_*, skills │
│   ├─ Permission engine (execution profiles)                                  │
│   ├─ Secret redaction filter (before any context assembly)                   │
│   └─ Conversation store + AGENTS.md/rules + skills loader                    │
└──────────────────────────────────────────────────────────────────────────────┘
```

A second frontend — the `ait-gui` desktop binary (§5.12, Release 2) — joins
the TUI on the same core; both are thin frontends holding no business logic.

### 3.1 Crate layout (Decided, ADR-0003)

| Crate | Responsibility |
|---|---|
| `ait-term` | PTY spawn, VT parsing, grid + scrollback, session lifecycle, OSC interception |
| `ait-config` | `settings.toml` model, JSON Schema emission, secrets lookup |
| `ait-llm` | Provider trait, per-schema message IR, streaming adapters |
| `ait-shell` | Shell-integration scripts (bash/zsh/fish) and the hook/exec protocol |
| `ait-agent` | Tool registry, agent loop, permission engine, redaction |
| `ait` | TUI binary (frontend only — no business logic) |
| `ait-store` | SQLite store: conversations, transcript items, plans, terminal sessions, block metadata |
| `ait-gui` | GUI frontend binary (Round 13, §5.12) — planned |

The layout was confirmed in Round 1 (ADR-0003) and may grow (e.g. `ait-ssh`);
`ait-store` landed in Round 8 (ADR-0011).

---

## 4. Configuration model

Location: `~/.config/ait/settings.toml` (respect `$XDG_CONFIG_HOME`).
`ait --init` writes a commented default file. `ait --print-schema` prints the
JSON Schema generated from the Rust types.

### 4.1 Custom endpoints (Decided)

```toml
[agents.custom_endpoints.local]
name     = "Local Ollama"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"   # | "openai_responses" | "anthropic_messages"
api_key_env = "OLLAMA_API_KEY"         # optional; or use the keyring entry "ait/<endpoint-key>"

[[agents.custom_endpoints.local.models]]
name       = "qwen2.5-coder:14b"       # model id sent on the wire
alias      = "Qwen Coder"              # display name
config_key = "6f1c…-uuid4"             # stable id referenced by model roles
```

- Non-secret endpoint config lives in `settings.toml`. Secrets come only from the
  environment variable named by `api_key_env` or from the OS keyring.
- `config_key` is a stable UUID so renaming a model never breaks references.

### 4.2 Model roles (Decided)

Model roles are separate from endpoints and are set per execution profile:
`base_model`, `coding_model`, `command_model` (NL→command), `context_window_limit`.
Each role references a model `config_key`. Unset roles fall back to `base_model`.

### 4.3 Other settings

- `[terminal]` shell path override, scrollback lines, `osc52` child-app clipboard opt-in, keybindings.
- `[ssh]` `enable_integration`, `hosts_allowlist`, `hosts_denylist`, `reuse_existing_control_master` (Round 9, §5.9).
- `[redaction]` `extra` user regexes, `enable`/`disable` per named pattern — the §5.10.3 built-ins plus opt-in PII categories (Round 10).
- `[input]` `classifier` — `heuristic` (default) or `off`, the §5.10.4 input classifier's arm switch (Round 11).

---

## 5. Functional requirements

### 5.1 Terminal core (Round 1)

- Spawn the user's `$SHELL` (fallback `/bin/sh`) in a PTY; forward resize events.
- Parse output into a styled cell grid with scrollback (configurable length).
- Track session lifecycle: starting, running, exited (with exit status).
- Intercept OSC sequences without breaking normal rendering:
  - `OSC 7` → current working directory.
  - `OSC 133` A/B/C/D → prompt/command boundaries (consumed in §5.5).
  - `OSC 52` → clipboard write (opt-in).
  - `OSC 9` / `OSC 777` → desktop notifications (surface as a TUI toast).
- Frontend-neutral key model: frontends translate their key events into a
  single `Key` type; the core encodes them into PTY bytes.
- Public API is testable without a real terminal (feed bytes in, assert grid).

### 5.2 TUI shell (Round 2)

- `ratatui` + `crossterm` rendering of the session grid, including colors,
  bold/italic/underline, cursor shape, and wide characters.
- Input routing: app keybindings are intercepted **before** PTY input
  (e.g. Ctrl+S must never reach the PTY as XOFF when bound).
- Tabs (multiple sessions), a status line (cwd, shell, model, session state),
  and a clean exit that restores the host terminal even on panic.
- Mouse: scroll through scrollback; text selection + copy.
- Runs correctly inside tmux and over SSH.

### 5.3 Configuration & providers (Round 3)

- Load, validate and hot-reload `settings.toml` (§4); clear error messages that
  point at the offending key.
- One `LlmProvider` trait with three adapters:
  `openai_chat_completions`, `openai_responses`, `anthropic_messages`.
- Streaming (SSE) for all three, including streamed tool calls.
- Keep a **per-schema message IR**. Tool-call serialization and streaming events
  differ between schemas; do not unify them prematurely.
- Timeouts, cancellation (Esc cancels an in-flight request), retry with backoff
  on 429/5xx.
- Contract tests against recorded fixtures for each schema; no network in CI.

### 5.4 Natural language → command (Round 4, MVP)

- Trigger: a line starting with `# ` in the input, or a keybinding.
- Context assembly: cwd (from OSC 7), the last N lines/blocks of output, `$SHELL`,
  OS, and the typed request.
- **Secret redaction runs on all terminal output before context assembly**
  (built-in patterns from §5.10.3; minimal set required in this round).
- The generated command is inserted into the input line; it is **never** run
  without the user pressing Enter.
- Streaming preview of the suggestion; Esc cancels.

### 5.5 Shell integration & blocks (Round 5)

Blocks (one command + its output + exit code) come from shell hooks, not from
parsing output.

- Ship rc snippets for bash, zsh and fish. `ait` injects them on session start
  without editing the user's dotfiles (e.g. via `--rcfile`/`ZDOTDIR` wrappers).
- Snippets emit standard `OSC 133;A/B/C/D;<exit>` and `OSC 7`.
- Snippets also emit AIT hook events on a private channel (Decided,
  ADR-0008):

  ```
  ESC ] 7700 ; <hex(json)> BEL
  json = {"hook":"preexec","value":{"command":"...","session_id":N}}
  hooks: init_shell | bootstrapped | precmd | preexec | exit_shell
  ```

  Hex-encoding the JSON guarantees no payload byte collides with string
  terminators or control characters.
- Group grid output into blocks; show exit status and duration per block;
  allow selecting/copying a block.
- **Graceful degrade:** if integration is absent (unknown shell, `sudo -i`,
  dumb remote shell), the terminal still works — no blocks, no errors.

#### 5.5.1 In-band command execution (Decided, ADR-0008 — amended)

Lets `ait` run a command inside the user's live shell (with their env, aliases,
functions) and capture the result without a second channel:

```
ait → PTY input : __ait_run <id> <hex(command)>
shell → output  : ESC ] 7701 ; A BEL
                  ESC ] 7701 ; R ; <len> ; <hex> BEL          (repeated)
                  ESC ] 7701 ; B BEL
```

The `R` frames carry successive slices of one `<id>;<stdout+stderr>;
<exit_code>` payload — `<len>` is the decoded byte length of *that* frame, and
every complete OSC stays under ~1024 raw bytes (the `vte` parser cap). The
shell function runs the command in a capture subshell, so the user's prompt is
not disturbed. Responses are not rendered in the grid; the typed `__ait_run`
line stays visible.

#### 5.5.2 Shell-native completions (stretch — protocol reserved, not implemented)

Codes `7702` and `7703` are reserved by ADR-0008; nothing emits or consumes
them yet. The sketch below is still proposal-grade:

`__ait_complete <hex(line)>` asks the shell's own completion system (bash
`complete -p` + dynamic loaders, zsh compsys, fish `complete -C`) for matches:

`__ait_complete <hex(line)>` asks the shell's own completion system (bash
`complete -p` + dynamic loaders, zsh compsys, fish `complete -C`) for matches:

```
ESC ] 7702 ; A BEL
ESC ] 7702 ; C ; <hex(match)> BEL            (repeated)
ESC ] 7702 ; D ; <hex(description)> BEL      (optional, after a match)
ESC ] 7702 ; B BEL
```

`ESC ] 7703 BEL` requests a grid reset.

### 5.6 Agent mode (Round 6)

- Agent loop: model ↔ tools until the task is done, the user cancels, or a step
  budget is hit. Streaming output in an agent conversation panel.
- Tools (v1):
  - `run_command` — typed into a PTY; completion and exit code detected via
    OSC 133 (falls back to a sentinel marker when integration is absent).
  - `read_files` — with size and binary guards.
  - `edit_files` — produces a diff; the user previews it before it is applied.
  - `grep` / `glob` — scoped to the working directory.
  - `ask_user_question` — multiple-choice or free-text prompt in the TUI.
  - `write_to_pty` — **agent-owned PTY**: the agent drives its own interactive
    session (REPLs, `ssh`, `sudo` prompts, installers). This is a core
    differentiator.
- System prompt includes cwd, OS, shell, and the repo's `AGENTS.md` if present.
- All tool calls pass through the permission engine (§5.7).
- We write all of our own system prompts and tool descriptions.

### 5.7 Execution profiles — permission engine (Round 7)

```toml
[agents.execution_profiles.default]
read_files        = "agent_decides"   # agent_decides | always_allow | always_ask | never
write_to_pty      = "always_ask"      # also: ask_on_first_write
execute_commands  = "always_ask"
apply_code_diffs  = "always_ask"
mcp_permissions   = "always_ask"
ask_user_question = "always_allow"
command_allowlist   = []               # regexes, matched against the full command
command_denylist    = ['^(bash|sh|zsh|fish)(\s.*)?$', '^rm(\s.*)?$',
                       '^(curl|wget)(\s.*)?$', '^(ssh|scp|rsync)(\s.*)?$']
directory_allowlist = []
mcp_allowlist = []
mcp_denylist  = []
base_model = ""                        # model config_key (§4.2)
coding_model = ""
command_model = ""
context_window_limit = 0               # 0 = model default
```

- Per tool × per level. Denylist beats allowlist. `agent_decides` lets the model
  flag a call as safe/unsafe; unsafe falls back to asking.
- Multiple named profiles; switch from the TUI; show the active profile in the
  status line.
- The default profile above ships as the safe starting point.

### 5.8 Persistence & planning (Round 8)

- Store conversations, blocks metadata, and agent plans in SQLite under
  `~/.local/share/ait/` (respect `$XDG_DATA_HOME`).
- Resume a previous conversation; list/delete conversations.
- `update_plan` tool: the agent maintains a visible TODO list for the task.
- Long conversations are summarized when they approach `context_window_limit`.

### 5.9 SSH & executor tiers (Round 9)

- Detect `ssh` sessions started inside `ait`; optionally bootstrap the remote
  shell with the same rc snippet (§5.5) so blocks and in-band exec work remotely.
  Remote sessions export `AIT_IS_SSH=1`; the snippet emits `exit_shell` on
  EXIT/HUP so the client can release resources tied to that session.
- Executor tiers behind one `CommandExecutor` trait (design the trait in Round 6
  so these upgrades need no re-architecture):
  1. **In-band** (§5.5.1) — zero setup, works anywhere; races with user typing.
  2. **ControlMaster** — reuse/establish an SSH ControlMaster socket for
     out-of-band remote commands (agent can read files while the user types).
  3. **Remote daemon** (stretch) — `ait remote-daemon` uploaded over the
     ControlMaster connection (per-arch build, local cache, verify after install);
     request/response protocol with request ids and a rotating auth token.
  4. **No-op** — fallback when nothing else is available.
- Per-host allow/deny (`[ssh] hosts_denylist`), and an opt-out per session.

Decided in Round 9 (Spec R09, ADR-0012): each injected shell defines an
`ssh()` function that rewrites a bare interactive, policy-allowed
`ssh [options] <destination>` into `ssh -t <args> '<remote bootstrap>'`,
attaching `-o ControlMaster=auto -o ControlPath=<control-dir>/%C -o
ControlPersist=<dur>` unless the user's own config already manages a socket
(`reuse_existing_control_master`). All other shapes pass through to
`command ssh` — deny means no wrap, never a block. The wrapper emits
`ssh_begin` before connect and `ssh_end` after return — every wrapper hook
carrying a minted `hop` id so nested `ssh` levels attribute correctly;
the remote bootstrap
(a self-contained POSIX-sh script, never a dotfile) lands the snippet file
set in a private `mktemp -d`, exports `AIT_IS_SSH=1` + the `AIT_SSH_*`
policy so nested hops re-wrap, traps EXIT/HUP for `exit_shell` and cleanup,
then runs the remote `$SHELL` as a child. The frontend folds vetted hooks
into a per-tab remote-context stack — a close event retires the level its
`hop` names and everything above — feeding the `ssh:<target>` status
segment, bridge `remote_state` (whose `bootstrapped` is the remote shell's
own integration proof — an unbootstrapped remote clamps off the in-band
tier), and `blocks.host` at seal; ait-owned control sockets retire on
close. `select_executor` prefers a live ControlMaster socket — demoting
permanently on transport failure, the hit run erroring rather than
replaying — and remote
`read_files`/`grep`/`glob` compose through the task's executor while
`edit_files` reports "unsupported" honestly.

### 5.10 Depth features (Rounds 10–11)

#### 5.10.1 MCP client (Round 10)
`rmcp`-based client; servers declared in `~/.config/ait/mcp.json` (and a
project-local `.ait/mcp.json`); tools `call_mcp_tool`, `read_mcp_resource`;
governed by `mcp_permissions` + allow/deny lists.

#### 5.10.2 Skills loader (Round 10)
`SKILL.md` directories loaded from `~/.config/ait/skills/` and `.ait/skills/`,
watched for changes; the agent gets an `invoke_skill` tool.

#### 5.10.3 Secret redaction (minimal in Round 4, complete in Round 10)
One combined regex set (compiled once) applied to everything leaving the PTY
before it reaches context assembly. Built-in patterns: AWS access keys (`AKIA…`),
GitHub tokens (`ghp_`, `github_pat_`, `gho_`, `ghu_`, `ghs_`), OpenAI (`sk-…`),
Anthropic (`sk-ant-…`), Stripe (`sk_live_`, `sk_test_`), Slack (`xox…`, `xapp-`),
JWTs, private-key PEM blocks, plus optional PII (IPv4/IPv6, emails, phone, MAC).
Users can add patterns and disable built-ins.

#### 5.10.4 Input classifier (Round 11)
Auto-detect whether typed input is a shell command or natural language. Start
with a heuristic (known binaries on `$PATH`, shell syntax, sentence shape);
a small local model is a later option behind a cargo feature.

#### 5.10.5 Headless mode & control CLI (Round 11)
`ait --headless "<task>"` runs the agent non-interactively (profile rules still
apply; anything that would ask is denied). `ait ctl <cmd>` drives a running
`ait` instance over a local socket (open tab, send input, read last block).

### 5.11 Packaging & distribution (Round 12)

- `cargo-dist` releases for Linux (x86_64, aarch64) and macOS; shell install
  script; `cargo install` path.
- `ait --version`, `ait --init`, `ait --print-schema`, man-page-style `--help`.
- README, user guide (config, providers, profiles, SSH), and a CHANGELOG.
- Verified install from a published release on a clean machine/container.
- Windows joins the release targets in Release 2 (§5.14).

### 5.12 GUI frontend (Round 13, Release 2)

- An optional desktop frontend over the same core crates, shipped as a
  separate `ait-gui` binary. The TUI remains the default; nothing in the GUI
  is required to use `ait`, and `ait` gains no GUI dependencies.
- Reuses `ait-term`, `ait-shell`, `ait-agent`, `ait-config`, `ait-store` and
  `ait-llm` unchanged — the core stays frontend-agnostic (§1.1); the GUI
  crate holds no business logic.
- Feature-parity target: tabs, scrollback with search/selection, blocks with
  exit status, the agent conversation panel, permission prompts, the plan
  view, and the status line — every frontend renders the same session model.
- Toolkit: **Proposed** — a GPU-accelerated Rust stack rendering the
  `ait-term` grid directly (e.g. `wgpu`-based); `egui` or a webview shell are
  the fallbacks. Decided in the round's grilling and recorded as an ADR.
  Heavyweight deps stay behind cargo features and out of the `ait` tree.
- Distribution: `ait-gui` joins the §5.11 release artifacts for Linux and
  macOS; Windows follows once §5.14 lands. GUI workspace panes (file
  explorer, web browser) ship in the same binary — §5.13.

### 5.13 GUI panels — file explorer & web browser (Round 14, Release 2)

- A file explorer pane in `ait-gui`: browse the session's working directory
  (tracked via OSC 7), preview files, and open them in the system editor.
  Read-only by default — any write goes through a confirmation or the
  agent's `edit_files` diff flow, never a silent mutation.
- An embedded web browser pane for docs and links. Toolkit: **Proposed** —
  a system-webview embedding (e.g. `wry`/`tao`), decided in the round's
  grilling and recorded as an ADR; opening links in the external browser is
  the fallback if embedding is rejected.
- Links and file paths detected in terminal output can open in the
  corresponding pane (opt-in per click; exact gesture decided in the round).
- Both panes are `ait-gui`-only: the core crates and the `ait` TUI gain no
  new dependencies or features. Local-first applies to the web pane —
  terminal content is never sent to the webview host or any remote service,
  and nothing is fetched except what the user navigates to.

### 5.14 Windows support (Round 15, Release 2)

- `ait` (and `ait-gui` with its §5.13 panes where the toolkit allows) runs
  natively on Windows:
  `portable-pty`'s ConPTY backend in `ait-term`; `crossterm` already supports
  Windows on the TUI side.
- Shell integration for PowerShell (`pwsh` and Windows PowerShell) emitting
  the same OSC 133/7 and OSC 7700-series hook events as the Unix snippets.
  `cmd.exe` and integration-less setups degrade gracefully — no blocks, no
  errors (the §5.5 rule applies).
- Platform specifics live behind the existing seams: config under
  `%APPDATA%`, secrets in Windows Credential Manager (`keyring` already
  abstracts it), path handling, and the `[terminal]` shell override.
- SSH integration targets Windows OpenSSH where present; remote sessions are
  unchanged — the far side is still a Unix shell.
- `x86_64-pc-windows-msvc` joins the §5.11 release targets (aarch64 Windows
  optional); CI gains a `windows-latest` leg for the tests that can run
  headless there.

---

## 6. Non-functional requirements

- **Performance:** handle `cat` of a 100 MB file and `yes` floods without
  freezing input; render at ≥ 30 fps under load; startup < 200 ms.
- **Security:** secrets never written to settings or logs; redaction before
  context assembly; permission engine on every tool call; no network calls
  except configured LLM/MCP endpoints.
- **Portability:** Linux first, macOS supported, Windows from Release 2
  (§5.14); bash, zsh, fish, PowerShell.
- **Reliability:** the terminal stays usable when the LLM is slow/down;
  panics restore the host terminal.
- **Testability:** core crates are testable without a TTY or network; CI runs
  `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
  `cargo test --workspace`.

---

## 7. Delivery roadmap

| Round | Feature | Section |
|---|---|---|
| 1 | Terminal core | §5.1 |
| 2 | TUI shell | §5.2 |
| 3 | Configuration & providers | §4, §5.3 |
| 4 | Natural language → command (+ minimal redaction) | §5.4, §5.10.3 |
| 5 | Shell integration & blocks | §5.5 |
| 6 | Agent mode | §5.6 |
| 7 | Execution profiles | §5.7 |
| 8 | Persistence & planning | §5.8 |
| 9 | SSH & executor tiers | §5.9 |
| 10 | MCP client, skills loader, full redaction | §5.10.1–3 |
| 11 | Input classifier, headless mode, control CLI | §5.10.4–5 |
| 12 | Packaging & release | §5.11 |
| 13 | GUI frontend | §5.12 |
| 14 | GUI panels — file explorer & web browser | §5.13 |
| 15 | Windows support | §5.14 |

**MVP = Rounds 1–4**: a working TUI terminal that turns natural language into a
command using any configured endpoint. **v1 = Rounds 1–12**; **Release 2 =
Rounds 13–15**. Post-v1 candidates pending review live in §12.

---

## 8. Risks & engineering constraints

1. OSC 133 alone isn't enough — shells without integration need the rc-snippet
   path, and everything must still work with no integration at all.
2. In-band execution races with the user's typing. Ship it first, but design the
   `CommandExecutor` trait so ControlMaster/daemon tiers are drop-in upgrades.
3. `anthropic_messages` and `openai_*` differ in tool-call serialization and
   streaming events — keep a per-schema message IR.
4. Terminal output → LLM is the main secret-leak path. Redact before context
   assembly, not at the HTTP edge.
5. Feature creep. TUI first; every round must end with something usable and
   merged.

---

## 9. Glossary seed (copy into `GLOSSARY.md`)

- **Session** — one PTY + shell process + its grid and blocks.
- **Grid** — the styled cell matrix plus scrollback for a session.
- **Block** — one command, its output and exit code, delimited by shell hooks.
- **Shell integration / rc snippet** — the script injected into a shell that
  emits OSC 133/7 and AIT hook events.
- **Hook event** — a hex-encoded JSON message on OSC 7700 (`precmd`, `preexec`, …).
- **OSC 133** — the standard semantic-prompt sequences marking prompt/command
  boundaries.
- **Endpoint** — a configured LLM server (`name`, `base_url`, `schema`, models).
- **Endpoint schema** — wire format: `openai_chat_completions`,
  `openai_responses`, or `anthropic_messages`.
- **Model role** — `base_model`, `coding_model`, `command_model`; references a
  model `config_key`.
- **Execution profile** — named set of per-tool permission levels plus
  allow/deny lists.
- **Permission level** — `always_allow`, `always_ask`, `agent_decides`, `never`.
- **Executor tier** — how a command reaches a shell: local PTY, in-band,
  ControlMaster, remote daemon, no-op.
- **In-band execution** — running a command through the user's live shell and
  capturing the result via OSC 7701.
- **Agent-owned PTY** — a PTY the agent drives itself via `write_to_pty`.
- **Remote bootstrap** — injecting shell integration into a remote shell over SSH.
- **Redaction** — masking secrets in terminal output before it reaches an LLM.

## 10. Coding standards seed (copy into `CODING_STANDARDS.md`)

1. Degrade gracefully when shell integration is absent — never error, just lose
   blocks.
2. Design `CommandExecutor` for tier upgrades; in-band execution races with user
   typing.
3. Keep a per-schema message IR; don't unify provider formats early.
4. Secrets never in `settings.toml` or logs — env vars or keyring only.
5. Redact terminal output before context assembly.
6. After removing an element from a collection, guard the empty state before
   indexing (e.g. closing the last tab).
7. Intercept app keybindings before PTY input (Ctrl+S is XOFF).
8. Heavyweight or optional dependencies go behind cargo features.
9. The core crates never depend on the frontend; the TUI holds no business logic.
10. Write our own prompts, tool descriptions and assets.
11. Tests at agreed seams only; core logic is testable without TTY or network.
12. Resist feature creep — every round ships something usable.

## 11. Out of scope for v1

- Accounts, cloud sync, telemetry, team features.
- Hosted model credits or subscription sign-in flows.
- Codebase indexing (`tantivy`) and voice input — post-v1 candidates (§12).

The GUI frontend and Windows support used to be parked here; they are now
committed post-v1 features — §5.12–§5.14 (Release 2, §7).

## 12. Planned features (post-v1, pending review)

Candidates held for review before any commitment to implement. Each becomes a
spec + tickets through a Part A planning round (grill → spec → tickets,
milestone "Release 3"); nothing is implemented until its Part B step runs.
Drop a candidate by closing its spec; move it by editing the milestone.

| Plan | Feature | Spec | Milestone |
|---|---|---|---|
| P01 | Codebase indexing and search (`tantivy`, `search_codebase` tool) | — | Release 3 |
| P02 | Remote daemon executor (§5.9 tier 3) | — | Release 3 |
| P03 | Shell-native completions (§5.5.2) | — | Release 3 |
| P04 | Local model input classifier (§5.10.4) | — | Release 3 |
| P05 | Voice input (push-to-talk → configured STT endpoint or local model) | — | Release 3 |
