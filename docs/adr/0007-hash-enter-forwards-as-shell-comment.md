# 7. `# `+Enter forwards to the shell as a comment

Date: 2026-10-29

## Status

Accepted

## Context

Spec R04's trigger — a line starting with `# ` submitted with Enter (§5.4)
— leaves one user-visible question unanswered: what happens to the
submission itself. Once the shadow+grid check verifies the shell really
echoed the request line, `ait` could either

1. **erase-and-swallow** — send backspaces to wipe the typed line and eat
   the Enter, so the request never reaches the shell, then stream the
   suggestion onto the same input line; or
2. **forward-and-comment** — send the Enter through untouched, letting
   `# <request>` evaluate as a comment, and stream the suggestion onto
   the fresh prompt that follows.

The choice is user-visible and hard to reverse once users rely on it: it
decides what lands in shell history, what a failed or misfired request
costs, and how the feature composes with shells and REPLs that do not
treat `# ` as a comment.

## Decision

We forward the Enter (option 2). The `# ` line is submitted to the shell
like any other input; in bash, zsh and fish a line starting with `# ` is
a comment — a no-op that lands in history like a normal command — and
the suggestion streams into the next prompt the shell draws.

- The trigger's verification (`InputShadow` + grid-echo agreement)
  happens *before* the keypress is forwarded, so desynced shadows —
  history recall, heredoc bodies, REPL input — degrade to an ordinary
  submission, never a swallowed line or a wrong request.
- Erasing the request text is reserved for `Alt+Enter`, where the typed
  line *is* the request and must be cleared so request and suggestion
  cannot interleave — the `# ` path has no such collision because the
  comment takes the line with it.
- Failures cost exactly one no-op comment: a toast explains the failure,
  the request text remains visible in history for retrying or editing,
  and nothing the user typed is lost.

## Consequences

- Request lines persist in the shell's history file — useful for reuse
  (`Up` resubmits, and the `ends_with` verification still applies), but
  it means request text reaches the same place commands do; a request
  containing a secret lands in history like any typed command. Accepted:
  the terminal's redaction duty covers model-bound context (§5.10.3),
  not the shell's own history.
- On a shell that treats `#` differently (e.g. a REPL where `# ` is not
  a comment), the forwarded submission does whatever that shell does
  with it — a cheap, visible no-op-or-error rather than a silently
  swallowed line. The verification check is unchanged in both cases.
- The suggestion types onto the *fresh* prompt, so request text and
  command text can never interleave — the same guarantee `Alt+Enter`
  buys with backspaces, obtained for free.
- `REQUIREMENTS.md` needed no change: §5.4 fixes the trigger but not the
  submission semantics — this ADR supplies the missing decision rather
  than contradicting any.
- The e2e seam proves the behavior end to end
  (`crates/ait/tests/e2e.rs`: the `# ` line stays in scrollback as a
  submitted comment while the suggestion types onto the fresh prompt).
