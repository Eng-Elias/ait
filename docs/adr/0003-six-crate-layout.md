# 3. Six-crate workspace layout

Date: 2026-10-24

## Status

Accepted

## Context

`REQUIREMENTS.md` §3.1 marked the crate layout **Proposed**: six crates
splitting the terminal core, config, LLM access, shell integration, agent
logic and the TUI binary. Round 1 creates the first real crate, so the
layout decision must be settled before the workspace grows.

## Decision

We adopt the §3.1 six-crate layout unchanged:

| Crate | Responsibility | Status |
|---|---|---|
| `ait-term` | PTY spawn, VT parsing, grid + scrollback, session lifecycle, OSC interception | exists (Round 1) |
| `ait-config` | `settings.toml` model, JSON Schema emission, secrets lookup; §5.7 command-list regex validation (Round 7 — adds the `regex` edge); §5.10.3 `[redaction]` section + `pattern_names` vocabulary + `watch_dirs` directory watching for the skills roots (Round 10); §5.10.4 `[input]` section — `input.classifier` (Round 11) | exists (Round 3) |
| `ait-llm` | Provider trait, per-schema message IR, streaming adapters | exists (Round 3) |
| `ait-shell` | Shell-integration scripts (bash/zsh/fish) and the hook/exec protocol; §5.10.4 per-shell builtin tables (`builtins_for` — the shared POSIX set plus bash/zsh/fish), the classifier `ProgramIndex`'s builtin half (Round 11); §5.14 `pwsh.ps1` serving pwsh 7 + WinPS 5.1, `ShellKind::Pwsh` planning, `ShellFamily` builtin tables and the shell-aware Absent sentinel (Round 16, ADR-0020) | exists (Round 5) |
| `ait-agent` | Redaction, context assembly, reply sanitization, `CommandSuggester` (Round 4); `AgentSession` loop, tool registry + interim gate, `CommandExecutor`/`ShellBridge`, agent-owned PTY (Round 6 — adds `ait-term` and `ait-shell` edges); §5.7 permission engine (Round 7); §5.8 transcript vocabulary, replay and summarization (Round 8); named-pattern `Redactor` built per request from `[redaction]` + §5.10.1 `McpPool`/`call_mcp_tool`/`read_mcp_resource` on the worker runtime — adds the `rmcp` edge; §5.10.2 `SkillsCatalog` snapshot on `AgentRequest` + `invoke_skill` on the `read_files` level (Round 10); §5.10.4 `classify` + `ProgramIndex`/`InputClass` — the input classifier's decision half (Round 11) | exists (Round 4; agent mode Round 6) |
| `ait` | TUI binary (frontend only — no business logic); §5.10.2 `LiveSkills` watcher on both skills roots feeding each `AgentRequest`'s catalog snapshot (Round 10); §5.10.4 the third submission trigger (NL verdict → `# <line>` rewrite) and §5.10.5 the `ait ctl` control socket server/client + the `--headless` driver (Round 11) | exists (Round 2 TUI; Round 3 config wiring) |
| `ait-store` | SQLite store: conversations, transcript items, plans, terminal sessions, block metadata (Round 8, ADR-0011) | exists (Round 8) |
| `ait-testkit` | e2e toolkit — PTY harness, TOML scenario runner, record/replay driver protocol, `target/e2e-failures/` evidence (E01); `gui` module — `GuiE2e`/`GuiSpawnConfig`/`GuiBinary`/`GuiScenario` spawning the real `ait-gui` binary under Xvfb and driving it over the test-driver socket, stub-opener + dual-binary legs (Round 15, ADR-0019) | exists (E01) — dev-dependency only |
| `ait-gui` | eframe/egui-wgpu desktop frontend (§5.12, Round 13, ADR-0016/0017); explorer + browser panes (§5.13, Round 14, ADR-0018); off-by-default `test-driver` feature — the env-named driver socket, `egui::Event`/`RawInput` injection, versioned `dump` + hit-target registry (Round 15, ADR-0019) | exists (Round 13) |

The layout may still grow (e.g. `ait-ssh`) — such growth gets its own ADR
(`ait-store`'s was ADR-0011).

## Consequences

- Core crates never depend on the frontend (CODING_STANDARDS.md §9): the `ait`
  binary is frontend-only, everything testable lives below it.
- Each round adds its crate to `crates/` under the workspace's
  `members = ["crates/*"]` glob; the AGENTS.md crate map tracks
  exists/planned status.
- `ait-term` lands first as a pure, byte-fed core; the live PTY driver and the
  remaining crates arrive in their own rounds per §7.
