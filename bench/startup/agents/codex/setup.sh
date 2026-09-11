#!/bin/sh
# check_for_update_on_startup and [analytics] are Codex's switches for its
# update check and telemetry; the [features] flags turn off the plugin
# marketplace sync, which otherwise clones github.com/openai/plugins at
# startup.
mkdir -p ~/.codex
cat > ~/.codex/config.toml <<'TOML'
model = "bench-model"
model_provider = "bench"
check_for_update_on_startup = false

[analytics]
enabled = false

[features]
remote_plugin = false
plugin_sync = false

[model_providers.bench]
name = "bench"
base_url = "http://127.0.0.1:9/v1"
env_key = "OPENAI_API_KEY"

[projects."/work"]
trust_level = "trusted"
TOML
echo '{"OPENAI_API_KEY": "sk-bench"}' > ~/.codex/auth.json
