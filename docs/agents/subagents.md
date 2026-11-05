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
- **Toolchain**: a subagent shell may lack `cargo` on PATH — the
  rustup shim lives at `$HOME/.cargo/bin`. When the sandbox denies a
  subagent's cargo entirely, the parent runs `scripts/check.sh` in
  that worktree itself; a ticket that never gated doesn't merge.
- **Dispatches are immutable** — a running background subagent
  can't be resumed or messaged mid-flight; front-load every
  pointer at dispatch. An update that lands mid-task rides the
  notes dir (tell implementers to re-check it once mid-work) or
  waits for the report.
- **Notes timing**: explorers land mid-round — dispatch them
  before or alongside the first implementer, not after. A
  first-wave implementer's notes-dir poll may come up empty; it
  proceeds regardless.
- **Target dirs**: parallel ticket worktrees each keep their own
  `target/` (the default) or a dedicated `CARGO_TARGET_DIR` — never
  share one between siblings. Same-name crates built concurrently
  confuse cargo's artifacts: an R08 gate ran a sibling's test
  binary and clippy-checked a lib missing its own branch's changes.
  Sharing is safe only for serial builds of the same content — the
  integration branch's merge gates may share one dir. Round target
  dirs run tens of GB: delete them at cleanup.
- **Teardown**: a merged ticket branch leaves its worktree checked
  out and its `target/` on disk — the merge doesn't clean either.
  At round cleanup: `git worktree remove ait-worktrees/<ticket>`
  (`--force` when its untracked `target/` blocks removal) drops the
  worktree *and* its target dir, then `git branch -D <branch>`;
  GitHub already deleted the remote branch on merge.
- **Gate output**: run the gate teed to a log —
  `scripts/check.sh 2>&1 | tee /tmp/gate-<ticket>.log`. Piping to
  `tail` keeps the tail but discards the failure detail a diagnosis
  then has to rebuild.

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
  tests, gate results, merge state, deviations from the ticket's
  acceptance criteria — required, write "none" — hand-off names for
  later tickets).
- **Merger/explorer** dispatches stay sparse: the explorer needs only
  the notes-dir path, the spec issue, and the topic list; the merger
  only the two branches and the gate command.

