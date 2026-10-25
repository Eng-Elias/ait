# Subagents

Conventions for runbook rounds (`/implement-spec` and friends) that dispatch
subagents.

- **Worktrees**: each ticket branch lives in its own worktree under the
  sibling `ait-worktrees/` directory, never inside the repo checkout.
- **Research notes** live in a sibling `ait-notes-<round>/` directory
  outside the repo (the AGENTS.md ground rule carves these out). Treat
  notes as leads, not gospel: verify version-sensitive API claims against
  docs.rs or the crate source before coding from them.
- **Skill tool**: a subagent without a Skill tool reads the skill file
  directly — `.devin/skills/<name>/SKILL.md` — and follows it.
- **Untracked round docs**: worktrees only see committed files. The
  orchestrator copies untracked round docs (runbook, design doc) into
  `ait-notes-<round>/` so implementers can read them.
- **Implementer prompt template** — the boilerplate every ticket
  subagent gets:

  ```prompt
  Ticket <id> on <tracker>. Repo <path> (do NOT work in the main
  checkout). Integration branch <branch>; worktree
  ../ait-worktrees/<slug> via `git worktree add ../ait-worktrees/<slug>
  -b <slug> <branch>`; confirm `git merge-base --is-ancestor <branch>
  HEAD`, else `git reset --hard <branch>`.
  Read: the ticket, the spec issue, ait-notes-<round>/ (design doc +
  notes — verify against source), AGENTS.md, CODING_STANDARDS.md,
  GLOSSARY.md, .devin/skills/tdd/SKILL.md (+ tests.md, mocking.md).
  Check the crate's existing utils (hash, hex, temp dir, name
  sanitizers) before writing a private one — parallel tickets have
  landed duplicates before.
  Build exactly the ticket. Gates: cargo fmt --all --check; cargo
  clippy --workspace --all-targets -- -D warnings; cargo test
  --workspace. Commit, `git merge --no-edit <branch>` into your branch,
  re-run gates if it changed anything. Report: commit hash(es), public
  API surface, test list + results, deviations, notes for later
  tickets.
  ```
