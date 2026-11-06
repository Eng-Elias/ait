# Changelog

All notable changes to AIT are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and releases use
[Semantic Versioning](https://semver.org/); the version lives once in the
root manifest's `[workspace.package]` (ADR-0015).

## [Unreleased]

## [1.0.0]

The first release of AIT: a local-first, provider-agnostic AI terminal —
a full TUI terminal emulator with an agent that reads your shell output,
suggests and runs commands, and works against any OpenAI-compatible
endpoint including local servers. Ships twelve rounds (REQUIREMENTS.md
§7):

- Terminal core (R01): a byte-fed VT emulator rendering into a styled
  cell grid, wrapped in a GPU-free ratatui TUI with tabs, scrollback and
  an input editor (R02).
- Configuration & providers (R03): `settings.toml` with layered config,
  endpoint schemas and model roles, secrets in env/keyring only, hot
  reload, `ait --init` and `ait --print-schema`.
- Natural language → command (R04): `# <task>` turns a sentence into a
  reviewed shell command against any configured provider.
- Shell integration & blocks (R05): OSC 7700 hooks for bash/zsh/fish
  give the terminal semantic command blocks and in-band agent execution.
- Agent mode (R06): a model↔tools loop that reads terminal output and
  runs commands under a permission engine (R07) of tunable execution
  profiles (`Alt+P` to switch).
- Persistence & planning (R08): a local-first SQLite store for agent
  conversations, transcripts, plans and replay.
- SSH & executor tiers (R09): `ssh()` snippet wrappers, remote sessions
  over ControlMaster and remote-aware agent tools.
- Depth features (R10–R11): an MCP client (`mcp.json`), a skills loader
  (`~/.config/ait/skills` + project `.ait/skills` roots), complete secret redaction, an input
  classifier, headless mode (`ait --headless "<task>"`) and the `ait
  ctl` control CLI.
- Packaging & release (R12): one workspace version, this changelog,
  cargo-dist release builds for Linux (gnu + musl, x86_64 + aarch64) and
  macOS (ARM + Intel), a checksum-verifying `ait-installer.sh`, and a
  man-page-style `ait --help`.
