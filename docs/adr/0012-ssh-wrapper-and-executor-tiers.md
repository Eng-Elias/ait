# 12. The ssh() wrapper and the ControlMaster executor tier

Date: 2026-12-03

## Status

Accepted

## Context

Round 9 (Spec R09, REQUIREMENTS.md §5.9) carried the round's core
motivation: `ssh prod` typed inside an integrated `ait` tab discarded
everything §5.5 gave the user — prompt marks, command blocks, in-band
execution, status context — for as long as the session lasted. Two
decisions were hard to reverse and worth recording:

1. **How to detect and instrument `ssh`.** Alternatives considered in
   the grill: typing a bootstrap line into the channel after connect
   (stdin injection), shipping an `ait ssh` subcommand users opt
   into, or wrapping `ssh` in the injected snippets. Stdin injection
   races password/key auth prompts — the first bytes can be consumed
   as credentials — and can never attach ControlMaster options to the
   user's own connection. A separate subcommand fails silently the
   first time a user types plain `ssh` out of habit.
2. **Which executor tier serves remote sessions.** §5.9 lists four;
   the trait (ADR-0009) was designed so they slot in without
   re-architecture. The question was which to build first and how a
   failed one degrades. The remote-daemon tier was deferred to
   Backlog R09 — its upload/cache/protocol surface is a round of its
   own — leaving ControlMaster versus in-band-only.

## Decision

- **The injected snippets own an `ssh()` shell function** — bash,
  zsh and fish — that is both the detection and the interception
  seam. A bare interactive `ssh [options] <destination>` the armed
  `[ssh]` policy allows is rewritten to
  `ssh -t <args> '<remote bootstrap script>'`, optionally carrying
  `-o ControlMaster=auto -o ControlPath=<ait control dir>/%C -o
  ControlPersist=<dur>` so the already-authenticated session becomes
  the master. Every other shape — remote-command invocations,
  control/config operations, denied hosts, `AIT_SSH_OFF=1` — passes
  through to `command ssh` untouched: ait never blocks the user's
  `ssh`, it only declines to wrap it.

- **The remote command is a self-contained POSIX-sh script**
  (`ait_shell::bootstrap_script`) that writes the same
  `Injection::files` set into a private `mktemp -d` (0700), exports
  the session env (`AIT_SESSION_ID`, `AIT_IS_SSH=1`, the armed
  `AIT_SSH_*` policy so nested hops re-wrap identically), traps
  EXIT/HUP to emit `exit_shell` and remove the dir, then runs the
  remote `$SHELL` as a child — `--rcfile`/`ZDOTDIR`/`--init-command`
  per family, plain `-l` fallback for anything else. **No remote
  dotfile is touched.** The child runs rather than `exec` because
  `exec` would destroy the script's traps; the remote `HUP` trap
  covers the severed-connection case `ssh_end` alone can't (the
  remote `exit_shell` is lost when the link dies).

- **Two new hook names bracket the session** — `ssh_begin` before the
  connect, `ssh_end` after `ssh` returns (ADR-0008 amendment). The
  frontend folds vetted ones into a per-tab `RemoteContext`: the
  `ssh:<target>` status segment, the bridge's `remote_state()`, and
  `blocks.host` all read it. An ait-managed control socket is retired
  best-effort (`ssh -S <sock> -O exit`) when the context closes;
  `ControlPersist` covers whatever outlives that.

- **`select_executor` picks the tier per task**: remote + a live
  control socket (`ssh -S <sock> -O check`, probed once at task
  start) → `ControlMasterExecutor` wrapped in `DemotingExecutor`;
  otherwise `UserShellExecutor` — whose in-band `__ait_run` reaches
  the bootstrapped remote shell identically — and `DetachedExecutor`
  with no bridge. A transport failure (`ssh`'s own 255 band) demotes
  the task to the in-band tier permanently: **demote, never hang.**

- **Remote read tools ride the executor**: on a `Remote` task,
  `read_files`/`grep`/`glob` compose remote commands through the same
  `CommandExecutor` `run_command` uses, with `ToolCwd`'s path
  confinement applied lexically before composition. `edit_files`
  reports "remote editing unsupported" honestly rather than writing
  through an unauthenticated channel.

## Consequences

- The wire vocabulary grew: `HookName::SshBegin`/`SshEnd`, the
  `AIT_SSH_*` env names and `AIT_IS_SSH` are the contract between the
  spawn-time renderer, the `ssh()` wrapper and the remote bootstrap —
  all defined once in `ait-shell` (standard 17). ADR-0008's hook list
  is amended in place.
- `RemoteState` joins `ait-agent`'s bridge vocabulary — the same
  standard-17 seam `ShellIntegration` established — so the future
  daemon tier reads the same `control_path`/`target` state without a
  new interface.
- Wrapping means the user's `ssh` invocation changes argv in ways
  `ssh -v` and process listings reveal — accepted, documented, and
  the price of transparency; the passthrough cases are the escape
  hatch and `AIT_SSH_OFF=1` the per-session kill switch.
- `ssh()`'s argv rewrite only covers the bare-interactive shape:
  `ssh host cmd`, `scp`, `sftp`, `mosh`, `sudo ssh`, tmux-nested ssh
  all pass through unchanged — the spec's accepted out-of-scope.
- `blocks.host` stamped at seal time means a block records the
  session it ran under; blocks sealed before `ssh_begin` stays NULL.
- `REQUIREMENTS.md` §5.9 gains the decided mechanism notes and §4.3's
  `[ssh]` row stops being proposed.
