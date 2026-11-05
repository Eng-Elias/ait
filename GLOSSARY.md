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
- **Pattern name** — a §5.10.3 redaction pattern's stable id
  (`aws_access_key`, `pii_email`, …), defined once in
  `ait_config::pattern_names`: the `[redaction]` `enable`/`disable`
  lists reference it, and `ait-agent`'s `Redactor` table keys each
  compiled regex on it (§4.3, Spec R10).
- **Remote bootstrap** — the self-contained POSIX-sh script an integrated
  `ssh()` sends as the remote command (Spec R09): it lands the injected
  snippet file set in a private remote `mktemp -d`, exports the session env
  (`AIT_SESSION_ID`, `AIT_IS_SSH=1`, the `AIT_SSH_*` policy), traps
  EXIT/HUP for `exit_shell` and cleanup, then runs the remote `$SHELL` —
  never touching a remote dotfile.
- **Remote session** — a tab whose foreground shell is a bootstrapped
  remote shell; `ssh_begin` opens it, `ssh_end` or a remote `exit_shell`
  closes it, and the status line marks it `ssh:<target>` (§5.9).
- **Hop** — the wrapper-minted id (`<session>.<pid>.<seq>`) every SSH
  lifecycle hook carries, attributing it to one level of a possibly-nested
  `ssh` chain; every level shares `AIT_SESSION_ID`, so `hop` is what a
  remote `exit_shell` or inner `ssh_end` is told apart by (§5.9).
- **ControlMaster tier** — the executor tier that multiplexes `run_command`
  out-of-band over the SSH control socket the `ssh()` wrapper attached
  (`ssh -S <sock> <target> -- <cmd>`); a transport failure demotes the task
  permanently — the hit run errors rather than replaying (§5.9).
- **Redaction** — masking secrets in terminal output before it reaches an LLM.
- **Input classifier** — the §5.10.4 decision on Enter (`classify` in
  `ait-agent`): the verified input line plus a `ProgramIndex` — the
  detected shell's builtin table (`ait-shell`'s `builtins_for`, keyed
  on the program basename) merged with `$PATH` executable basenames —
  classifies `Command` or `NaturalLanguage`; a natural-language verdict
  rewrites the submission into the `# ` suggestion flow. Conservative:
  shell syntax, resolvable first words and anything without positive
  sentence evidence stay commands; `[input] classifier` arms it.
- **MCP server** — a Model Context Protocol stdio server declared in
  `mcp.json` (`{"mcpServers": {"name": {"command", "args", "env"}}}`):
  `ait-agent`'s per-session `McpPool` connects user-tier entries
  eagerly per task and exposes their tools/resources to the model via
  `call_mcp_tool`/`read_mcp_resource`, gated by `mcp_permissions` and
  the `mcp_allowlist`/`mcp_denylist` literals (§5.10.1).
- **Project-local config layer** — the `.ait/` directory nearest a
  session's cwd (`project_ait_dir` walks ancestors like `AGENTS.md`):
  its `mcp.json` entries shadow same-named user-tier ones and are
  listed but never spawned — a cloned repo cannot run code on the
  user's machine silently (§5.10.1, Spec R10 us-8).
- **Skill** — a `<dir>/SKILL.md` under `~/.config/ait/skills/` or the
  project-local `.ait/skills/` whose frontmatter `name`/`description`
  the agent lists and whose body + bundled files `invoke_skill`
  returns; a project skill shadows a same-named user skill (§5.10.2).
- **Skill catalog** — the per-task `SkillsCatalog` snapshot of the
  skills on disk (effective skills + surfaced skip errors) that rides
  `AgentRequest.skills`; the frontend's `LiveSkills` watcher refreshes
  it on changes under either root, keeping last-good copies of
  entries a write left malformed (§5.10.2, ADR-0006).
- **Control socket** — the per-instance Unix socket a running `ait`
  listens on (`${XDG_RUNTIME_DIR}/ait/<pid>.sock`, else
  `$TMPDIR/ait-<uid>`; dir `0700`, socket `0600`), drained on the app
  tick and carrying one single-line JSON request/response per
  connection. `ait ctl <verb>` is its client; `AIT_CTL_SOCKET` —
  exported into every spawned session — names it (§5.10.5).
- **Scenario** — a declarative TOML test of user-visible behavior.
- **Recording** — the committed concrete inputs an agent step resolved to.
- **Driver** — the backing that resolves agent steps.
- **Replay** — re-sending recorded inputs with no model calls.
- **Drift** — a screen-state hash mismatch between recording and replay.
