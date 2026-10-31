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
- Crate map (ADR-0003): `crates/ait-term` — exists (Round 1: pure byte-fed
  terminal core); `crates/ait` — exists (Round 2 TUI; Round 3 wires
  `ait-config` into it); `crates/ait-testkit` — exists (Round E01: e2e
  testing toolkit; dev-dependency only, no product crate may depend on
  it; see `docs/testing-toolkit.md`);
  `crates/ait-llm` — exists (Round 3: `LlmProvider` trait, `Endpoint`
  dispatch, per-schema IRs, HTTP/SSE streaming adapters);
  `crates/ait-config` — exists (Round 3: `settings.toml` model, load,
  secrets, hot-reload watcher; wired into the `ait` binary — `--init`,
  `--print-schema`, `[terminal]` session options, status model slot);
  `crates/ait-agent` — exists (Round 4: §5.4 NL→command —
  credential redaction, context assembly and reply sanitization pure
  seams, channel-based `CommandSuggester`; the tool registry, agent
  loop and permission engine land in Round 6);
  `crates/ait-shell` — exists (Round 5: §5.5 — OSC 7700-series
  hook/run codec, `RunAccumulator` and the in-band `__ait_run`
  emit side; snippets and the injection planner land in later
  tickets).

## Commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must pass before any push. CI runs the same three, plus
`scripts/check-dep-comments.py` — every `crates/*/Cargo.toml`
dependency documents its resolved version + release date in a comment
(ADR-0002 procedure, `docs/adr/0002`).
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

## Agent skills

### Issue tracker

Issues are tracked as GitHub issues on `Eng-Elias/ait`, via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`,
`ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` at the repo root plus `docs/adr/`. See
`docs/agents/domain.md`.

### Subagents

Worktree, research-notes and skill conventions for runbook rounds:
`docs/agents/subagents.md`.
