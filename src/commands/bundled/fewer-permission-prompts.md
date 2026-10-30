---
description: pre-approve safe read-only commands based on your sessions
---
Cut the permission prompts I keep answering yes to. Scope: $ARGUMENTS

Read the recent sessions in `~/.wizard/sessions` (newest files first, a few dozen is enough) and list the shell commands and MCP tools that were run, with how often. Keep only those that are read-only and low risk: `git status`, `git diff`, `git log`, `ls`, `cat`, `grep`, test and build runners that do not publish, and MCP tools that only fetch. Leave out anything that writes outside the project, deletes, pushes, installs globally or reaches a network host with credentials.

Show the rules you would add in Claude's format (`Bash(git diff:*)`, `mcp__server__tool`), then add them to `allow` under `permissions` in `.claude/settings.local.json`, creating it if needed and keeping what is there. Tell me what you added and what you chose to leave out.
