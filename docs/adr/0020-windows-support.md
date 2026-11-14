# 20. Windows support: named-pipe IPC, a single PowerShell snippet, and the XDG→%APPDATA% directory mapping

Date: 2026-11-14

## Status

Accepted

## Context

Round 16 (§5.14, Spec R16) takes `ait` and `ait-gui` native on
Windows. Three design axes were settled in the grilling, plus a
fourth that the scenario-matrix work surfaced mid-round:

1. **What carries the local IPC on Windows.** The ctl socket
   (ADR-0014) and the R15 test-driver socket are `UnixListener`s —
   a transport Windows doesn't have. The options: named pipes
   (`\\.\pipe\…`, the platform's same-machine IPC), a TCP loopback
   listener, or shipping Windows with the ctl/test-driver surface
   off.
2. **How many PowerShell integrations to ship.** §5.14 covers both
   `pwsh` 7 and the Windows PowerShell 5.1 every box ships — their
   prompt/PSReadLine surfaces differ (5.1's PSReadLine 2.0 API is
   the older one). One snippet with runtime feature gates, or two
   snippets selected at injection time.
3. **Where config, secrets, the store and runtime dirs live.** The
   codebase's XDG conventions (`$XDG_CONFIG_HOME`/`$XDG_DATA_HOME`/
   `$XDG_RUNTIME_DIR`, `0700` dirs) have no Windows counterpart —
   the platform's own vocabulary is `%APPDATA%` (roaming,
   per-user), `%LOCALAPPDATA%` (machine-local) and `%TEMP%`
   (per-user ACL'd scratch).
4. **How the scenario suite gates per-OS legs.** The testkit's TOML
   scenarios hardcode Unix fixtures (`SHELL=/bin/bash`, `kill`
   probes, ssh); Windows legs (pwsh, WinPS 5.1, cmd degrade) join
   the same matrix. Options: a per-scenario platform declaration,
   `#[cfg]` splits in the runner code, or parallel scenario dirs.

## Decision

- **Named pipes carry local IPC on Windows — one transport
  abstraction, identical wire.** `interprocess` 2.4.7's
  local-socket API sits behind a new `ait_app::ipc` module: the
  `UnixListener` path on Unix (unchanged wire, unchanged
  `0600`/parent-dir rules) and `\\.\pipe\ait-<user>-<pid>` for ctl
  / `ait-driver-<user>-<pid>-<seq>` for the test driver on Windows
  (the `-<seq>` tail is the harness's per-spawn counter — parallel
  GUI legs in one test process never collide), where the pipe's
  default same-user ACL is the `0600` equivalent.
  One-request-per-connection, single-line JSON, buffered accept —
  the codec can't tell which side it's on.
  `ENV_CTL_SOCKET`/`ENV_DRIVER_SOCKET` carry the pipe name instead
  of a path. `interprocess` is pinned `=2.4.7` with the ADR-0002
  version+date comment; `ait-testkit` resolves the same line for
  its driver client.
- **One `pwsh.ps1` snippet serves both PowerShell hosts.** Authored
  in-house beside bash/zsh/fish and embedded via `include_str!`
  (materialized into the session dir as `snippet.ps1`), it
  feature-gates every piece at load: `$PSVersionTable` and
  PSReadLine presence decide whether the session gets the full
  surface (prompt marks + hook events + `preexec` via a
  `Set-PSReadLineKeyHandler Enter` wrapper reading
  `GetBufferState`) or degrades to marks+hooks (the MarksOnly
  executor tier) or plain shell. `init_shell` emits at load;
  `bootstrapped` is the re-source guard's reply and the `ssh()`
  wrapper's post-return re-integration — the same split the Unix
  snippets run. `precmd`/OSC 133 `D`-with-`$LASTEXITCODE`/OSC 7/`A`
  come from the overridden `prompt` function, `preexec`/`B`/`C`
  from the Enter wrapper, and `exit_shell` via `Register-EngineEvent
  PowerShell.Exiting` — clean-quit only, the same limit the Unix
  trap has. The injection planner maps basenames `pwsh` and
  `powershell` (`.exe`-stripped, case-insensitive on Windows) to
  `ShellKind::Pwsh` — the `.exe`-stripped, case-insensitive match
  runs on every OS (only Windows spellings ever exercise it);
  spawn args are `-NoExit -ExecutionPolicy
  Bypass -Command ". '<sessiondir>\snippet.ps1'"` — profiles load
  first, `Bypass` is scoped to the one spawned process.
  `__ait_run`'s payload runs as a `[ScriptBlock]` inside the
  function's child scope — an honest narrowing of the spec's
  "session scope" wording, which literal PowerShell cannot give a
  function: payload-defined variables and functions die with the
  call, while cwd (`Set-Location`) persists, location being
  session-global rather than scope-bound. For `run_command`
  payloads — command lines, not session setup — that is the
  "user's own shell" guarantee that matters.
- **Windows directories map behind the existing path seams — no
  new seams.** `XDG_CONFIG_HOME` → `%APPDATA%\ait` (settings,
  `mcp.json`, skills), `XDG_DATA_HOME` → `%LOCALAPPDATA%\ait`
  (store.db — machine-local stays local), session snippet dirs and
  runtime-dir fallbacks → `std::env::temp_dir()` (already per-user
  ACL'd, so the `0700` bit is a no-op there). `XDG_CONFIG_HOME`
  set on Windows is still honored first — an explicit env wins
  over the platform default. Secrets need no mapping: `keyring`
  resolves to Windows Credential Manager on its own.
- **Scenario TOMLs declare a `platforms` gate, defaulting to
  all.** Unix fixtures skip loudly on Windows rather than
  failing; `pwsh`/`powershell`/`cmd` legs join the same files.
  Runner code carries no per-OS branching — the gate is data.
- **Rejected.** TCP loopback for IPC (a listening port invites
  firewall prompts and cross-user probing a pipe's ACL answers by
  default — and it would fork the wire surface); ctl-off on
  Windows (the §5.10.5 control surface is part of the product —
  US-24/25 want `ait ctl` working); two PowerShell snippets
  (divergence risk for zero gain — the differences are three
  feature gates, not two programs); a `KnownFolders`-style
  per-entry mapping table (three roots cover every consumer); a
  `platforms` default of "unix" (would silently pin every
  forgotten new scenario to one OS).

## Consequences

- The ConPTY OSC-passthrough spike (R16-T1) proved the round's one
  unportable assumption before the snippet work landed: `ESC]770x`
  and OSC 133/7 emitted by a ConPTY child arrive at
  `SessionEvent::Private`/`PromptMark` intact.
- R14 had already pre-landed the Windows arms in `ait-gui` — the
  `Win32`/`WinRT` raw-handle mapping, the `cmd /c start ""` opener
  and `cmd /c` shell-eval, WebView2-init-failure → external-open
  (the §5.13/ADR-0018 rule *extended*, since a missing runtime is
  a load-time error, not a `DT_NEEDED`). T7 finished them
  (Windows path grammar in `detect`, OSC 7 `file:///X:` decode,
  `FILE_ATTRIBUTE_HIDDEN`); this ADR is where those comments'
  "§5.14's platform, not yet shipped" becomes shipped.
- `x86_64-pc-windows-msvc` joins both dist target lists and
  `installers` gains `"powershell"` — `ait-installer.ps1` /
  `ait-gui-installer.ps1` beside the `.sh` twins, `.zip` archives
  by dist's Windows default, and a `verify-windows` leg on
  `windows-latest` in `verify-install.yml` (hand-owned;
  `release.yml` itself needed no edit — its build matrix is fully
  dynamic from the plan output). `aarch64-pc-windows-msvc` is
  deliberately skipped: §5.14 marks it optional, there's no free
  native runner, and nothing could test the artifact.
- CI carries a `windows-latest` leg (workspace build + tests) and
  a Windows e2e matrix — both PowerShells and `cmd.exe` ship on
  the runner. What CI cannot prove stays manual and is recorded
  in `docs/windows-verification.md`: the `ait-gui` window/wgpu
  render, WebView2 embed + missing-runtime fallback, Credential
  Manager lookup on a real desktop, the TUI inside conhost and
  Windows Terminal, and the `.ps1` installer on a clean box.
- Unix-only tests were `cfg`-gated, never deleted — the Linux/
  macOS coverage is untouched; `cargo test --workspace` is green
  on all three CI OS legs.
