# AGENTS.md

AIT is a local-first, provider-agnostic AI terminal (TUI) written in Rust.

## Ground rules

- `REQUIREMENTS.md` is the source of truth for what we build. Cite its section
  numbers (§5.3 …) in specs, tickets and PRs.
- Decisions made since live in `docs/adr/`; an ADR wins over `REQUIREMENTS.md`.
  Update `REQUIREMENTS.md` in the same PR that adds a conflicting ADR.
- Work only inside this repository — except the sibling `ait-notes-*/`
  and `ait-worktrees/` directories used by runbook rounds for research
  notes and ticket worktrees.
- Write our own prompts, tool descriptions and assets.

## Navigation

- `REQUIREMENTS.md` — product requirements; §7 is the round roadmap.
- `GLOSSARY.md` — domain vocabulary. Use these terms in code, issues and docs.
- `CODING_STANDARDS.md` — rules the reviewer enforces.
- `docs/adr/` — architecture decision records.
- `docs/agents/` — issue tracker, triage labels, domain-doc rules.
- Crate map — one line per crate; each round's landed surface lives
  in ADR-0003's table, not here:
  `crates/ait-term` — exists (pure byte-fed terminal core);
  `crates/ait` — exists (the TUI);
  `crates/ait-testkit` — exists (e2e toolkit; dev-dependency only — `docs/testing-toolkit.md`);
  `crates/ait-llm` — exists (provider trait + per-schema IRs/adapters);
  `crates/ait-config` — exists (`settings.toml`, secrets, hot reload);
  `crates/ait-agent` — exists (NL→command, agent mode, execution profiles, transcripts);
  `crates/ait-shell` — exists (OSC 7700 hooks + blocks, shell snippets, ssh);
  `crates/ait-store` — exists (§5.8 persistence — `rusqlite` facade; ADR-0011).

## Commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must pass before any push. `scripts/check.sh` runs the
whole CI gate locally in one command — CI runs the same three, plus
`scripts/check-dep-comments.py` (every `crates/*/Cargo.toml`
dependency documents its resolved version + release date in a comment,
ADR-0002 procedure) and `scripts/check-src-hygiene.sh`.
`rust-toolchain.toml` pins the exact toolchain CI reads it from — bump
it deliberately, one line per release.

## Workflow

- Issue tracker: GitHub `Eng-Elias/ait`, via the `gh` CLI.
- Each round of `REQUIREMENTS.md` §7 is one feature:
  grill → spec → tickets → implement → review → PR → retro.
- A spec issue without `RNN-T*` tickets gets them created from its
  Implementation Decisions — with blocked-by edges — before
  implementing; a round that lands a crate flips its crate-map status
  (and ADR-0003's table) in the same PR.
- Naming:
  - Spec issue: `Spec RNN: <feature>` (e.g. `Spec R01: Terminal core`)
  - Ticket issues: `RNN-T<k>: <title>`
  - Backlog issues: `Backlog RNN: <feature>`
  - Integration branch: `round-NN-<slug>`; retro branch: `round-NN-retro`
  - Planned (post-v1) features: spec `Spec PNN: <feature>`, tickets
    `PNN-T<k>: <title>`, branch `plan-NN-<slug>`, assigned to a
    `Release N` milestone. Do not implement a planned issue until asked to.
- Branch protection (repo ruleset `protected-branches`): `main`, `qa` and
  `dev` are protected. Changes reach them only via pull request — never push
  directly — and the ruleset rejects deletions and non-fast-forward pushes,
  so never delete or force-push them. All merge methods (merge, squash,
  rebase) are allowed; approvals are not required.
- Branch flow: cut every work branch (rounds, tickets, fixes) from `dev` —
  the default branch — and merge it back into `dev` via PR. On release,
  promote `dev` to `main` with a PR; `main` only receives promotion PRs from
  `dev`. `qa` is updated by PR only.
- PRs into `dev` carry a `Closes #N` line for the spec issue and every ticket.
- Actions minutes are budgeted (private repo): CI runs on `main` only —
  the `dev`→`main` promotion PR and the post-merge push. PRs to `dev`
  get no CI; all development checks are local — run `scripts/check.sh`
  before pushing. Its one gap vs the CI suite is the docker SSH e2e
  (`scripts/ssh-e2e.sh run`) — run it when touching §5.9 remote code.
  A draft promotion PR still skips CI; marking it ready runs the suite
  once, and a push during an in-flight run cancels it (`concurrency`).
- Release (ADR-0015):
  - Checklist: promotion PR `dev`→`main` → CI green → flip the repo
    public → `git tag v1.0.0 <main-sha>` → `git push origin v1.0.0` →
    `release.yml` builds all five targets → `verify-install` gate →
    announced release.
  - Version bump for later releases: bump `[workspace.package].version`
    in the root manifest, add the `## [x.y.z]` CHANGELOG entry, and
    update the literal `expect_text` in
    `crates/ait/tests/scenarios/version.toml` — the fixture hardcodes
    the version string and `scripts/check-release-version.py` catches
    drift between all of them.
  - Tag pushes bypass the `protected-branches` rulesets (branches
    only), so `guard.yml` is the only on-main enforcement: a version
    tag on a non-`main` commit or a version mismatch fails the release
    before any build leg.

## Agent skills

Skills live in `.devin/skills/<name>/SKILL.md` — when the Skill tool
doesn't list the one a prompt names, read the file directly.

### Issue tracker

Issues are tracked as GitHub issues on `Eng-Elias/ait`, via the `gh` CLI. See `docs/agents/issue-tracker.md` — prefer `gh api`: `gh issue view` and `gh pr edit` fail on a Projects-classic deprecation.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`,
`ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` at the repo root plus `docs/adr/`. See
`docs/agents/domain.md`.

### Subagents

Worktree, research-notes and skill conventions for runbook rounds:
`docs/agents/subagents.md`.
