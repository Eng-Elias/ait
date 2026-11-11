# 13. The project-local `.ait/` layer and the `mcp.json` shape

Date: 2026-11-05

## Status

Accepted

## Context

Round 10 (Spec R10, REQUIREMENTS.md §5.10.1–2) added two config
surfaces outside `settings.toml`: `mcp.json` declaring MCP stdio
servers, and `skills/` directories holding `SKILL.md` files — each at
a user layer (`$XDG_CONFIG_HOME/ait/…`) and a project layer
(`<project>/.ait/…`). Two decisions are hard to reverse or surprising
without context:

1. **Which document shape `mcp.json` speaks.** Agent tools have
   converged on a de-facto standard — `{"mcpServers": {"name":
   {"command": ..., "args": [...], "env": {...}}}}` — and §5.10.1
   fixes the locations but not the schema. Alternatives: a bespoke
   ait-specific shape, TOML beside `settings.toml`, or folding server
   declarations into `settings.toml` itself. The choice is
   user-visible on day one and painful to migrate once users have
   files on disk.
2. **How much a repository may make ait do.** A project-local `.ait/`
   layer is what lets a repo ship its own agent tooling — but a
   cloned repo is *untrusted input*: `git clone && ait` must not run
   code the repo shipped without the user consenting. Alternatives:
   spawn project servers like user ones (eager, per task), a
   "trust this repo" allowlist, or no project layer at all.

## Decision

- **`mcp.json` speaks the de-facto `mcpServers` shape, stdio only.**
  `ait_config::McpConfig::from_json` validates
  `{"mcpServers": {"<name>": {"command", "args", "env"}}}` —
  `command` required and non-empty, `args`/`env` optional, unknown
  fields denied — so configs users already maintain for other agent
  tools drop in verbatim (Spec R10 us-1). A `url`-keyed remote entry
  is a load error naming the server (`mcpServers.<name>.url: stdio
  transport only this round`), not a silent skip; remote transports
  are out of scope. `env` values pass through literally — no
  `${VAR}` expansion or keyring integration this round — and a
  missing file is no servers while a malformed one is a
  `ConfigError` naming the path, carried into the prompt's "MCP
  servers" section rather than toasting-and-forgetting.

- **`.ait/` is the project-local config layer — discovered, never
  trusted.** `project_ait_dir` walks ancestors from a session's cwd
  and takes the nearest `.ait` directory — the same nearest-wins
  walk `AGENTS.md` gets. Its `mcp.json` servers shadow same-named
  user-tier ones and its `skills/` shadow same-named user skills
  (the more specific layer wins). The trust boundary is *what runs,
  when*: user-tier servers connect eagerly at each task's reconcile
  (spawn, handshake, `tools/list` + `resources/list` under a connect
  timeout); project-tier entries are catalog-listed as `declared`
  but **never spawned at reconcile** — their only connect is lazy,
  inside the already-allowed `call_mcp_tool`/`read_mcp_resource`
  call itself, so a denied call never spawns the process (Spec R10
  us-8). The approve card names the declaring layer —
  `(declared by the project .ait/mcp.json)` — because asking to run
  repository-shipped code reads very differently from asking to run
  the user's own config (us-9). Skills need no equivalent gate:
  `invoke_skill` returns text, not a spawned process, and rides the
  `read_files` level like any read.

- **Consent is per-call, not per-repo.** There is no "trust this
  repo" flag this round — the §5.7 gate's existing machinery
  (`mcp_permissions`, literal `mcp_allowlist`/`mcp_denylist`
  `server`/`server:tool` entries, *allow for task*) is the consent
  surface. A repo-level trust tier would be a second, weaker gate;
  the per-call card already answers "do you want this server's code
  to run?".

- **`mcp.json` is re-read per task, not watched.** The per-task
  reconcile is the reload seam — editing `mcp.json` applies to the
  next task with no restart — while the `skills/` roots get the
  `watch_dirs` machinery because their catalog is frontend-owned
  (`LiveSkills`). Two mechanisms, one each, where each file's
  consumer already lives.

## Consequences

- A cloned repository can declare servers and skills but cannot
  cause a spawn: the lazy connect inside the gated call is the only
  project-tier spawn path, and `mcp_allowlist`/`always_allow` take
  the same lazy path — skipping the ask never skips the consent
  that is the allow itself.
- The `mcpServers` shape means zero-format docs of our own to
  maintain, and a `url` rejection today leaves remote transports a
  clean extension point later.
- The catalog prompt section distinguishes `available` /
  `declared` / `unavailable` per server plus origin — the model can
  name project-tier tools before the first connect, and failures
  surface instead of silently absenting.
- Rejected alternatives on record: a bespoke or `settings.toml`
  shape (strands users' existing configs, splits one concern across
  two files), eager project spawns (the clone-and-run failure this
  ADR exists to prevent), a repo trust list (a second, weaker
  consent surface), namespaced per-server tools on the wire (the
  generic `call_mcp_tool`/`read_mcp_resource` keep the registry
  fixed) and `mcp.json` file watching (the per-task reconcile is
  reload enough).
- `REQUIREMENTS.md` §4.3's `[redaction]` row stops being proposed in
  the same round's PRs; GLOSSARY.md gained *MCP server*, *skill*,
  *skill catalog*, *pattern name* and *project-local config layer*.
