# 4. `ait-term` stays synchronous; `tokio` joins at the `ait` binary

Date: 2026-10-24

## Status

Accepted

## Context

`REQUIREMENTS.md` §2 lists `tokio` as the async runtime ("PTY reader, agent
loop, HTTP run concurrently"). Round 1 built `ait-term`'s live PTY driver
(`LiveSession`) on `std::thread` + `std::sync::mpsc` with no async runtime,
and Spec R01 (issue #3) decided that split explicitly: "No async runtime
inside `ait-term`; `tokio` joins at the `ait` binary in Round 2" — noting
that §2's tokio row "is about the app runtime, not this crate". The decision
was never written down; this ADR records it.

## Decision

`ait-term` contains no async runtime. `LiveSession` runs a dedicated
`std::thread` reader and waiter, communicates over `std::sync::mpsc`
channels, and exposes a fully synchronous API (`drain_events`,
`try_recv_event`, `send_bytes`).

`tokio` remains the *application* runtime: it joins at the `ait` binary in
Round 2 for the agent loop, HTTP and SSE per §2/§5.6. The §2 Async row
governs that app runtime, not `ait-term`'s internals. The frontend bridges
the synchronous session API onto tokio (a pump task per `LiveSession`, or
`try_recv_event` polled from the render loop); the core crate itself does
not take a tokio dependency.

## Consequences

- `ait-term` stays dependency-light, embeddable in non-tokio contexts, and
  testable without a runtime.
- §2's Async row Notes now points here, so the scope of the tokio decision
  can't silently drift onto the core crate.
- The sync→async bridge is a Round 2 frontend concern; if polling ever
  proves insufficient, `spawn_blocking` or a dedicated pump thread lives in
  `ait`, not `ait-term`.
