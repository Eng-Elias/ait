# Glossary

Domain vocabulary for AIT. Use these terms in code, issues and docs.
(Seeded from REQUIREMENTS.md §9.)

- **Session** — one PTY + shell process + its grid and blocks.
- **Tab** — one session plus its viewport, scroll offset and selection
  state.
- **Grid** — the styled cell matrix plus scrollback for a session.
- **Block** — one command, its output and exit code, delimited by shell hooks.
- **Shell integration / rc snippet** — the script injected into a shell that
  emits OSC 133/7 and AIT hook events.
- **Hook event** — a hex-encoded JSON message on OSC 7700 (`precmd`,
  `preexec`, …).
- **OSC 133** — the standard semantic-prompt sequences marking prompt/command
  boundaries.
- **Endpoint** — a configured LLM server (`name`, `base_url`, `schema`,
  models).
- **Endpoint schema** — wire format: `openai_chat_completions`,
  `openai_responses`, or `anthropic_messages`.
- **Model role** — `base_model`, `coding_model`, `command_model`; references a
  model `config_key`.
- **Execution profile** — named set of per-tool permission levels plus
  allow/deny lists.
- **Permission level** — `always_allow`, `always_ask`, `agent_decides`,
  `never`.
- **Executor tier** — how a command reaches a shell: local PTY, in-band,
  ControlMaster, remote daemon, no-op.
- **In-band execution** — running a command through the user's live shell and
  capturing the result via OSC 7701.
- **Agent-owned PTY** — a PTY the agent drives itself via `write_to_pty`.
- **Remote bootstrap** — injecting shell integration into a remote shell over
  SSH.
- **Redaction** — masking secrets in terminal output before it reaches an LLM.
