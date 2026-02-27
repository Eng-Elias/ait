# ============================================================
# LiteLLM Proxy + HF Endpoint Proxy — HuggingFace Spaces
# ============================================================
# Runs both LiteLLM and the HF endpoint proxy using supervisord.
# HF Spaces requires port 7860.
#
# Set secrets in Space Settings > Repository secrets:
#   HF_TOKEN, HF_ENDPOINT_URL, LITELLM_MASTER_KEY, DATABASE_URL
# ============================================================

FROM python:3.11-slim

# Install system dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    supervisor \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Install Python dependencies
RUN pip install --no-cache-dir \
    litellm[proxy] \
    fastapi \
    uvicorn \
    httpx

WORKDIR /app

# Copy configuration files
COPY config.yaml .
COPY supervisord.conf /etc/supervisor/conf.d/supervisord.conf
COPY proxy/app.py ./proxy/

# Create log directory
RUN mkdir -p /var/log/supervisor

EXPOSE 7860

# Start supervisord to manage both processes
CMD ["/usr/bin/supervisord", "-c", "/etc/supervisor/conf.d/supervisord.conf"]
