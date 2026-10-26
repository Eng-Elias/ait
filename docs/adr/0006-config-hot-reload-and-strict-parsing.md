# 6. Config hot-reload policy and strict settings parsing

Date: 2026-10-27

## Status

Accepted

## Context

Spec R03 requires `settings.toml` to load, validate and hot-reload
(§5.3), with errors that point at the offending key — but left three
consumer policies undecided, each hard-ish to reverse once users depend
on it:

1. **What a broken config does to a running session.** A mid-session
   edit that fails to parse could drop the live settings, keep the last
   good ones, or refuse the session entirely. §6's reliability mandate —
   a typo never costs the terminal — rules out the last option.
2. **What `[terminal]` `shell`/`scrollback_lines` apply to.** Applying
   them to live sessions on reload would retroactively change a running
   session's program (impossible — it is already spawned) and scrollback
   capacity (destructive — shrinking it would drop history a user may be
   reading).
3. **Whether unknown keys pass silently.** §4 reserves sections like
   `[ssh]`/`[redaction]` for later rounds; ignoring unknown keys would
   let a misspelled `schema` or a premature `[ssh]` block sit inert
   forever, violating "errors point at the offending key".

## Decision

- **Keep last-good, toast the error.** At startup an invalid
  `settings.toml` boots `ait` on built-in defaults; a mid-session reload
  that fails to parse keeps the previous `Settings`. Both surface the
  `ConfigError` — which names the offending key path — as a
  `config error: <msg>` toast on the status line (~3 s). A successful
  reload swaps the whole live `Settings` at once, so derived state (the
  status-line model slot) can never lag the settings it came from.
- **`[terminal]` fields are a spawn-options template.** `shell` and
  `scrollback_lines` are read when a session is spawned and apply to
  sessions spawned from then on — including on reload, where the updated
  template reaches only future tabs. Live sessions keep their spawn-time
  program and scrollback. Everything else in the settings model (model
  roles → the status line, and later the providers) applies live.
- **Unknown keys are errors.** `deny_unknown_fields` on every settings
  table: a typo'd or premature key fails validation with an error naming
  the key path — at startup, on reload, in `ait --init`'s own template
  validation, and structurally in `--print-schema`'s output
  (`additionalProperties: false`), so editor validation matches the
  loader's strictness.

## Consequences

- A broken edit — including an editor's mid-save — never degrades a live
  session; the debounced watcher (300 ms quiet window) also shields
  reloads from truncated reads. The only cost of strictness is that a
  config written for a *future* ait version fails loudly rather than
  partially applying — the desired trade-off per §5.3.
- The toast is transient: after it expires, a still-broken settings file
  leaves no persistent indicator. Accepted — the file stays broken until
  fixed, and the error reappears on the next save or restart.
- `REQUIREMENTS.md` needed no change: §5.3 mandates hot-reload and
  key-pointing errors without fixing these policies, and §4.3 lists
  `[terminal]` fields without scoping them — this ADR supplies the
  missing decisions rather than contradicting any.
- The e2e seam proves the policy end to end (`crates/ait/tests/e2e.rs`:
  invalid file still boots with the toast; a good reload swaps the model
  slot; a new tab after reload spawns the configured shell while the old
  tab keeps `sh`; a broken reload keeps last-good).
