# Security Model

## Overview

AIT implements layered security controls to protect API credentials, prevent dangerous command execution, and ensure safe interaction with AI-generated shell commands.

---

## API Token Protection

### Storage

Configuration is stored at:
- **Unix**: `~/.ait/config.json` (file `0600`, directory `0700`)
- **Windows**: `%USERPROFILE%\.ait\config.json`

Only the file owner has read/write access. Permissions are enforced on creation.

### Display Masking

API tokens are **never shown in full**. All display output masks tokens to show only the last 4 characters:

```
API Token: ***********a1b2
```

This applies to:
- `ait config` output
- Error messages
- Debug logs

### What Is Never Done

- Tokens are never logged in full
- Tokens are never printed to stdout
- Tokens are never included in error messages
- Tokens are never sent to any endpoint other than the configured `api_endpoint`

---

## Virtual Key System (via LiteLLM)

When using LiteLLM as a proxy (see `deploy/litellm/`), users authenticate with **virtual keys** instead of raw provider tokens.

### Why Virtual Keys?

| Risk | Raw Provider Token | LiteLLM Virtual Key |
|------|-------------------|-------------------|
| Revocation | Must regenerate token | Disable instantly |
| Scope | Full provider access | Specific models only |
| Tracking | No usage visibility | Per-key cost tracking |
| Budget | No limits | Configurable spend caps |
| Rate limits | Provider-level only | Per-key RPM/TPM |

### Key Format

```
sk-litellm-{random}
```

Virtual keys are created via the LiteLLM dashboard or API:

```bash
curl -X POST http://localhost:4000/key/generate \
  -H "Authorization: Bearer $LITELLM_MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"models": ["default"], "max_budget": 5.0, "budget_duration": "30d"}'
```

### Key Lifecycle

| Action | Method |
|--------|--------|
| Create | Dashboard or `POST /key/generate` |
| Disable | Dashboard or `POST /key/delete` |
| Set budget | Dashboard or `POST /key/update` |
| View spend | Dashboard or `GET /key/info` |

---

## Command Safety

AI-generated commands are **always shown to the user before any action is taken**. AIT never auto-executes commands.

### User Workflow

```
1. User provides natural language description
2. AI generates a shell command
3. Command is printed to stdout
4. User reviews the command
5. User decides whether to copy/run it manually
```

### Risk Awareness

Users should be aware of potentially dangerous commands. Common risky patterns:

| Risk Level | Examples | Recommendation |
|------------|----------|----------------|
| **Safe** | `ls`, `cat`, `pwd`, `echo` | Run freely |
| **Low** | `grep`, `find`, `du`, `df` | Review paths |
| **Medium** | `chmod`, `chown`, `apt install` | Verify arguments |
| **High** | `curl ... \| bash`, `rm -rf` | Inspect carefully |
| **Critical** | `rm -rf /`, `:(){ :\|:& };:` | Never run |

### Design Principle

AIT intentionally **does not execute commands** — it only prints them. This is a deliberate security choice:
- The user's existing terminal handles execution
- The user has full control and visibility
- No privilege escalation through the tool
- No hidden side effects

---

## Data Privacy

### What AIT Sends to the API

Each request contains:
- **System prompt**: OS type and shell type (e.g., "Linux", "bash")
- **User prompt**: The natural language description provided by the user

### What AIT Does NOT Send

- File contents
- Environment variables
- Command history
- Directory listings
- Any data beyond the user's explicit prompt

### Local Data

| Data | Stored Locally | Location |
|------|---------------|----------|
| Config (endpoint, model, shell) | Yes | `~/.ait/config.json` |
| API token | Yes (in config) | `~/.ait/config.json` |
| Debug logs (if enabled) | Yes | `~/.ait/debug.log` |
| Command history | No | — |
| Prompts | No | — |
| AI responses | No | — |

### Debug Logging

When `--debug` is enabled:
- Logs are written to `~/.ait/debug.log`
- API tokens are masked in logs
- Logs contain request metadata (not full prompts by default)
- Users should **not** share debug logs publicly without review

---

## Rate Limiting (via LiteLLM)

When using LiteLLM, rate limits are enforced per virtual key:

```yaml
Free Tier:
  rpm: 30          # requests per minute
  tpm: 5000        # tokens per minute

Pro Tier:
  rpm: 120
  tpm: 20000

Enterprise:
  rpm: 600
  tpm: 100000
```

Rate limiting prevents:
- Accidental cost overruns
- API abuse
- Denial of service

---

## Access Control (via LiteLLM)

| Role | API Access | Dashboard | Key Management |
|------|-----------|-----------|---------------|
| End user | Virtual key → specific models | No | No |
| Admin | Master key → all models | Full access | Full control |

### Admin Capabilities

- Create/revoke virtual keys
- Set per-key budgets and rate limits
- View spend logs and usage metrics
- Configure model routing

---

## Incident Response

### Compromised Virtual Key

1. **Disable** the key immediately via dashboard or API
2. **Review** spend logs for unauthorized usage
3. **Generate** a new key for the affected user
4. **Update** AIT config: `ait config set api_token sk-new-key`

### Compromised Master Key

1. **Rotate** the master key (`LITELLM_MASTER_KEY` in `.env`)
2. **Restart** the LiteLLM proxy
3. **Revoke** all existing virtual keys
4. **Regenerate** virtual keys for legitimate users
5. **Audit** database for unauthorized key creation

---

## Network Security

### HTTPS

- Always use HTTPS endpoints in production
- AIT sends the `Authorization: Bearer` header — this must be encrypted in transit
- Local development (localhost) may use HTTP

### Minimal Attack Surface

- AIT is a stateless CLI tool — no listening ports, no daemon
- Configuration is local files only
- No telemetry, no analytics, no phone-home

---

## Compliance Considerations

### GDPR / Data Protection

- User prompts are processed but **not stored** by AIT
- LiteLLM can be configured with data retention policies
- Users can delete their local config at any time: `rm -rf ~/.ait`

### Audit Trail (via LiteLLM)

When using LiteLLM with a database, all administrative actions are logged:
- Key creation and deletion
- Budget changes
- Rate limit updates
- Access denials
