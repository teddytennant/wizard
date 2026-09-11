#!/bin/sh
mkdir -p ~/.codex
cat > ~/.codex/config.toml <<'TOML'
model = "bench-model"
model_provider = "bench"

[model_providers.bench]
name = "bench"
base_url = "http://127.0.0.1:9/v1"
env_key = "OPENAI_API_KEY"
TOML
echo '{"OPENAI_API_KEY": "sk-bench"}' > ~/.codex/auth.json
cat >> ~/.codex/config.toml <<'TOML'

[projects."/work"]
trust_level = "trusted"
TOML
