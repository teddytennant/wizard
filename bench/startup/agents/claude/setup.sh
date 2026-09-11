#!/bin/sh
# Pre-answer onboarding (theme, API key approval, folder trust) so the first
# frame is the prompt. Key is a dummy; the network is off anyway.
mkdir -p ~/.claude
cat > ~/.claude.json <<JSON
{"hasCompletedOnboarding": true, "theme": "dark", "lastOnboardingVersion": "99.0.0",
 "customApiKeyResponses": {"approved": ["00000000000000000000"], "rejected": []},
 "projects": {"/work": {"hasTrustDialogAccepted": true, "allowedTools": []}}}
JSON
