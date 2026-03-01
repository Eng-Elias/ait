# Deployment Guide: LiteLLM on HF Spaces + Model on HF Inference API

A step-by-step practical guide to deploy the full AIT backend stack and test the app end-to-end.

**What you'll set up:**

```
AIT CLI  ──►  LiteLLM Proxy (HF Spaces)  ──►  HF Endpoint Proxy  ──►  Model (HF Dedicated Endpoint)
```

**Time required:** ~15–20 minutes

---

## Prerequisites

- A [HuggingFace account](https://huggingface.co/join) (free)
- A HuggingFace API token with **write** access
- A free [Supabase](https://supabase.com) account (for the LiteLLM database)
- `ait` binary built and available (see main README)

---

## Step 1: Get Your HuggingFace Token

1. Go to [huggingface.co/settings/tokens](https://huggingface.co/settings/tokens)
2. Click **New token**
3. Name: `ait-deploy`
4. Type: **Write** (needed to create Spaces)
5. Click **Generate**
6. Copy the token — you'll need it in Steps 2 and 3

```
hf_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

---

## Step 2: Deploy the Model on HF Inference API

The HuggingFace Inference API lets you use models for free (rate-limited) or via paid endpoints.

### Option A: Use a Public Model (Free, No Setup)

HuggingFace hosts many models on their free Inference API. No deployment needed — just pick a model:

| Model | Good For | Model ID |
|-------|----------|----------|
| Qwen2.5-Coder-0.5B-Instruct | Fast, lightweight code/command generation | `Qwen/Qwen2.5-Coder-0.5B-Instruct` |
| Qwen2.5-Coder-1.5B-Instruct | Better quality, still fast | `Qwen/Qwen2.5-Coder-1.5B-Instruct` |
| CodeLlama-7b-Instruct-hf | Strong code generation | `codellama/CodeLlama-7b-Instruct-hf` |

**Test the model directly:**

```bash
curl https://api-inference.huggingface.co/models/Qwen/Qwen2.5-Coder-0.5B-Instruct/v1/chat/completions \
  -H "Authorization: Bearer hf_YOUR_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "Qwen/Qwen2.5-Coder-0.5B-Instruct",
    "messages": [
      {"role": "system", "content": "You are a shell command generator. Return ONLY the command, no explanation."},
      {"role": "user", "content": "Generate a single shell command for: list all files larger than 100MB"}
    ],
    "max_tokens": 200
  }'
```

If you get a valid response with a command, the model is ready. Move to Step 3.

### Option B: Create a Dedicated Inference Endpoint (Paid)

For production use with guaranteed availability and no rate limits:

1. Go to [ui.endpoints.huggingface.co](https://ui.endpoints.huggingface.co/)
2. Click **New Endpoint**
3. **Model**: `Qwen/Qwen2.5-Coder-0.5B-Instruct`
4. **Cloud**: AWS or Azure
5. **Instance**: CPU (cheapest) or GPU (faster)
6. **Scaling**: Enable **Scale to Zero** to save costs
7. Click **Create Endpoint**
8. Wait for status to show **Running**
9. Copy the endpoint URL:
   ```
   https://xxxx.aws.endpoints.huggingface.cloud
   ```

---

## Step 3: Set Up Supabase Database

LiteLLM requires PostgreSQL for virtual keys, spend tracking, and user authentication.

### 3.1 Create a Supabase Project

1. Go to [supabase.com](https://supabase.com) and sign in (free account)
2. Click **New Project**
   - **Name**: `litellm-ait`
   - **Database password**: choose a strong password — **save it, you'll need it below**
   - **Region**: choose the closest to you
3. Wait ~1 minute for the project to be created

### 3.2 Get the Connection Pooler String

Cloud platforms like HuggingFace Spaces typically block direct PostgreSQL connections (port 5432). Use Supabase's **connection pooler** instead (port 6543).

1. Go to **Project Settings** → **Database**
2. Find **Connection string** → switch to **Connection pooling** → select **Session** mode
3. Copy the URI. It looks like this:

```
postgresql://postgres.[PROJECT-REF]:[YOUR-PASSWORD]@aws-0-[REGION].pooler.supabase.com:6543/postgres
```

> **Note the differences from the direct connection:**
> - **Host**: `aws-0-REGION.pooler.supabase.com` (not `db.PROJECT.supabase.co`)
> - **Port**: `6543` (not `5432`)
> - **User**: `postgres.PROJECT_REF` (includes project ref)

4. **Replace `[YOUR-PASSWORD]`** with the database password you chose above

### 3.3 URL-Encode Special Characters in Password

If your password contains special characters (`@`, `#`, `!`, `%`, etc.), you **must URL-encode them** or the URI will be parsed incorrectly.

| Character | Encoded | Character | Encoded |
|-----------|---------|-----------|---------|
| `@` | `%40` | `#` | `%23` |
| `!` | `%21` | `$` | `%24` |
| `%` | `%25` | `&` | `%26` |
| `/` | `%2F` | `=` | `%3D` |
| `+` | `%2B` | `:` | `%3A` |

**Example:** password `MyP@ss#123` → encoded as `MyP%40ss%23123`

Encode your password with:

```bash
python3 -c "import urllib.parse; print(urllib.parse.quote('YOUR_PASSWORD_HERE', safe=''))"
```

### 3.4 Verify the Final Format

Your `DATABASE_URL` should look like this (with real values, password URL-encoded):

```
postgresql://postgres.abcdefghijkl:MyP%40ss%23123@aws-0-us-east-1.pooler.supabase.com:6543/postgres
```

Double-check:
- Starts with `postgresql://`
- User is `postgres.YOUR_PROJECT_REF`
- Password special characters are URL-encoded
- Host is `aws-0-REGION.pooler.supabase.com`
- Port is `6543`
- Only one `@` appears (the one separating credentials from host)
- No spaces, no quotes, no line breaks

---

## Step 4: Deploy LiteLLM on HuggingFace Spaces

### 4.1 Create the Space

1. Go to [huggingface.co/new-space](https://huggingface.co/new-space)
2. Fill in:
   - **Space name**: `ait-proxy`
   - **SDK**: **Docker**
   - **Visibility**: **Private** (recommended)
3. Click **Create Space**

### 4.2 Set Secrets

Go to your Space → **Settings** → **Repository secrets** and add these **4 secrets**:

| Secret Name | Value | Example |
|-------------|-------|---------|
| `HF_TOKEN` | Your HuggingFace token (Step 1) | `hf_abc123...` |
| `LITELLM_MASTER_KEY` | A strong random key | Generate with `openssl rand -hex 32` |
| `DATABASE_URL` | Supabase connection string (Step 3) | `postgresql://postgres:pass@db.xxx.supabase.co:5432/postgres` |
| `UI_PASSWORD` | Dashboard admin password | Any strong password |

> **Warning:** When pasting `DATABASE_URL`, make sure there are **no extra spaces or line breaks**. Copy the exact URI from Supabase.

### 4.3 Upload Deploy Files

Clone the Space repo and add two files:

```bash
git clone https://huggingface.co/spaces/YOUR_USERNAME/ait-proxy
cd ait-proxy
```

**Create `config.yaml`:**

```yaml
model_list:
  - model_name: "default"
    litellm_params:
      model: "huggingface/Qwen/Qwen2.5-Coder-0.5B-Instruct"
      api_key: "os.environ/HF_TOKEN"

general_settings:
  master_key: "os.environ/LITELLM_MASTER_KEY"
  database_url: "os.environ/DATABASE_URL"
  enable_user_auth: true
  max_budget: 10.0
  budget_duration: "30d"
  ui_access_mode: "admin_only"

rate_limits:
  - model: "default"
    tpm: 10000
    rpm: 60

litellm_settings:
  telemetry: false
  drop_params: true
  cache: true
  cache_params:
    type: "local"
    ttl: 600
```

**Create `Dockerfile`:**

```dockerfile
FROM docker.litellm.ai/berriai/litellm:main-stable

WORKDIR /app

COPY config.yaml .

EXPOSE 7860

# UI is available at /ui by default — no flag needed
CMD ["--port", "7860", "--config", "config.yaml"]
```

**Push to the Space:**

```bash
git add Dockerfile config.yaml
git commit -m "Deploy LiteLLM proxy"
git push
```

### 4.4 Wait for Build

1. Go to your Space page on HuggingFace
2. Watch the **Logs** tab — the build takes 2–5 minutes
3. When you see `LiteLLM Proxy started`, the proxy is live

Your proxy URL is:
```
https://YOUR_USERNAME-ait-proxy.hf.space
```

### 4.5 Verify the Proxy

```bash
# Health check
curl https://YOUR_USERNAME-ait-proxy.hf.space/health

# Expected: {"status":"healthy"}
```

Open the dashboard at:
```
https://YOUR_USERNAME-ait-proxy.hf.space/ui
```

Log in with your `LITELLM_MASTER_KEY`.

---

## Step 5: Create a Virtual Key

Virtual keys let you control access without exposing your master key or HF token.

### Via curl:

```bash
curl -X POST https://YOUR_USERNAME-ait-proxy.hf.space/key/generate \
  -H "Authorization: Bearer YOUR_MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "models": ["default"],
    "max_budget": 5.0,
    "budget_duration": "30d"
  }'
```

**Response:**

```json
{
  "key": "sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  ...
}
```

Save the `key` value — this is your API token for ait.

### Via Dashboard:

1. Open `https://YOUR_USERNAME-ait-proxy.hf.space/ui`
2. Log in with your `LITELLM_MASTER_KEY`
3. Go to **Virtual Keys**
4. Click **Generate Key**
5. Set models, budget, and duration
6. Copy the generated key

---

## Step 6: Configure ait

Now connect your ait CLI to the deployed proxy:

```bash
# Set the API endpoint to your LiteLLM proxy
ait config set api_endpoint https://YOUR_USERNAME-ait-proxy.hf.space/v1/chat/completions

# Set your virtual key
ait config set api_token sk-your-virtual-key-here

# Set the model name (must match model_name in config.yaml)
ait config set model default
```

Or run the setup wizard:

```bash
ait setup
# API Endpoint: https://YOUR_USERNAME-ait-proxy.hf.space/v1/chat/completions
# API Token: sk-your-virtual-key-here
# Model: default
```

Verify the config:

```bash
ait config
```

Expected output:

```
api_endpoint: https://YOUR_USERNAME-ait-proxy.hf.space/v1/chat/completions
api_token:    ***********xxxx
model:        default
shell:        auto
```

---

## Step 7: Test the App

### Basic Test

```bash
ait "list all files in the current directory"
```

Expected: prints a command like `ls -la` (Linux/macOS) or `Get-ChildItem` (Windows).

### Cross-Platform Test

```bash
ait "find all log files larger than 10MB" -t linux
# Expected: find / -name "*.log" -size +10M

ait "find all log files larger than 10MB" -t win
# Expected: Get-ChildItem -Path C:\ -Recurse -Filter *.log | Where-Object {$_.Length -gt 10MB}

ait "find all log files larger than 10MB" -t mac
# Expected: find / -name "*.log" -size +10M
```

### Headless / Pipe Test

```bash
ait generate "show current date and time"
# Prints just the command to stdout, e.g.: date
```

### Error Handling Tests

```bash
# Test with invalid token
ait config set api_token sk-invalid-key
ait "test"
# Expected: error about authentication

# Restore your key
ait config set api_token sk-your-virtual-key-here
```

---

## Troubleshooting

### "Connection refused" or timeout

- **Check the Space is running**: visit `https://YOUR_USERNAME-ait-proxy.hf.space`
- **HF Spaces may sleep** after inactivity — the first request wakes it up (~30s)
- **Check health**: `curl https://YOUR_USERNAME-ait-proxy.hf.space/health`

### "Authentication failed"

- Use a **virtual key** from Step 5 (not the master key) as your `api_token`
- Verify the key is active in the LiteLLM dashboard
- Regenerate the key if needed

### "Model not found" or empty response

- Check `config.yaml` — the `model_name` must match what you set in ait config
- Verify the HF model is accessible: test it with curl (Step 2)
- Check LiteLLM logs in the Space **Logs** tab

### "Database connection failed" (P1001 error)

The `DATABASE_URL` secret is malformed or the database is unreachable. Common causes:

1. **Using direct connection instead of pooler** — HF Spaces blocks port 5432. You must use the **connection pooler** (port 6543). See Step 3.2.
   ```
   # WRONG (direct, port 5432):
   postgresql://postgres:PASS@db.PROJECT.supabase.co:5432/postgres

   # CORRECT (pooler, port 6543):
   postgresql://postgres.PROJECT:PASS@aws-0-REGION.pooler.supabase.com:6543/postgres
   ```
2. **Special characters in password not URL-encoded** — `@`, `#`, `!` etc. must be percent-encoded. See Step 3.3.
3. **Supabase project is paused** — free-tier projects pause after 1 week of inactivity. Go to the Supabase dashboard and click **Restore**.

### Slow responses

- First request may be slow (~10–30s) if the HF Space or model endpoint was sleeping
- Subsequent requests should be faster (1–5s)
- Consider enabling LiteLLM caching (already enabled in config)

---

## Architecture Summary

```
┌──────────────────┐     HTTPS      ┌─────────────────────┐     HTTPS      ┌──────────────────┐
│                  │  ──────────►   │                     │  ──────────►   │                  │
│   ait CLI     │                │  LiteLLM Proxy      │                │  HF Inference    │
│   (your machine) │  ◄──────────   │  (HF Spaces)        │  ◄──────────   │  API / Endpoint  │
│                  │                │                     │                │                  │
└──────────────────┘                │  • Virtual keys     │                └──────────────────┘
                                    │  • Rate limiting    │
                                    │  • Spend tracking   │                ┌──────────────────┐
                                    │  • Response caching │                │  PostgreSQL      │
                                    │  • Admin dashboard  │  ◄──────────   │  (Supabase)      │
                                    └─────────────────────┘                └──────────────────┘
```

---

## Cost Breakdown

| Component | Cost |
|-----------|------|
| HuggingFace Inference API (free tier) | Free (rate-limited) |
| HuggingFace Space (Docker, private) | Free |
| Supabase (free tier, 500 MB) | Free |
| **Total** | **$0/month** |

For higher throughput, consider a Dedicated Inference Endpoint (~$0.06/hr for CPU).
