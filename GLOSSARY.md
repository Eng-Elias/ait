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
- **Integration state** — how much shell integration a tab's session has
  reported: `Absent` (none), `MarksOnly` (`OSC 133` without
  `init_shell`), `Integrated` (`init_shell`/`bootstrapped` seen).
- **OSC 133** — the standard semantic-prompt sequences marking prompt/command
  boundaries.
- **Endpoint** — a configured LLM server (`name`, `base_url`, `schema`,
  models).
- **Endpoint schema** — wire format: `openai_chat_completions`,
  `openai_responses`, or `anthropic_messages`.
- **Model role** — `base_model`, `coding_model`, `command_model`; references a
  model `config_key`.
- **config_key** — a model entry's stable id, non-empty and unique across all
  endpoints' models; model roles resolve through it (§4.1–4.2).
- **Execution profile** — named set of per-tool permission levels plus
  allow/deny lists.
- **Active profile** — the execution profile currently armed: `default`
  at launch, switched with `Alt+P`, shown on the status line when it is
  not `default`; it drives every tool call's verdict and the model-role
  resolution for the next task or suggestion. Ephemeral — never written
  back to `settings.toml`; a name a reload deletes stays armed and the
  engine asks for every call until the user picks a live one.
- **Permission level** — `always_allow`, `always_ask`, `agent_decides`,
  `ask_on_first_write`, `never`.
- **Executor tier** — how a command reaches a shell: local PTY, in-band,
  ControlMaster, remote daemon, no-op.
- **In-band execution** — running a command through the user's live shell and
  capturing the result via OSC 7701.
- **In-band request** — the `__ait_run <id> <hex(command)>` line ait writes to
  the PTY of an `Integrated` tab (§5.5.1); it stays visible in the grid while
  its merged output and exit code return over OSC 7701 frames.
- **Agent-owned PTY** — a PTY the agent drives itself via `write_to_pty`.
- **Agent conversation** — the agent's durable unit of work (§5.8): one
  per-tab conversation — the user's tasks, the model's streamed replies,
  tool calls and their verdicts, steering messages and terminal statuses
  (done, cancelled, budget reached, error) — as panel entries, model
  context and one `conversations` row in the store alike. Context spans
  tasks; resuming rebuilds all three from the transcript.
- **Transcript** — an agent conversation's neutral persisted event log:
  one `transcript_items` row per `TranscriptItem` (`task`, `steering`,
  `assistant_text`, `tool_call`, `tool_result`, `ask`/`answer`,
  `status`, `summary`, `plan`). The source of truth — replay rebuilds
  whichever schema IR the resolved `coding_model` speaks, so a provider
  switch strands nothing (ADR-0011).
- **Plan block** — the TODO list pinned at the top of the agent panel,
  fed by `update_plan` snapshots (`PlanUpdated`); the latest `plans` row
  restores it on resume.
- **Summary marker** — a `summary` transcript item: the worker rewrote
  the oldest `covers_seq` items into `text` when the estimate crossed
  `SUMMARIZE_AT` of the effective limit; it replays as leading context
  and paints as a "conversation summarized" status entry.
- **Store** — `ait-store`'s `Store`: the synchronous SQLite facade over
  `$XDG_DATA_HOME/ait/store.db` (WAL, `PRAGMA user_version` migrations)
  holding conversations, transcripts, plans, terminal sessions and block
  metadata.
- **Tool card** — the conversation-panel entry rendering one tool call:
  name, argument summary and status (awaiting approval, running, done,
  denied, failed).
- **Approve card** — the prompt a tool call parks on until the user picks
  *allow once*, *allow this tool for the rest of the task*, or *deny* —
  what the active profile's *ask* verdict (and the missing-profile
  fail-safe) renders.
- **Step budget** — the cap on model turns per task (`STEP_BUDGET`, 50);
  tripping it ends the task with a `BudgetReached` status.
- **Remote bootstrap** — the self-contained POSIX-sh script an integrated
  `ssh()` sends as the remote command (Spec R09): it lands the injected
  snippet file set in a private remote `mktemp -d`, exports the session env
  (`AIT_SESSION_ID`, `AIT_IS_SSH=1`, the `AIT_SSH_*` policy), traps
  EXIT/HUP for `exit_shell` and cleanup, then runs the remote `$SHELL` —
  never touching a remote dotfile.
- **Remote session** — a tab whose foreground shell is a bootstrapped
  remote shell; `ssh_begin` opens it, `ssh_end` or a remote `exit_shell`
  closes it, and the status line marks it `ssh:<target>` (§5.9).
- **ControlMaster tier** — the executor tier that multiplexes `run_command`
  out-of-band over the SSH control socket the `ssh()` wrapper attached
  (`ssh -S <sock> <target> <cmd>`); a transport failure demotes the task to
  the in-band tier for good (§5.9).
- **Redaction** — masking secrets in terminal output before it reaches an LLM.
- **Scenario** — a declarative TOML test of user-visible behavior.
- **Recording** — the committed concrete inputs an agent step resolved to.
- **Driver** — the backing that resolves agent steps.
- **Replay** — re-sending recorded inputs with no model calls.
- **Drift** — a screen-state hash mismatch between recording and replay.
