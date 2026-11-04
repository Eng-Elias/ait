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
| `ait-config` | `settings.toml` model, JSON Schema emission, secrets lookup; §5.7 command-list regex validation (Round 7 — adds the `regex` edge) | exists (Round 3) |
| `ait-llm` | Provider trait, per-schema message IR, streaming adapters | exists (Round 3) |
| `ait-shell` | Shell-integration scripts (bash/zsh/fish) and the hook/exec protocol | exists (Round 5) |
| `ait-agent` | Redaction, context assembly, reply sanitization, `CommandSuggester` (Round 4); `AgentSession` loop, tool registry + interim gate, `CommandExecutor`/`ShellBridge`, agent-owned PTY (Round 6 — adds `ait-term` and `ait-shell` edges); §5.7 permission engine (Round 7); §5.8 transcript vocabulary, replay and summarization (Round 8) | exists (Round 4; agent mode Round 6) |
| `ait` | TUI binary (frontend only — no business logic) | exists (Round 2 TUI; Round 3 config wiring) |
| `ait-store` | SQLite store: conversations, transcript items, plans, terminal sessions, block metadata (Round 8, ADR-0011) | exists (Round 8) |

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
