---
title: LiteLLM Proxy
emoji: 🚀
colorFrom: blue
colorTo: purple
sdk: docker
pinned: false
---

# LiteLLM + HF Endpoint Proxy — HuggingFace Spaces Deployment

This directory contains the configuration for deploying LiteLLM with an integrated HF endpoint proxy on HuggingFace Spaces.

## Architecture

```
ait CLI → LiteLLM (port 7860) → hf-proxy (port 8000) → HF Dedicated Endpoint
                  ↓                      ↓
           /v1/chat/completions    POST / (root path)
```

Since HF Spaces doesn't support docker-compose, we use **supervisord** to run both services in a single container.

## Files

- `Dockerfile` - Combined container with LiteLLM + proxy
- `supervisord.conf` - Process manager configuration
- `config.yaml` - LiteLLM configuration pointing to internal proxy
- `proxy/app.py` - FastAPI proxy that bridges OpenAI format to HF native format

## Deployment Steps

### 1. Create a new HuggingFace Space

1. Go to [huggingface.co/new-space](https://huggingface.co/new-space)
2. Select **Docker** as the SDK
3. Choose a name (e.g., `litellm-proxy`)

### 2. Upload files to the Space

Copy all files from this directory to your Space repository:

```
your-space/
├── Dockerfile
├── supervisord.conf
├── config.yaml
└── proxy/
    └── app.py
```

### 3. Set Space Secrets

Go to **Settings > Repository secrets** and add:

| Secret | Description |
|--------|-------------|
| `HF_TOKEN` | Your HuggingFace API token |
| `HF_ENDPOINT_URL` | Your HF dedicated endpoint URL (root path, no `/v1/`) |
| `LITELLM_MASTER_KEY` | Master key for LiteLLM admin access |
| `DATABASE_URL` | Supabase connection string (use pooler on port 6543) |

### 4. Wait for Build

The Space will automatically build and deploy. Check the **Logs** tab for any errors.

### 5. Create Virtual Keys

1. Open `https://your-space.hf.space/ui`
2. Login with your master key
3. Create a virtual key for ait

### 6. Configure ait

```bash
ait config set api_endpoint https://your-space.hf.space/v1/chat/completions
ait config set api_token sk-your-virtual-key
ait config set model default
```

## Troubleshooting

### 503 Service Unavailable
Your HF dedicated endpoint may be sleeping. The first request will wake it up. Wait ~30 seconds and retry.

### Connection Timeout
Increase the timeout in `config.yaml` under `router_settings.timeout`.

### Database Connection Errors
Ensure you're using the Supabase **connection pooler** (port 6543) and URL-encode any special characters in the password.
