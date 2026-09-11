#!/bin/sh
mkdir -p ~/.config/crush
cat > ~/.config/crush/crush.json <<'JSON'
{"$schema": "https://charm.land/crush.json",
 "options": {"disable_provider_auto_update": true},
 "providers": {"bench": {"type": "openai", "base_url": "http://127.0.0.1:9/v1", "api_key": "sk-bench",
   "models": [{"id": "bench-model", "name": "bench-model", "context_window": 128000, "default_max_tokens": 4096}]}},
 "models": {"large": {"model": "bench-model", "provider": "bench"}, "small": {"model": "bench-model", "provider": "bench"}}}
JSON
# Crush asks to write AGENTS.md on first sight of a project; this flag file is
# what it writes after answering.
mkdir -p /work/.crush && touch /work/.crush/init
