# Subagents

Conventions for runbook rounds (`/implement-spec` and friends) that dispatch
subagents.

- **Worktrees**: each ticket branch lives in its own worktree under the
  sibling `ait-worktrees/` directory, never inside the repo checkout.
- **Research notes** live in a sibling `ait-notes-<round>/` directory
  outside the repo (the AGENTS.md ground rule carves these out). Treat
  notes as leads, not gospel: verify version-sensitive API claims against
  docs.rs or the crate source before coding from them. A note flags any
  claim its author could not verify (a sandboxed researcher without
  crates.io/docs.rs access, say) with an explicit UNVERIFIED marker, so
  the implementer knows exactly what to re-check.
- **Skill tool**: a subagent without a Skill tool reads the skill file
  directly — `.devin/skills/<name>/SKILL.md` — and follows it.
- **Profiles**: `subagent_explore` is read-only — it returns notes in
  its final report and the parent writes them to `ait-notes-<round>/`
  itself. Dispatch note-writing explorers as `subagent_general`, or
  plan to persist the report.
- **Checkpoints**: implementers commit after each logical step, not
  once at the end — a subagent that dies mid-task leaves its last
  commit, not an uncommitted worktree.

## Prompt skeletons

Runbook prompts carry the same boilerplate every ticket — pass it as
pointers, restated per dispatch, not re-derived:

- **Every dispatch**: the worktree path and branch; the base it must
  verify first (`git rev-parse HEAD` vs the integration branch's tip —
  reset onto it when drifted); the ticket fetch command (`gh api
  repos/<owner>/<repo>/issues/<n> --jq .body` — `gh issue view` fails);
  the spec's issue number for context; the notes dir
  `ait-notes-<round>/`; the skill to invoke (or the
  `.devin/skills/<name>/SKILL.md` path when there's no Skill tool);
  `scripts/check.sh` as the done-gate.
- **Implementer** additionally gets: which sibling tickets run in
  parallel and may merge first (merge the integration tip back before
  reporting done, resolve, re-verify, `git push -u origin <branch>`),
  the API/event names earlier tickets landed that it must consume
  (one short block — the parent keeps this, the implementer shouldn't
  re-explore for it), and the terse report format (what was built,
  tests, gate results, merge state, hand-off names for later tickets).
- **Merger/explorer** dispatches stay sparse: the explorer needs only
  the notes-dir path, the spec issue, and the topic list; the merger
  only the two branches and the gate command.

