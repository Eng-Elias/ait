# ait user guide

How to install, configure and use ait — the local-first,
provider-agnostic AI terminal. `ait --help` prints the full CLI
surface; this guide covers what lives behind it. Domain vocabulary
follows [GLOSSARY.md](../GLOSSARY.md).

## Installation

### Shell installer (recommended)

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Eng-Elias/ait/releases/latest/download/ait-installer.sh | sh
```

The installer downloads the archive for your platform, verifies its
published `.sha256` checksum before running anything, and installs
the `ait` binary under `$HOME` — no `sudo` needed. The default install
dir is `$CARGO_HOME/bin` (usually `~/.cargo/bin`); the installer adds
it to your `PATH` through your shell profile, so start a new shell or
re-source the profile after the first install.

Environment variables the installer reads:

- `AIT_INSTALL_DIR` — install somewhere other than the default dir.
- `AIT_NO_MODIFY_PATH=1` — skip the `PATH` edits entirely.

Re-running the same one-liner upgrades `ait` to the latest release.
`ait --version` prints the installed version.

### Supported platforms

Prebuilt archives ship for five targets:

- `x86_64-unknown-linux-gnu` — Linux x86_64 (glibc)
- `aarch64-unknown-linux-gnu` — Linux ARM64 (ARM servers, Raspberry
  Pi, Asahi)
- `x86_64-unknown-linux-musl` — a fully static musl build for Alpine,
  NixOS and distros with an old glibc
- `aarch64-apple-darwin` — macOS on Apple Silicon
- `x86_64-apple-darwin` — macOS on Intel

Windows is unsupported (REQUIREMENTS.md §6: Linux first, macOS
supported; bash, zsh, fish).

**musl caveat:** the shell installer cannot detect a musl system, so
on Alpine and other musl distros it picks the gnu archive, which
won't run. Download the musl archive and its checksum by hand instead:

```sh
curl -LO https://github.com/Eng-Elias/ait/releases/latest/download/ait-x86_64-unknown-linux-musl.tar.gz
curl -LO https://github.com/Eng-Elias/ait/releases/latest/download/ait-x86_64-unknown-linux-musl.tar.gz.sha256
sha256sum -c ait-x86_64-unknown-linux-musl.tar.gz.sha256
tar -xzf ait-x86_64-unknown-linux-musl.tar.gz
```

**macOS Gatekeeper:** the release binary is unsigned, so the first
launch may be blocked. Either clear the quarantine attribute:

```sh
xattr -d com.apple.quarantine /path/to/ait
```

or right-click the binary and choose *Open*.

### cargo install

With a Rust toolchain installed:

```sh
cargo install --git https://github.com/Eng-Elias/ait ait
```

ait is not published to crates.io — `--git` is the cargo path.

## Configuration

`settings.toml` lives at `$XDG_CONFIG_HOME/ait/settings.toml`,
falling back to `~/.config/ait/settings.toml`. A missing file is not
an error — ait runs on built-in defaults.

- `ait --init` writes a commented default file and prints the path it
  wrote. It refuses to overwrite an existing file;
  `ait --init --force` is the overwrite form.
- `ait --print-schema` prints the settings file's JSON Schema —
  point an editor at it for completion and validation.
- An invalid file fails loudly: load errors name the offending TOML
  key path (`agents.custom_endpoints.local.models[0].config_key: …`).

**Hot reload:** ait watches the file and reloads on save; a bad save
keeps the last good settings armed. `[terminal]` values (`shell`,
`scrollback_lines`, `osc52`, `shell_integration`) apply to new
sessions only — live sessions keep their spawn-time values.

The file's tables:

| Table | Holds |
|---|---|
| `[terminal]` | shell, scrollback, `osc52`, `shell_integration` |
| `[agents]` | `custom_endpoints`, `execution_profiles` — next two sections |
| `[ssh]` | SSH-integration policy — see [SSH](#ssh) |
| `[redaction]` | named patterns masking secrets before output reaches a provider |
| `[input]` | `classifier` — `heuristic` (default) or `off` |

### The `.ait/` project layer

Beside the user config, ait reads a project-local layer: the nearest
`.ait/` directory walking up from a session's working directory —
the same nearest-wins walk `AGENTS.md` discovery uses.

- `mcp.json` — MCP stdio server declarations, shaped
  `{"mcpServers": {"name": {"command", "args", "env"}}}`. The user
  layer is `$XDG_CONFIG_HOME/ait/mcp.json`. A project entry shadows a
  same-named user entry, and project-layer servers are listed in the
  catalog but never spawned eagerly — they only connect through a
  gated tool call, so a cloned repo can't run code on your machine
  silently.
- `skills/` — directories of `<name>/SKILL.md` skills the agent can
  list and invoke. The user root is
  `$XDG_CONFIG_HOME/ait/skills/`; a project skill shadows a
  same-named user skill.

## Providers

An endpoint is any OpenAI- or Anthropic-compatible LLM server —
including local servers like Ollama or LM Studio — declared under
`[agents.custom_endpoints.<key>]`:

```toml
[agents.custom_endpoints.local]
name        = "Local Ollama"
base_url    = "http://localhost:11434/v1"
schema      = "openai_chat_completions"
api_key_env = "OLLAMA_API_KEY"   # optional

[[agents.custom_endpoints.local.models]]
name       = "qwen2.5-coder:14b"      # model id sent on the wire
alias      = "Qwen Coder"             # display name (status line)
config_key = "6f1c…-uuid4"            # stable id model roles reference
# context_window = 32768              # optional cap; unset = the model's own limit
```

- `schema` picks the wire format — one of `openai_chat_completions`,
  `openai_responses` or `anthropic_messages`.
- `config_key` must be non-empty and unique across every endpoint's
  models. Model roles reference it, so renaming a model's `name` or
  `alias` never breaks references.

**Model roles** are set per execution profile: `base_model` (the
fallback everything resolves through), `coding_model` and
`command_model` (the NL→command model). Each names a `config_key`;
an unset role (`""`) falls back to `base_model`, and referencing an
undeclared `config_key` is a load error naming the role.

**Secrets never live in `settings.toml`.** An endpoint's API key comes
from the environment variable `api_key_env` names, or — when that is
unset — the OS keyring entry `ait/<endpoint-key>` (e.g. `ait/local`),
stored with your platform's tools. With neither present, requests go
out with no `Authorization` header — the right shape for
unauthenticated local servers.

## Execution profiles

An execution profile is a named set of per-tool permission levels plus
allow/deny lists, declared under
`[agents.execution_profiles.<name>]`. `ait --init`'s file ships a
commented `default` profile as the safe starting point.

```toml
[agents.execution_profiles.default]
read_files        = "agent_decides"
write_to_pty      = "always_ask"
execute_commands  = "always_ask"
apply_code_diffs  = "always_ask"
mcp_permissions   = "always_ask"
ask_user_question = "always_allow"
command_allowlist   = []
command_denylist    = ['^rm(\s.*)?$']
directory_allowlist = []
mcp_allowlist = []
mcp_denylist  = []
base_model    = "…config_key…"
coding_model  = ""   # falls back to base_model
command_model = ""   # falls back to base_model
context_window_limit = 0   # 0 = the model's own limit
```

**Permission levels** — what each per-tool key accepts:

- `always_allow` — never ask
- `always_ask` — ask every time (the approve card offers *allow once*,
  *allow this tool for the rest of the task*, or *deny*)
- `agent_decides` — the model flags the call safe/unsafe; unsafe falls
  back to asking
- `ask_on_first_write` — ask on the first write to the PTY, then
  allow; meaningful on `write_to_pty`
- `never` — never permit

**The tool keys:** `read_files`, `write_to_pty` (the agent-owned or
user PTY), `execute_commands`, `apply_code_diffs`, `mcp_permissions`
and `ask_user_question`.

**The lists:** `command_allowlist`/`command_denylist` are regexes
matched against the full command — denylist beats allowlist.
`directory_allowlist` restricts file access to listed directories;
`mcp_allowlist`/`mcp_denylist` always allow or deny MCP servers/tools.

**Switching:** `default` is armed at launch. `Alt+P` switches the
active profile — a picker over three or more declared profiles (↑/↓,
Enter, Esc), a direct cycle between two — and the status line shows
the profile's name whenever it isn't `default`. The selection is
ephemeral: it is never written back to `settings.toml`.

## SSH

Inside an integrated session, `ssh` becomes a shell function the
rc snippet defines. An interactive `ssh <dest>` to an allowed host
is rewritten so the remote shell gets bootstrapped with the same
integration your local one has — a self-contained POSIX-sh script
that lands the snippet files in a private remote `mktemp -d` and
never touches a remote dotfile. The status line marks the tab
`ssh:<target>` while the remote session is open, and a nested `ssh`
inside it bootstraps one hop further.

Everything else stays a plain `command ssh`: `ssh host <command>`
forms, control/config operations, denied hosts — and `command ssh`
typed directly always bypasses the wrapper. `AIT_SSH_OFF=1` opts a
session out per-session.

The `[ssh]` table sets the policy:

- `enable_integration` — the global off switch (default: on);
  `false` makes `ssh` a plain command in every new session
- `hosts_allowlist` — OpenSSH `Host`-pattern globs on the destination
  token (`*`, `?`, a leading `!` negates); empty means open
- `hosts_denylist` — a matching `ssh` stays a plain passthrough,
  never blocked; deny beats allow
- `reuse_existing_control_master` — honor your own `ControlMaster`/
  `ControlPath` configuration instead of attaching a parallel socket
  (default: on)

When your own ssh config doesn't already multiplex the host, the
wrapper attaches ControlMaster options (`ControlMaster=auto`, a
`ControlPath`, `ControlPersist`) to the connection. That socket is
what the ControlMaster executor tier multiplexes the agent's
`run_command` over — out-of-band, so agent commands on a remote
session never interleave with your typing in the live shell.

## Headless mode and the control CLI

### `ait --headless "<task>" [--profile <name>]`

Runs one agent task non-interactively: a hidden, fully-integrated
shell, the profile `--profile` names (the launch default when
omitted). Assistant text streams to stdout — pipe-safe — while tool
calls and status activity go to stderr. Approval gates are
auto-denied and `ask_user_question` gets a canned no-interactive
answer, so a run never parks on a prompt nobody can see.

Exit codes: `0` when the task completes, `1` on error, `2` when the
step budget is reached, `130` on Ctrl+C (SIGINT is the headless
`Esc`).

### `ait ctl <verb>`

Drives a running ait instance over its control socket — a
per-instance Unix socket at `${XDG_RUNTIME_DIR}/ait/<pid>.sock` (or
`$TMPDIR/ait-<uid>`). `AIT_CTL_SOCKET` — exported into every session
ait spawns — names the socket; with exactly one running instance
`ait ctl` resolves it on its own, and with several it fails naming
the candidates so you can set `AIT_CTL_SOCKET` to pick one.

```text
ait ctl open-tab
ait ctl send [--tab <id>] [--no-enter] <text>
ait ctl last-block [--tab <id>] [--command|--exit]
```

- `open-tab` — allocate a fresh tab; prints the response's JSON line
  (`{"ok":true,"session_id":"…"}` — compose with `jq`).
- `send` — write `<text>` into the target tab and press Enter;
  `--no-enter` leaves the text on the input line verbatim. `<id>` is
  the tab's `AIT_SESSION_ID`; absent means the active tab.
- `last-block` — print the newest *sealed* block's output (default),
  its command with `--command`, or its exit code with `--exit`.

Every failure prints `ait ctl: <message>` on stderr and exits
non-zero.
