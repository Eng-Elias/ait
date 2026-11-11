# 14. The NL-verdict submission rewrite and the `ait ctl` socket surface

Date: 2026-11-05

## Status

Accepted

## Context

Round 11 (Spec R11, REQUIREMENTS.md §5.10.4–5) adds two user-visible
contracts that are hard to reverse once relied on:

1. **What a natural-language verdict does to the submission.** Once
   `classify` reads the verified input line as natural language, ait
   could (a) swallow the Enter and erase the line like `Alt+Enter`
   does, keeping the request out of the shell entirely; (b) forward
   the Enter and let the shell fail the sentence; or (c) rewrite the
   submission so the request still lands in shell history. The choice
   decides what `Up`-history recall shows, what a heuristic misfire
   costs, and whether the comment survives a cancelled suggestion.
2. **How `ait ctl` reaches a running instance.** Scripted control of
   the TUI needs a transport — a per-instance Unix socket, a TCP port,
   or a shared well-known socket — plus the trust boundary that
   decides who may drive the terminal, and what staged input does to
   the input triggers.

## Decision

- **A `NaturalLanguage` verdict rewrites the submission, it does not
  swallow it.** On a verified line the submission arm swallows the
  user’s Enter and writes `Ctrl+U` (`\x15`, readline’s
  kill-to-start-of-line) then `# <line>` + `\r` to the PTY: the
  request lands in shell history and scrollback as a comment —
  ADR-0007 semantics, identical to an explicit `# ` request — the
  shell draws a fresh prompt, and the armed `SuggestRequest` (the
  `Alt+Enter` whole-line form) streams the suggestion onto it. The
  rewrite goes out only *after* `fire` accepts the request — the
  erase-after-fire ordering `suggest_from_line` established — so a
  dead suggester leaves an ordinary verbatim submission, never an
  erased line. `Command` verdicts, `classifier = "off"`, stale
  shadows and echo disagreement all degrade to verbatim; `Up`-recall
  of the comment resubmits through the explicit `# ` path, so a retry
  never needs the heuristic to agree twice.

- **`ait ctl` speaks single-line JSON over a per-instance Unix
  socket.** The server binds
  `${XDG_RUNTIME_DIR}/ait/<pid>.sock` (parent dir `0700`, socket
  `0600`, unlinked on clean exit, re-bound over stale files) and is
  drained nonblocking on the app tick; the client resolves
  `AIT_CTL_SOCKET` (exported into spawned sessions, so `ait ctl` in a
  tab drives its own instance), else the single socket under the
  directory, else errors — zero sockets is "no running ait
  instance", several is a listing. Same-uid filesystem permissions
  are the whole trust boundary this round — no auth token, no TCP.
  `send` writes bytes verbatim and marks the tab’s `InputShadow`
  stale: scripted input never passed through keystrokes, so it can
  never arm the `# ` or classifier triggers against a line the user
  is also typing (desync-is-inert).

## Consequences

- A classified request is indistinguishable from an explicit `# `
  one: history shows `# <line>`, Esc cancels the stream and leaves
  the comment behind, and a misfire costs a comment entry — never a
  swallowed line or a wrong request.
- The `# ` rewrite depends on the shell treating `Ctrl+U` as a line
  kill — true for bash/zsh/fish readlines and for canonical-mode
  reads — so the trigger only arms when the tab's program is a
  known shell (`ait_shell::is_known_shell`, keyed on the tab title
  like the `ProgramIndex`): a REPL or other echo-only program
  degrades to a verbatim submission rather than risking `# <line>`
  appended onto an un-wiped line.
- The rewrite preserves request text exactly — `# ` + the verified
  line — so a misfired classification is fully visible in history
  rather than silently discarded.
- Classification consults the *local* `$PATH`; a remote-only binary
  name can misclassify. Accepted for the heuristic round — the `# `
  escape is always available.
- The ctl trust boundary is filesystem permissions: anything running
  as the user can drive the instance. A wider surface (TCP, a shared
  socket, an auth token) is a deliberate non-goal this round.
