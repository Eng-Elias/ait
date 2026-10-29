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
