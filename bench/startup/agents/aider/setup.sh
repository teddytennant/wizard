#!/bin/sh
echo '{"permanently_disabled": true}' > ~/.aider.analytics.json
cat > ~/.aider.conf.yml <<'YAML'
model: gpt-4o
openai-api-base: http://127.0.0.1:9/v1
check-update: false
show-model-warnings: false
gitignore: false
show-release-notes: false
YAML
