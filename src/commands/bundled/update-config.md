---
description: change Wizard's configuration from a plain-words request
---
Change Wizard's configuration as asked: $ARGUMENTS

User settings live in ~/.wizard/config.toml, MCP servers in ~/.wizard/mcp.toml, hooks in ~/.wizard/hooks.toml, and per-project overrides in .wizard/ in the project. docs/ in the Wizard repository describes every key. Read the current file first, change only the keys the request names, keep the rest of the file as it was, and say what you changed. If the request needs a key you cannot find in the docs, say so instead of inventing one. Run `wizard doctor` afterwards if the change could break startup.
