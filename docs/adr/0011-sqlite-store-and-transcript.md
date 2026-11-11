# 11. SQLite store and the transcript as source of truth

Date: 2026-11-04

## Status

Accepted

## Context

Round 8 (Spec R08, REQUIREMENTS.md §5.8) ended the agent's
ephemerality: conversations, terminal-session block metadata and
plans now persist under `$XDG_DATA_HOME/ait/`, resumable across
restarts, tabs, model switches and provider-schema switches. That
forced three decisions worth recording rather than leaving in ticket
text, because each is hard to reverse or surprising without context:

1. **Where does persistence live?** ADR-0003 anticipated an
   `ait-store` crate ("the layout may still grow — such growth gets
   its own ADR"), but an alternative existed: the `ait` binary could
   have owned SQLite calls directly, or `ait-agent` could have grown
   a storage module.
2. **What is the persisted form of a conversation?** The obvious
   candidates were the per-schema request IRs `ait-llm` already
   serializes (store the OpenAI/Anthropic messages verbatim) and a
   neutral vocabulary of conversation events replayed into whichever
   IR the resolved model speaks. The DB schema is a one-way door:
   every row written in v1 must remain readable forever, so the
   choice is made at first write, not at leisure.
3. **Which SQLite binding, and how packaged?** `settings.toml`
   parsing already warned against assuming system libraries; a
   terminal that stores nothing without a system `libsqlite3` would
   fail in the field, not in CI.

## Decision

- **A new `ait-store` crate owns the whole storage surface.** One
  synchronous `Store` facade over `rusqlite` — no tokio, no `ait-*`
  dependencies: it knows SQL and opaque JSON, nothing else. The `ait`
  binary holds the handle (open failure degrades to a toast and an
  unpersisted run, never a crash); `ait-agent` stays disk-free —
  continuity arrives on `AgentRequest.prior`, so the worker remains
  testable without a filesystem. Synchronous is deliberate (ADR-0004:
  the binary is tokio-free) and sufficient: writes are small inserts
  batched per drain in one transaction, never on the render path.

- **The persisted form is a neutral transcript, not the schema IR.**
  `transcript_items` stores `ait-agent`'s `TranscriptItem` vocabulary
  — `task`, `steering`, `assistant_text`, `tool_call`, `tool_result`,
  `ask`/`answer`, `status`, `summary`, `plan` — as internally tagged
  JSON, one row per event with a contiguous `seq`. Replay
  (`Conversation::from_transcript`) rebuilds whichever IR the
  resolved `coding_model` speaks at resume time. The alternatives
  lost for concrete reasons:
  - Storing the IR would pin a conversation to the schema that
    produced it: switching providers mid-conversation would strand
    the history or require a cross-schema migration of every stored
    row — §5.3's no-unify rule respected in the wrong place.
  - The IR also conflates *panel state* with *model state*. The
    transcript keeps both separated by kind: `status` items persist
    for the panel and drop at replay; `plan` snapshots replay only
    their latest; an unanswered `tool_call` (a crash mid-run, an
    approve card parked at exit) gains a synthetic "interrupted"
    result in the IR so no provider sees a dangling call — while the
    record itself stays unanswered.
  - The cost accepted: replay re-pays item→IR translation on every
    resume and every in-tab task. It is bounded by conversation size
    and buys schema portability — the trade this ADR exists to name.

- **`rusqlite` with `bundled`, pinned per ADR-0002.** `bundled`
  compiles the vendored amalgamation via `cc`: no system SQLite is
  required and every build gets the same engine. The dependency is
  unconditional, not a cargo feature — CODING_STANDARDS.md §8 targets
  *optional* heavyweight deps, and the store is core Round-8
  function; a terminal that can't persist would degrade silently on
  minimal installs. The lockfile carries rusqlite 0.41.0 because
  0.41.1 was inside the 7-day release-age window at adoption — the
  `uuid` precedent.

- **The DB schema is versioned and deliberately boring.** Sequential
  migrations tracked by `PRAGMA user_version`; `journal_mode=WAL`, a
  `busy_timeout` and `foreign_keys=ON` on every open (concurrent
  `ait` instances share the file). Tables: `conversations`,
  `transcript_items`, `plans` (latest snapshot only),
  `terminal_sessions`, `blocks`. Metadata only — block rows carry
  command, exit, duration and bounds, never output text or
  scrollback. CRUD only: the hardest query is "load a transcript";
  search is a later round's problem (§2 `tantivy`, post-v1).

## Consequences

- The transcript vocabulary is now a *compat surface*: its serde
  shape is golden-tested, and changing a kind's wire form is a data
  migration, not a refactor.
- Resume, in-tab continuity and mid-conversation schema switches are
  one mechanism — `AgentRequest.prior` replay — so the semantic
  change (a conversation now spans tasks) needed no second design.
- `REQUIREMENTS.md` flips in the same change: §2's storage row
  loses "**Proposed:**" (decided: `rusqlite`, bundled), §3.1's crate
  table gains the `ait-store` row, and ADR-0003's table does the
  same — growth exactly as it anticipated.
- Rejected alternatives on record: IR-JSON storage (strands schema
  switches), `ait`-owned SQLite (mixes plumbing into the frontend),
  an async store (ADR-0004 forbids a runtime in the binary), and
  system-linked SQLite (a field-failure mode `bundled` removes).
