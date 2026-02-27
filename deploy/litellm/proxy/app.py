"""
HF Endpoint Proxy - Bridges LiteLLM to HuggingFace Dedicated Endpoints

This proxy receives OpenAI-compatible requests at /v1/chat/completions
and forwards them to the HF dedicated endpoint at the root path.
"""

from fastapi import FastAPI, Request, HTTPException
import httpx
import os

app = FastAPI(title="HF Endpoint Proxy")

HF_ENDPOINT_URL = os.environ.get("HF_ENDPOINT_URL", "")
HF_TOKEN = os.environ.get("HF_TOKEN", "")


@app.post("/v1/chat/completions")
async def chat_completions(request: Request):
    """
    Proxy /v1/chat/completions to HF dedicated endpoint root path.
    Converts OpenAI chat format to HF inputs format and back.
    """
    try:
        data = await request.json()
        messages = data.get("messages", [])
        
        # Extract user message
        user_content = ""
        for msg in messages:
            if msg.get("role") == "user":
                user_content = msg.get("content", "")
                break
        
        if not user_content:
            raise HTTPException(status_code=400, detail="No user message found")
        
        # Build HF request
        hf_payload = {
            "inputs": user_content,
            "parameters": {
                "max_new_tokens": data.get("max_tokens", 100),
                "temperature": data.get("temperature", 0.1),
            }
        }
        
        # Call HF endpoint at root path
        async with httpx.AsyncClient(timeout=120.0) as client:
            response = await client.post(
                HF_ENDPOINT_URL,
                json=hf_payload,
                headers={
                    "Authorization": f"Bearer {HF_TOKEN}",
                    "Content-Type": "application/json"
                }
            )
        
        if response.status_code != 200:
            raise HTTPException(status_code=response.status_code, detail=response.text)
        
        hf_response = response.json()
        
        # Extract generated text from HF response
        if isinstance(hf_response, list) and len(hf_response) > 0:
            generated_text = hf_response[0].get("generated_text", "")
        elif isinstance(hf_response, dict):
            generated_text = hf_response.get("generated_text", "")
        else:
            generated_text = str(hf_response)
        
        # Return OpenAI-compatible format
        return {
            "id": "chatcmpl-proxy",
            "object": "chat.completion",
            "created": 0,
            "model": data.get("model", "default"),
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": generated_text
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "total_tokens": 0
            }
        }
    
    except httpx.RequestError as e:
        raise HTTPException(status_code=503, detail=f"HF endpoint error: {str(e)}")


@app.get("/v1/models")
async def list_models():
    """List available models."""
    return {"data": [{"id": "default", "object": "model"}]}


@app.get("/health")
async def health():
    """Health check endpoint."""
    return {"status": "ok"}
