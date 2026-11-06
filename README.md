# ait

AIT is a local-first, provider-agnostic, SSH-native AI terminal written in
Rust: a terminal you can run locally, inside tmux, or on a remote host over
SSH, that understands natural language, can act as a tool-using agent inside
your real shell, and talks to any OpenAI- or Anthropic-compatible LLM
endpoint you configure — including local servers like Ollama or LM Studio.
No account, no cloud dependency, no vendor lock-in.

See [REQUIREMENTS.md](REQUIREMENTS.md) for what we're building and the
round-by-round roadmap.

## Install

Shell installer (Linux and macOS — no Rust toolchain needed):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://github.com/Eng-Elias/ait/releases/latest/download/ait-installer.sh | sh
```

The installer verifies the published `.sha256` checksum before running
anything, installs `ait` under `$HOME` (no `sudo`), and re-running it
upgrades to the latest release.

From a Rust toolchain, install from source instead:

```sh
cargo install --git https://github.com/Eng-Elias/ait ait
```

### Supported platforms

Prebuilt archives ship for five targets: Linux `x86_64` and `aarch64`
(glibc), Linux `x86_64` as a fully static musl tarball, and macOS on
Apple Silicon (`aarch64-apple-darwin`) and Intel
(`x86_64-apple-darwin`). Windows is unsupported (§6: Linux first,
macOS supported; bash, zsh, fish). See the
[user guide](docs/user-guide.md#installation) for the musl caveat and
the macOS Gatekeeper workaround for the unsigned binary.

## Quickstart

```sh
ait --init   # writes a commented default settings.toml
```

Point `base_model` (under `[agents.execution_profiles.default]`) at
the `config_key` of a model declared under
`[[agents.custom_endpoints.*.models]]`, whose endpoint's `base_url`
and `schema` reach your provider — the generated file ships a working
local-Ollama example — then run `ait`. Type a natural-language request on the prompt to get a
suggested command, or press `Alt+A` to open the agent panel and start a
task.

## Documentation

- [User guide](docs/user-guide.md) — installation, `settings.toml`,
  providers, execution profiles, SSH, headless mode and `ait ctl`
- [CHANGELOG.md](CHANGELOG.md) — what shipped in each release
- [REQUIREMENTS.md](REQUIREMENTS.md) — product requirements and the
  §7 roadmap
- `ait --help` — the full CLI surface, man-page style

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
