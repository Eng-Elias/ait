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
- **Fresh base**: round work anchors to `origin/dev`'s tip — fetch
  and confirm the local checkout is current *before* speccing or
  cutting worktrees, not just at dispatch; a stale `dev` has shipped
  a spec against pre-merge code (R12 near-miss).
- **Checkpoints**: implementers commit after each logical step, not
  once at the end — a subagent that dies mid-task leaves its last
  commit, not an uncommitted worktree.
- **Toolchain**: a subagent shell may lack `cargo` on PATH — the
  rustup shim lives at `$HOME/.cargo/bin`. The sandbox can deny any
  exec from a background subagent (R11: even `cargo fmt`) — a
  dispatch then reports "gate unrun" and the parent runs
  `cargo fmt --all` and `scripts/check.sh` in that worktree
  itself; a ticket that never gated doesn't merge.
- **ntfs3 target dirs**: the maintainer's dev box keeps worktrees
  on an ntfs3 mount where `fallocate`/mmap-backed linker output
  returns EINVAL — `cargo` fails writing binaries and some test
  artifacts. Point the worktree's `target/` at ext4:
  `ln -sfn ~/.cache/ait-targets/<ticket> target` (git-ignored);
  if `rust-lld` still fails on a binary link, run that step with
  `RUSTFLAGS="-C linker-features=-lld"` (GNU ld). R14 hit this in
  three tickets independently before it was written down.
- **PR screenshots**: evidence images go on a sibling
  `rNN-pr-assets` branch — cut from `origin/dev`, one commit marked
  "not for merge" — under `docs/pr-evidence/rNN/`; PR bodies embed
  `github.com/<owner>/<repo>/blob/<sha>/<path>?raw=true`. The
  assets branch is never merged.
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
  dirs run tens of GB — mostly incremental artifacts a ticket gate
  never needs: set `CARGO_INCREMENTAL=0` in ticket worktrees and
  delete the dirs at cleanup (an R10 implementer stalled mid-gate
  at 100% disk and spent a cycle reclaiming caches).
- **Teardown**: a merged ticket branch leaves its worktree checked
  out and its `target/` on disk — the merge doesn't clean either.
  Remove each ticket's worktree *when it merges*, not at round
  cleanup — sibling target dirs filled the disk mid-gate twice in
  R11: `scripts/remove-worktree.sh ait-worktrees/<ticket>` kills
  leftover e2e children, drops the worktree and its target dir, and
  deletes the branch (GitHub already deleted the remote side on
  merge). Run it by hand instead and the steps bite twice:
  `pkill -f <worktree>` matches the caller's own command line — a
  chained `git worktree remove <path>` repeats the literal name, so
  even a bracket-regex pattern still self-matches (R16 hit this 3x;
  pkill must run in a *standalone* command with no second command
  naming the worktree) — and an argv pattern can't see Xvfb at all,
  which is how ~970 stale Xvfb survived R15's removed worktrees
  holding deleted inodes on a near-full volume (and masquerading as
  a "process holds the link target" linker failure). Round cleanup
  verifies `git worktree list` shows none left and
  `pgrep -fl 'Xvfb|ait-gui'` shows no orphans.
- **Gate output**: run the gate teed to a log —
  `scripts/check.sh 2>&1 | tee /tmp/gate-<ticket>.log`. Piping to
  `tail` keeps the tail but discards the failure detail a diagnosis
  then has to rebuild.
- **Known-flaky tests**: the canonical list is `docs/using-the-testkit.md`'s
  "Flaky tests" section — a gate failure on one that goes green on a
  plain re-run needs no re-diagnosis. A round that flushes out a new
  flake appends it there in its retro; prompts carry this file's
  pointer, not the list.

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
  later tickets). New surface grown in a crate the spec froze is a
  deviation — name it in the report; shipping it silently turns it
  into a review finding (R14's `close_agent_panel`).
- **Merger/explorer** dispatches stay sparse: the explorer needs only
  the notes-dir path, the spec issue, and the topic list; the merger
  only the two branches and the gate command. The merger skips the
  re-gate when the merge yields a tree the ticket gate already
  passed — a branch containing the integration tip produces its own
  tree on merge (`git diff <tip> <merge-result>` empty); gate only
  when the merge introduces new content (conflict resolution, a
  sibling merged in between). A skipped gate is reported as skipped,
  not as a pass. A clean git merge can still produce a broken tree:
  when both sides add fields to the same struct literal — the
  `Shared`/`GuiApp` fixtures are the recurring seam (R13) — git merges
  both hunks silently and drops one side's field. Build before
  declaring the merge clean; never trust the absence of conflict
  markers on shared struct literals. `cargo fmt --all --check`
  the merge result too — union hunks merge clean but arrive
  unformatted (R14's T3 merge).
- **Flake triage**: a gate failure in a test on
  `docs/agents/flaky-tests.md`'s list is probably load, not your
  change — rerun the named test alone before re-diagnosing; a pass
  in isolation means record-and-move-on, never gate-loop.
- **Docker gates**: `ssh-e2e.sh`/`verify-install.sh` need a live
  docker daemon — the CLI may sit on a context with the daemon
  stopped (`desktop-linux` with Docker Desktop off, R12): start it
  first, don't debug a dead socket.

