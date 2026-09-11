#!/bin/sh
# Dummy OpenAI-compatible provider at a port nothing listens on, so wizard
# skips onboarding and any probe fails instantly instead of timing out.
mkdir -p ~/.wizard
cat > ~/.wizard/config.toml <<'TOML'
active_provider = "bench"
mode = "genie"

[[providers]]
name = "bench"
kind = "openai"
base_url = "http://127.0.0.1:9/v1"
model = "bench-model"
api_key_env = "OPENAI_API_KEY"
TOML
