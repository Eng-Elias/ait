# System Architecture

## Overview

AIT (AI Terminal) is a lightweight CLI tool that translates natural language into shell commands. It uses a layered architecture separating the client application from the AI backend, allowing flexible deployment and provider choice.

```
┌─────────────────────────────────────────────────────────┐
│                     User's Terminal                      │
│                                                         │
│  $ ait "find large log files" -t linux               │
│                                                         │
└──────────────────────┬──────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────┐
│              Layer 1: AIT CLI (Go)                    │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌────────────────────┐    │
│  │ cmd/     │  │ ai/      │  │ config/            │    │
│  │ root     │→ │ client   │→ │ load/save/validate │    │
│  │ setup    │  │ resolve  │  │ mask tokens        │    │
│  │ config   │  │ generate │  └────────────────────┘    │
│  │ generate │  └──────────┘                             │
│  │ version  │                                           │
│  └──────────┘                                           │
└──────────────────────┬──────────────────────────────────┘
                       │ HTTPS (OpenAI-compatible API)
                       ▼
┌─────────────────────────────────────────────────────────┐
│         Layer 2: LiteLLM Proxy (Optional)               │
│                                                         │
│  ┌────────────┐  ┌────────────┐  ┌──────────────┐      │
│  │ Virtual    │  │ Rate       │  │ Spend        │      │
│  │ Key Auth   │  │ Limiting   │  │ Tracking     │      │
│  └────────────┘  └────────────┘  └──────────────┘      │
│  ┌────────────┐  ┌────────────┐  ┌──────────────┐      │
│  │ Request    │  │ Response   │  │ Dashboard    │      │
│  │ Routing    │  │ Caching    │  │ (Admin UI)   │      │
│  └────────────┘  └────────────┘  └──────────────┘      │
│                                                         │
└──────────────────────┬──────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────┐
│              Layer 3: Model Endpoints                    │
│                                                         │
│  ┌──────────────────┐  ┌───────────────────────────┐    │
│  │ HuggingFace      │  │ OpenAI / Any Compatible  │    │
│  │ Inference API    │  │ Endpoint                  │    │
│  └──────────────────┘  └───────────────────────────┘    │
│  ┌──────────────────┐                                   │
│  │ Ollama (local)   │                                   │
│  └──────────────────┘                                   │
└─────────────────────────────────────────────────────────┘
```

---

## Layer 1: AIT CLI

The Go binary that the user interacts with directly. It has no background processes, no daemon, no TUI — just a single command invocation.

### Packages

| Package | Responsibility |
|---------|---------------|
| `cmd/root.go` | Parse prompt + `-t` flag, call AI, print result |
| `cmd/setup.go` | Interactive wizard to configure API credentials |
| `cmd/config.go` | Read/write config values |
| `cmd/generate.go` | Headless mode — print command to stdout (for scripting) |
| `cmd/version.go` | Print version |
| `internal/ai/client.go` | HTTP client for OpenAI-compatible chat completions API |
| `internal/config/config.go` | JSON config file management, token masking, validation |

### Request Flow

```
1. User runs:  ait "find PDFs modified this week" -t linux
2. cmd/root.go joins args into prompt, reads -t flag
3. config.Load() reads ~/.ait/config.json
4. config.Validate() checks api_token, api_endpoint, model exist
5. ai.ResolveTargetOS("linux") → ("Linux", "bash")
6. ai.GenerateCommand() sends POST to API with system prompt + user prompt
7. Response parsed, code fences stripped
8. Command printed to stdout
```

### OS Detection (`-t` flag)

| Input | Resolved OS | Shell Context |
|-------|-------------|---------------|
| `win` / `windows` | Windows | PowerShell |
| `linux` | Linux | bash |
| `mac` / `macos` / `darwin` | macOS | zsh |
| *(empty)* | `runtime.GOOS` | auto |

The resolved OS and shell are injected into the system prompt so the AI generates platform-appropriate commands.

---

## Layer 2: LiteLLM Proxy (Optional)

An optional middleware layer that adds enterprise features between AIT and model endpoints. Deployed via Docker Compose or HuggingFace Spaces (see `deploy/litellm/`).

### Why LiteLLM?

| Feature | Direct API | Via LiteLLM |
|---------|-----------|-------------|
| Virtual keys | No | Yes — revocable, scoped, budget-limited |
| Spend tracking | No | Yes — per-key cost logs |
| Rate limiting | No | Yes — RPM / TPM per model |
| Multi-provider routing | No | Yes — failover across providers |
| Response caching | No | Yes — identical prompts served from cache |
| Admin dashboard | No | Yes — web UI for key/spend management |

### Components

| Component | Technology | Purpose |
|-----------|-----------|---------|
| Proxy server | LiteLLM Docker image | API gateway |
| Database | PostgreSQL 16 | Virtual keys, spend logs |
| Dashboard | LiteLLM built-in UI | Admin management |

### Deployment Options

| Method | Use Case |
|--------|----------|
| `docker compose up` | Local development, self-hosted |
| HuggingFace Spaces (Docker SDK) | Free cloud hosting |
| Any Docker host | VPS, Kubernetes, etc. |

---

## Layer 3: Model Endpoints

AIT works with any OpenAI-compatible chat completions endpoint.

| Provider | Model Example | Latency | Cost |
|----------|--------------|---------|------|
| HuggingFace Inference API | Qwen2.5-Coder-0.5B | ~2-5s | Free tier available |
| OpenAI | gpt-4o-mini | ~1-3s | Pay per token |
| Ollama (local) | llama3, codellama | ~1-2s | Free (local GPU/CPU) |
| HF Dedicated Endpoint | Custom fine-tuned | ~1-2s | Pay per hour |

---

## Configuration

Stored at `~/.ait/config.json` (or `%USERPROFILE%\.ait\config.json` on Windows).

```json
{
  "api_endpoint": "https://api.openai.com/v1/chat/completions",
  "api_token": "sk-...",
  "model": "gpt-4o-mini",
  "shell": "auto"
}
```

- **Permissions**: directory `0700`, file `0600` (Unix)
- **Token display**: masked to last 4 characters in all output
- **Defaults**: created automatically on first run

---

## Data Flow Diagram

```
User                AIT CLI             LiteLLM Proxy       Model Endpoint
 │                     │                     │                     │
 │  "find big files"   │                     │                     │
 │ ──────────────────► │                     │                     │
 │                     │   POST /v1/chat/    │                     │
 │                     │   completions       │                     │
 │                     │ ──────────────────► │                     │
 │                     │                     │  Validate key       │
 │                     │                     │  Check budget       │
 │                     │                     │  Check rate limit   │
 │                     │                     │                     │
 │                     │                     │  POST /v1/chat/     │
 │                     │                     │  completions        │
 │                     │                     │ ──────────────────► │
 │                     │                     │                     │
 │                     │                     │  ◄──── response ─── │
 │                     │                     │  Log spend          │
 │                     │  ◄──── response ─── │                     │
 │                     │                     │                     │
 │                     │  Strip code fences  │                     │
 │  ◄── print command  │                     │                     │
 │                     │                     │                     │
 │  find . -size +100M │                     │                     │
 │  (user copies/runs) │                     │                     │
```

---

## Build & Distribution

| Target | Command | Output |
|--------|---------|--------|
| Current platform | `make build` | `dist/ait` |
| All platforms | `make build-all` | `dist/ait-{os}-{arch}` |
| Linux AMD64 | `make linux-amd64` | `dist/ait-linux-amd64` |
| macOS ARM64 | `make darwin-arm64` | `dist/ait-darwin-arm64` |
| Windows AMD64 | `make windows-amd64` | `dist/ait-windows-amd64.exe` |

Version is embedded at build time via `-ldflags "-X ait/cmd.Version=X.Y.Z"`.
