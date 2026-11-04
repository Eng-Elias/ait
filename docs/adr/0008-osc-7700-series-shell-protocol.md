# 8. The OSC 7700-series shell-integration protocol

Date: 2026-10-31

## Status

Accepted

## Context

REQUIREMENTS.md §5.5 sketched an AIT-private OSC family — `7700` for
shell hook events, `7701` for in-band `__ait_run` responses, `7702` and
`7703` for native completions and a grid reset — marked *Proposed*
pending Round 5's implementation. Round 5 (Spec R05) built it: the
rc snippets in `ait-shell` emit the hooks, `ait` decodes them, and the
in-band exec seam round-trips through `OSC 7701`. Shipping forced three
amendments to the proposed `7701` grammar, so the protocol needs a
ratified record before anything else builds on it — §5.5.1's executor
tiers (Round 6) and §5.5.2's completions both consume these codes.

## Decision

The `770x` family is ratified as implemented by `ait-shell` (`codec.rs`
decodes; `run.rs` and the snippets emit). `ESC` below means `0x1b`;
payloads may equally terminate on `ST` — the decoder only sees code +
params.

### 7700 — hook events: confirmed as proposed

```text
ESC ] 7700 ; <hex(json)> BEL
json = {"hook":"<name>","value":{…}}
hooks: init_shell | bootstrapped | precmd | preexec | exit_shell
         | ssh_begin | ssh_end        (amended, Round 9 / ADR-0012)
```

Exactly one parameter — a hex-encoded JSON object carrying `"hook"`
and `"value"`. `init_shell` fires once when the snippet is sourced
(`value.shell` names the family), `bootstrapped` on re-source of an
already-integrated shell (remote/nested ready, consumed in Round 9),
`precmd` before each prompt, `preexec` before each command
(`value.command` is the typed text), `exit_shell` from the shell's
exit trap. Every `value` echoes `session_id`, the `AIT_SESSION_ID`
environment ait exported at spawn (`null` when unset). Unknown hook
names decode forward-compatibly rather than failing.

### 7701 — `__ait_run` responses: confirmed with amendments

```text
ait → PTY input : __ait_run <id> <hex(command)>
shell → output  : ESC ] 7701 ; A BEL
                  ESC ] 7701 ; R ; <len> ; <hex> BEL        (repeated)
                  ESC ] 7701 ; B BEL
```

Amendments against the proposed text:

1. **The command argument is hex-encoded**, not shell-quoted:
   `__ait_run <id> <hex(command)>` — quoting edge cases (embedded
   quotes, newlines, `$`) vanish at the request layer just as they did
   for payloads.
2. **`R` frames are repeatable**, not a singleton: a response is any
   number of `R` frames between `A` and `B`. The snippets emit 500
   decoded bytes per frame (1000 hex chars), so every *complete* OSC
   stays under ~1024 raw bytes — the `vte` `MAX_OSC_RAW` budget even
   `no_std` parsers honor (`OSC_RAW_BUDGET` in `ait-shell`).
3. **`<len>` is the decoded byte length of *that* frame's payload** —
   a per-frame length, checked against the decoded hex during decode;
   a mismatch makes the frame malformed.

`A` and `B` carry no `<id>`: the wire serializes runs anyway (an
interactive shell runs one prompt line at a time), and the id travels
inside the reassembled payload — `<id>;<merged stdout+stderr>;
<exit_code>`, split on the *first* and *last* `;` so output containing
semicolons survives. `7701` traffic never renders in the grid; the
typed `__ait_run` line stays visible.

**Amendment (Round 9, ADR-0012):** `ssh_begin` and `ssh_end` joined the
hook vocabulary. The `ssh()` wrapper emits `ssh_begin` before the wrapped
connect (`value.target` is the destination token, `value.bootstrapped`
whether the remote command integrates, `value.control_path` the negotiated
ControlPath when one exists) and `ssh_end` after `ssh` returns — the
frontend folds both into the per-tab remote context (§5.9).

### 7702 / 7703 — reserved, unimplemented

`7702` (shell-native completions, §5.5.2) and `7703` (grid reset) keep
their numbers but nothing emits or consumes them this round. The codec
recognizes the whole `7702..=7709` range as reserved — a well-formed
sequence decodes to `ShellEvent::Reserved` so future traffic is never
mistaken for garbage; `7704..=7709` are unassigned. A `Backlog` issue
tracks the completions work.

### Failure contract

Malformed, truncated, foreign or over-long sequences decode to `None`,
never panic and never move state — the graceful-degrade rule of §5.5
applies at the wire layer too.

## Consequences

- `REQUIREMENTS.md` §5.5 loses its "Proposed" wording and gains the
  three `7701` amendments (hex command arg, repeatable `R` frames,
  per-frame `<len>`); §5.5.2 stays marked unimplemented.
- The `7702`/`7703` reservations are fixed publicly: any future
  completion or grid-reset design uses these codes or a new ADR.
- `R` frames carry no id, so the wire supports exactly one in-flight
  run per shell at a time — `ait`'s `InBandExec` serializes requests;
  callers that parallelize must queue. Accepted: interactive prompts
  serialize naturally, and ids still resolve waiters out of the
  reassembled payload.
- Snippets cap `R` frames at 500 decoded bytes; decoders must not
  assume a bound on total payload size — the accumulator concatenates
  until `B` with no fixed limit.
