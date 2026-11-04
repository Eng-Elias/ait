# 10. Execution-profile permission semantics

Date: 2026-11-04

## Status

Accepted

## Context

`settings.toml` has carried the §5.7 `ExecutionProfile` fields —
per-tool permission levels plus command/directory allow and deny
lists — as parsed-but-inert config since Round 3. Round 7 (Spec R07)
made them a real permission engine inside `ait-agent`: every tool
call now evaluates the active profile through a pure verdict seam
(`allow` / `ask` / `deny`) at the consult point Round 6's interim
gate left for it. With the engine live, users can finally *write*
configs — which is exactly when the semantics those configs mean must
stop being ticket text and become a ratified record: they are hard to
reverse (every settings file in the wild encodes them), surprising
without context (the `agent_decides` trust model especially), and
genuinely trade-off-laden.

The questions the file format alone did not answer:

1. **When several rules could fire on one call, which wins?** A
   `never` level, a denylist match, an allowlist match, the mapped
   level and an *allow for task* memory can all address the same
   `run_command`.
2. **How do list entries match?** Anchored or unanchored, against
   which argument, and who supplies the anchors?
3. **What does `agent_decides` delegate, and to whom?** The level
   exists to let routine calls flow while the model flags its own
   risky ones — but the grader is the same model being gated.
4. **Does `never` remove the tool?** Dropping it from the declared
   registry would hide the capability; denying the call keeps it.
5. **Is `directory_allowlist` a verdict or a confinement input?**
   `command_*` lists feed the verdict; the directory list could have
   meant either.
6. **What governs a call when the active profile resolves to
   nothing?** A reload can delete or rename the armed profile
   mid-session.

## Decision

- **The evaluation order is `never` → `command_denylist` →
  `command_allowlist` → level.** `never` refuses before anything else
  consults — the hard floor no list entry, verdict memory or `safety`
  flag crosses. For `run_command` alone the denylist then the
  allowlist search the verbatim `command` argument; a denylist hit
  denies, an allowlist hit allows, and only then does the mapped
  level decide (`always_allow` allows; `agent_decides` runs the flag
  check below; `ask_on_first_write` asks once, an allow covering the
  tool for the task; `always_ask`, unset fields and unmapped tools
  ask plainly). `deny` produces the tool result
  `"denied by execution profile <name>"` and a `Denied` card —
  byte-for-byte the shape of a user denial, so the model reroutes and
  can say why.

- **Command-list entries are unanchored regexes over the verbatim
  `command` argument** — `regex::is_match` search semantics, so an
  entry's own `^`/`$` decide where it binds and the author writes the
  anchors they mean (the shipped default denylist anchors its own
  entries). The lists govern `run_command` only — never `write_to_pty`
  keystrokes — and a malformed entry fails `Settings` validation at
  load naming its key path (`agents.execution_profiles.<name>.
  command_denylist[i]`), matching every other bad key under ADR-0006.

- **`agent_decides` trusts the model's declared self-assessment** —
  a required `safety` argument injected into the governed tools'
  parameter schemas at declaration time; `"safe"` allows, and
  `"unsafe"`, an absent flag or a malformed one asks. The trade-off is
  named, not hidden: *the model being gated grades its own call* — a
  jailbroken or careless model could flag everything `safe`, so
  `agent_decides` is a convenience level, not a sandbox. It is
  accepted because the flag is opt-in (everything but an explicit
  `safe` still asks), honest (it rides the call's own arguments —
  visible on the card and recorded in the transcript, never a side
  channel), and the stronger alternative — a second model grading
  calls — is a different feature entirely. Users who need stronger
  guarantees write `always_ask`, `never` or denylist entries.

- **`never` declares-but-denies.** The tool stays in the registry;
  its calls return the profile denial so the model learns the refusal
  reason and can tell the user which profile to switch. Removing the
  tool instead would hide the capability entirely — the model would
  not know to ask.

- **`directory_allowlist` is additive confinement, not a verdict.**
  The file tools' roots become the session's cwd ∪ the allowlist's
  normalized roots: entries *grant* reach beyond cwd, escapes outside
  the union stay tool errors, and an empty list keeps cwd-only
  confinement. Unlike the command lists it never produces an allow or
  a deny — the engine's verdicts are unchanged by it.

- **A missing active profile asks everything.** A name that resolves
  to nothing — deleted or renamed by a reload, never declared, or
  zero profiles declared — evaluates every tool as `always_ask`: the
  Round 6 interim gate verbatim. The armed name is never silently
  rewritten, and the frontend toasts it
  (`execution profile "<name>" no longer declared`), so a config edit
  can neither loosen nor lock the agent without saying so.

## Consequences

- Config authors can rely on the floor: `never` and the denylist out
  everything — a permissive stance can never be crossed by its own
  lists, the `safety` flag, or an *allow for task* memory.
- `agent_decides`' security posture is now on record: it trades a
  weaker trust boundary for uninterrupted routine work, and the
  card's visible `safety` argument keeps every self-assessment
  auditable.
- `mcp_permissions`, `mcp_allowlist`, `mcp_denylist` and
  `context_window_limit` remain declared-but-inert — Round 10's MCP
  tools and Round 8's summarization inherit this order when they
  land, but nothing here preempts their decisions.
- `REQUIREMENTS.md` needed no change: §5.7's "denylist beats
  allowlist" is refined, not contradicted — this ADR records the
  full order, the unanchored matching and the `never`/missing-profile
  semantics the requirement's one line left implicit.
