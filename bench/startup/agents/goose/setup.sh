#!/bin/sh
mkdir -p ~/.config/goose
cat > ~/.config/goose/config.yaml <<'YAML'
GOOSE_PROVIDER: openai
GOOSE_MODEL: bench-model
GOOSE_TELEMETRY_ENABLED: false
OPENAI_HOST: http://127.0.0.1:9
YAML
