# ============================================================
# LiteLLM Proxy + HF Endpoint Proxy — HuggingFace Spaces
# ============================================================
# Runs both LiteLLM and the HF endpoint proxy using supervisord.
# HF Spaces requires port 7860.
#
# Set secrets in Space Settings > Repository secrets:
#   HF_TOKEN, HF_ENDPOINT_URL, LITELLM_MASTER_KEY, DATABASE_URL
# ============================================================

FROM ghcr.io/berriai/litellm:main-stable

USER root

# Install system dependencies (Alpine uses apk)
RUN apk add --no-cache supervisor

# Install proxy dependencies
RUN pip install --no-cache-dir \
    fastapi \
    uvicorn \
    httpx

WORKDIR /app

# Copy configuration files
COPY config.yaml .
COPY supervisord.conf /etc/supervisor/conf.d/supervisord.conf
COPY proxy/app.py ./proxy/

# Create required directories for supervisord
RUN mkdir -p /var/log/supervisor /var/run

EXPOSE 7860

# Override entrypoint and start supervisord to manage both processes
ENTRYPOINT []
CMD ["/usr/bin/supervisord", "-c", "/etc/supervisor/conf.d/supervisord.conf"]
