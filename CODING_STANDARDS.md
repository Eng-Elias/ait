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
