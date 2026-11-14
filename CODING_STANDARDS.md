# Coding standards

Rules the reviewer enforces. (Seeded from REQUIREMENTS.md §10.)

1. Degrade gracefully when shell integration is absent — never error, just
   lose blocks.
2. Design `CommandExecutor` for tier upgrades; in-band execution races with
   user typing.
3. Keep a per-schema message IR; don't unify provider formats early.
4. Secrets never in `settings.toml` or logs — env vars or keyring only.
5. Redact terminal output before context assembly.
6. After removing an element from a collection, guard the empty state before
   indexing (e.g. closing the last tab).
7. Intercept app keybindings before PTY input (Ctrl+S is XOFF).
8. Heavyweight or optional dependencies go behind cargo features.
9. The core crates never depend on the frontend; the TUI holds no business
   logic.
10. Write our own prompts, tool descriptions and assets.
11. Tests at agreed seams only; core logic is testable without TTY or
    network.
12. Resist feature creep — every round ships something usable.
13. Model-originated text bound for PTY input passes the sanitizer — no
    control bytes, truncated at the first line break; it types, never
    self-executes (the mirror of 5's terminal→context redaction).
14. In-flight async work pins a stable identity (`Tab::id`, a session
    token) — never a positional index or an OS handle that reorder or
    recycle underneath it (R04's `SuggestionPin`).
15. Gate PTY/shell test waits on output only execution produces — never
    the command's own echo: type `echo FD$((100+24))`, wait for `FD124`
    (`docs/using-the-testkit.md`).
16. A test that scripts an external protocol's event order mirrors the
    order a real emitter produces; a convenient fictional order is a
    test bug, not a shortcut. (R06's MarksOnly `133;B→D` passed tests
    while failing every real shell, whose precmd order is `D,A,B`.)
17. Text that crosses a crate boundary — wire strings, synthetic ids,
    status lines, preview clips — is defined once in the library
    crate and imported by the frontend, never re-literal'd at a
    second site (R08's interrupted-result / replay-id duplication
    drifted until the review single-sourced them).
18. A regex that can plausibly over-match ships a negative specimen
    beside its positive one — near-miss text the pattern must NOT
    match. Over-match-prone categories especially (§5.10.3's PII
    set): a lax edge mangles ordinary text, and `pii_ipv6`'s bare
    `::` match would have redacted every Rust path segment.
19. Markdown prose wraps at ~80 columns; fenced code blocks, tables
    and URLs are exempt (a URL split across lines breaks copy-paste,
    a table row can't wrap). Reviewers cite this rule rather than
    re-adjudicating line-length nits per round (R12).
20. No filesystem or environment syscalls on per-frame paths (paint,
    dispatch, hover) without memoization — an `exists()` or a
    `/proc/<pid>/cwd` readlink per token per frame stalls the UI
    thread on slow filesystems. Cache keyed on the inputs that
    invalidate the result (position + content + cwd), never
    wall-clock. (R14: Ctrl+hover detection and the explorer's cwd
    re-root both needed a memo pass after review.)
21. An ADR's concrete names — filenames, identifiers, wire fields —
    are a contract: rename the artifact, update the ADR in the same
    commit. (R16: ADR-0020 promised `pwsh.ps1` and a `<seq>`-suffixed
    driver pipe while the code shipped `snippet.ps1` and a different
    scheme — the ADR is the cited contract, so drift is a defect.)
22. A test whose precondition can't be met fails loudly or gates
    itself off (`#[ignore]`, `cfg`, scenario `platforms`) — never a
    silent `return`. (R16: the inject smoke tests skipped on snippet
    materialization failure, hiding a real defect path.)
23. Check `GLOSSARY.md` before coining a domain term — a name already
    defined for another concept gets a fresh word. (R16: `endpoint`
    in the IPC docs collided with the existing LLM-endpoint meaning.)

