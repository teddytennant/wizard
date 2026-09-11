#!/bin/sh
# "autoupdate": false is OpenCode's switch for its GitHub release check.
mkdir -p ~/.config/opencode
cat > ~/.config/opencode/opencode.json <<'JSON'
{"$schema": "https://opencode.ai/config.json",
 "autoupdate": false,
 "provider": {"bench": {"npm": "@ai-sdk/openai-compatible", "name": "bench",
   "options": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "sk-bench"},
   "models": {"bench-model": {"name": "bench-model"}}}},
 "model": "bench/bench-model"}
JSON
