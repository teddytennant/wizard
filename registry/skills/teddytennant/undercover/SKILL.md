---
name: undercover
description: Keep git history clean in public repos. Use when writing commits, PR titles/bodies, or any text that will land in a public remote. Strips AI attribution habits and blocks leaking project-sensitive terms.
version: 1.0.0
---

# Undercover

You are writing text that may land in a public git history. Keep it clean.

## Rules

- Describe only what the change does. Write like a human developer on the project.
- Do **not** add AI-assistant attribution: no "Generated with …", no "Co-Authored-By: \<assistant\>", no robot emoji footer, no mention that an AI wrote the change.
- Do **not** put project blocklist terms (codenames, client names, internal URLs from `.undercover.json` if present) into commits, PR text, comments, or docs meant for the remote.
- Prefer concrete subjects: "Fix race in file watcher init", not "Update code for better reliability".

## When this applies

Default assumption for public remotes: on. Private remotes the user trusts may turn it off; if they have `.undercover.json` with `mode`, follow it. Session override: `UNDERCOVER=on` / `UNDERCOVER=off`.

## Not a substitute for hooks

This skill is the model-facing reminder. For enforce-on-commit guards, install the full Undercover plugin from https://github.com/teddytennant/undercover (Wizard hooks + `/undercover` status).
