# AGENTS.md

AIT is a local-first, provider-agnostic AI terminal (TUI) written in Rust.

## Ground rules

- `REQUIREMENTS.md` is the source of truth for what we build. Cite its section
  numbers (§5.3 …) in specs, tickets and PRs.
- Decisions made since live in `docs/adr/`; an ADR wins over `REQUIREMENTS.md`.
  Update `REQUIREMENTS.md` in the same PR that adds a conflicting ADR.
- Work only inside this repository. Do not read files outside it.
- Write our own prompts, tool descriptions and assets.

## Navigation

- `REQUIREMENTS.md` — product requirements; §7 is the round roadmap.
- `GLOSSARY.md` — domain vocabulary. Use these terms in code, issues and docs.
- `CODING_STANDARDS.md` — rules the reviewer enforces.
- `docs/adr/` — architecture decision records.
- `docs/agents/` — issue tracker, triage labels, domain-doc rules.
- Crate map: _filled in during Round 1; keep it current when crates change._

## Commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must pass before any push. CI runs the same three.

## Workflow

- Issue tracker: GitHub `Eng-Elias/ait`, via the `gh` CLI.
- Each round of `REQUIREMENTS.md` §7 is one feature:
  grill → spec → tickets → implement → review → PR → retro.
- Naming:
  - Spec issue: `Spec RNN: <feature>` (e.g. `Spec R01: Terminal core`)
  - Ticket issues: `RNN-T<k>: <title>`
  - Backlog issues: `Backlog RNN: <feature>`
  - Integration branch: `round-NN-<slug>`; retro branch: `round-NN-retro`
  - Planned (post-v1) features: spec `Spec PNN: <feature>`, tickets
    `PNN-T<k>: <title>`, branch `plan-NN-<slug>`, assigned to a
    `Release N` milestone. Do not implement a planned issue until asked to.
- Branches: work branches → PR into `dev` (the default branch).
  `main` only receives promotion PRs from `dev`. Never push directly to `dev`
  or `main`; never force-push them.
- PRs into `dev` carry a `Closes #N` line for the spec issue and every ticket.
