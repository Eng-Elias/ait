# Windows manual verification

Round 16 (§5.14, ADR-0020) makes `ait` and `ait-gui` native on
Windows. The round's CI-vs-manual split (Spec R16): the
`windows-latest` CI legs prove what a headless runner can — the
`windows` job builds `--workspace --all-targets` and runs
`cargo test --workspace` under x86_64-pc-windows-msvc, and
`windows-e2e` runs the PTY suite's Windows-capable scenario subset
through ConPTY (`shell-integration-pwsh`,
`shell-integration-winps`, `shell-degrade-cmd` plus the
platform-agnostic legs — unix fixtures skip loudly via the
scenario `platforms` gate). Release pipelines additionally gate on
`verify-windows` in `verify-install.yml`, which runs the published
`ait-installer.ps1` under `powershell.exe` and asserts both
binaries' `--version`.

What a headless runner cannot prove stays here. Run this checklist
on a real Windows box (Windows 10 1809+ — ConPTY's floor) on any
round touching the Windows seams: `ait_app::ipc`, `ait-shell`'s
pwsh planner/snippet, `ait-term`'s spawn chain or OSC-7 decode,
the `ait-config`/`ait-store` path seams, or `ait-gui`'s
platform arms.

## GUI legs — from the R16-T7 verification notes

- **Window/render** — run `ait-gui` (or `ait gui` from a
  `--features gui` build). A native window opens; the session UI
  renders on wgpu (D3D12 or Vulkan). Grid, tabs, side panes,
  clipboard copy/paste, selection, scrollback and typing all work.
- **WebView2 embed** — `Ctrl+Shift+B` → type `example.com` →
  Enter: the page embeds in the pane body (the WebView2 Runtime
  ships on most Win10/11 images; `Get-AppxPackage *WebView2*`
  confirms). Resize tracks; pane close hides the child.
- **External-open fallback** — on a box WITHOUT the WebView2
  Runtime (or with init forced to fail) the pane shows `the
  webview runtime failed to initialize` and Enter opens the URL in
  the system browser — the process survives, no crash. (Linux's
  webkit is a `DT_NEEDED` hard-fail at exec; Windows' WebView2 is
  a load-time init error, so this leg is the §5.13/ADR-0018 rule
  actually exercised.)
- **Explorer on Windows paths** — `Ctrl+Shift+E` roots at the
  pwsh cwd (`C:\…`); `..` at the drive root no-ops; Ctrl+click on
  a printed `C:\…` or `\\server\share\…` token navigates the
  explorer; `.` toggles `FILE_ATTRIBUTE_HIDDEN` files. There is no
  drive-letter picker by design — the pane stays rooted at the
  session cwd.

## Shell/integration legs

- **Default spawn chain** — `ait` on a box with `pwsh.exe` on PATH
  lands in PowerShell 7; remove it and the session falls to
  `powershell.exe` (5.1); `[terminal].shell` and a set `$SHELL`
  (Git/MSYS) override per §5.14's chain.
- **cmd degrade** — force `[terminal].shell = "cmd.exe"`: a plain
  working terminal, no blocks, no errors; agent `run_command`
  completes via the Absent tier's `cmd` sentinel rather than
  timing out.
- **TUI inside real hosts** — `ait` inside conhost.exe and inside
  Windows Terminal: crossterm raw mode, panic-restore, mouse and
  resize behave.
- **`ait ctl` over a named pipe** — `ait ctl` from inside a tab
  drives its own instance (`AIT_CTL_SOCKET` carries the
  `\\.\pipe\ait-<user>-<pid>` name); a second `ait` instance is
  addressed the same way. (The pipe wire test —
  `ait_app::ipc`'s `windows_pipe_binds_connects_and_round_trips_a_line` —
  runs in the `windows-latest` leg; the manual leg is the
  named-pipe ACL/discovery behavior on a real desktop.)

## Data/secrets legs

- **Directories** — settings land at `%APPDATA%\ait\settings.toml`
  (`mcp.json`, `skills/` beside it); the store at
  `%LOCALAPPDATA%\ait\store.db`; session snippet dirs under
  `%TEMP%`, removed on session end. `XDG_CONFIG_HOME` set still
  wins over `%APPDATA%`.
- **Credential Manager** — store an endpoint key, then confirm it
  surfaces in Windows Credential Manager under `ait/<endpoint-key>`
  and that `api_key_env` still takes precedence (§4.1).
- **Hot reload** — edit `settings.toml` while `ait` runs; the
  `watch` feature applies it without restart.

## Release legs

- **`.ps1` installer on a clean box** — on a machine (or VM)
  without cargo: `irm <release>/ait-installer.ps1 | iex`, then
  `ait --version` and `ait-gui --version` — the `verify-windows`
  CI leg approximates this on `windows-latest`; the manual leg is
  a truly clean image. Windows PowerShell 5.1 (the stock host) is
  the required compat floor — the verify script runs the installer
  under `powershell.exe` for exactly that reason.
