# 1. Record architecture decisions

Date: 2026-10-24

## Status

Accepted

## Context

AIT is specified in `REQUIREMENTS.md`, which marks some items **Proposed** —
defaults that a round's grilling session may confirm or change. We need a way
to record the outcome of those decisions (and any later ones) without
rewriting history or losing the reasoning behind them.

## Decision

We will keep a collection of architecture decision records (ADRs) in
`docs/adr/`, following the format described by Michael Nygard in
"Documenting Architecture Decisions". Each ADR is a numbered Markdown file
(`NNNN-slug.md`) with a title, date, status (proposed / accepted /
deprecated / superseded by ADR-NNNN), context, decision and consequences.

Per `REQUIREMENTS.md`, an ADR wins over `REQUIREMENTS.md` where they
disagree, and `REQUIREMENTS.md` must be updated in the same PR that adds a
conflicting ADR.

When a later round lands differently than an ADR decided, the ADR gains a
dated `## Amendment — YYYY-MM-DD` section recording what actually shipped
— the original text stays for the record (see ADR-0004's 2026-10-28
amendment). Amending at ticket time is the norm; a reviewer catching the
drift later is the failure mode.

## Consequences

- Decisions have a permanent, reviewable home; spec and tickets can cite ADR
  numbers alongside `REQUIREMENTS.md` sections.
- `REQUIREMENTS.md` stays the source of truth for *what* we build; ADRs hold
  the *why* that can't live there.
- Every conflicting decision requires a small two-file PR (ADR +
  `REQUIREMENTS.md` update), which is intentional friction against silent
  drift.
