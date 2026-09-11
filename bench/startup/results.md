# Startup benchmark

Measured 2026-09-10 on ?, ? CPUs, kernel ?, ?. One ubuntu:24.04 container per agent, 10 pty starts each, container network bridge.

This run predates the single phone-home rule: what each agent was told is under its row, and Claude Code, Codex and OpenCode were measured with their update, telemetry and policy fetches still on.

| agent | version | command | install | bundle | warm start (median) | warm p90 | cold start | `--version` | RSS | PSS | peak RSS | CPU at 3 s |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| wizard | wizard 3.0.1 | `wizard` | 25 MB | 25 MB | 30 ms | 34 ms | 327 ms | 14 ms | 20 MB | 19 MB | 20 MB | n/a |
| Claude Code | 2.1.268 (Claude Code) | `claude` | 219 MB | 219 MB | 1285 ms | 1359 ms | 3394 ms | 20 ms | 259 MB | 257 MB | 268 MB | n/a |
| Codex CLI | codex-cli 0.154.0 | `codex` | 553 MB | 339 MB | 86 ms | 89 ms | 1036 ms | 91 ms | 206 MB | 197 MB | 206 MB | n/a |
| OpenCode | 1.18.30 | `opencode` | 185 MB | 185 MB | 8328 ms | 10305 ms | 9535 ms | 1358 ms | 916 MB | 914 MB | 917 MB | n/a |
| Crush | crush version v0.93.1 | `crush` | 96 MB | 95 MB | 203 ms | 230 ms | 1077 ms | 121 ms | 84 MB | 84 MB | 84 MB | n/a |
| Goose | 1.50.0 | `goose session` | 315 MB | 315 MB | 164 ms | 247 ms | 1107 ms | 22 ms | 71 MB | 70 MB | 71 MB | n/a |
| Aider | aider 0.86.2 | `aider` | 698 MB | 574 MB | 2718 ms | 3197 ms | 19991 ms | 2126 ms | 158 MB | 154 MB | 158 MB | n/a |

install: bytes the install added on top of the shared base image (sum of its docker layers), runtimes included (Node for the npm install of Codex, uv's CPython for Aider). bundle: the agent's own artifact (a binary, the npm package, the uv tool venv). warm start: fork+exec to the first frame that contains the agent's prompt marker, median and p90 of starts 2 to 10 in one container, page cache warm. cold start: start 1 in that container, before anything is in the page cache. All markers are the first frame with an input line, not a fully loaded agent: wizard's status bar still says connecting tools, Codex still says model loading, Goose is still loading extensions. The pty answers DA1 (VT220, no sixel), DSR, cell size, DECRQM and OSC color queries; no kitty graphics probe was answered in this run. RSS/PSS: VmRSS and Pss summed over the process tree 3 s after the prompt; peak is VmHWM summed the same way; CPU is utime+stime over the same tree, so a slow wall time can be read as work or as waiting.

## Per agent

### wizard

- command: `wizard`, prompt marker: `❯ `
- cold 327 ms, warm (ms): 28, 36, 21, 23, 33, 33, 30, 25, 29
- processes at the prompt: wizard
- terminal queries answered: `ESC[16t`, `ESC[5n`, `ESC[c`
- Default install.sh (binary plus the MCP/subagent loadout). Config points at an OpenAI-compatible provider on 127.0.0.1:9. What the loadout did in this run: the base image has no Node, so the npx spawn for the Playwright MCP failed at once, the first frame carried an MCP error banner, and no Node process or npm lookup is in these numbers.

### Claude Code

- command: `claude`, prompt marker: `❯`
- cold 3394 ms, warm (ms): 1300, 1344, 1242, 1245, 1289, 1285, 1414, 1264, 1156
- processes at the prompt: claude
- terminal queries answered: `ESC[>0q`, `ESC[?2026$p`, `ESC[c`
- Native installer. Onboarding, API-key approval and folder trust pre-answered in ~/.claude.json; dummy ANTHROPIC_API_KEY, so its startup calls to api.anthropic.com get 401.

### Codex CLI

- command: `codex`, prompt marker: `Ask Codex`
- cold 1036 ms, warm (ms): 85, 90, 81, 88, 87, 80, 85, 73, 85
- processes at the prompt: node, codex, git, git, git, git-remote-http
- terminal queries answered: `ESC[6n`, `ESC[c`, `ESC]10;?ESC\`, `ESC]11;?ESC\`
- npm -g install on NodeSource Node 22; node is counted in the install size, the npm package (with its vendored native binary) as the bundle. Provider on 127.0.0.1:9, /work trusted in config.toml.

### OpenCode

- command: `opencode`, prompt marker: `Ask anything`
- cold 9535 ms, warm (ms): 8279, 13625, 7764, 9475, 9120, 8430, 8327, 8000, 7542
- processes at the prompt: opencode, git
- terminal queries answered: `ESCP+q4d73ESC\`, `ESC[14t`, `ESC[6n`, `ESC[>0q`, `ESC[?1004$p`, `ESC[?1016$p`, `ESC[?2004$p`, `ESC[?2026$p`, `ESC[?2027$p`, `ESC[?2031$p`, `ESC[c`, `ESC]10;?`, `ESC]11;?`, `ESC]12;?`, `ESC]4;0;?`, `ESC]4;10;?`, `ESC]4;11;?`, `ESC]4;12;?`, `ESC]4;13;?`, `ESC]4;14;?`, `ESC]4;15;?`, `ESC]4;1;?`, `ESC]4;2;?`, `ESC]4;3;?`, `ESC]4;4;?`, `ESC]4;5;?`, `ESC]4;6;?`, `ESC]4;7;?`, `ESC]4;8;?`, `ESC]4;9;?`
- opencode.ai/install. Custom @ai-sdk/openai-compatible provider on 127.0.0.1:9 in opencode.json.

### Crush

- command: `crush`, prompt marker: `> Ready`
- cold 1077 ms, warm (ms): 185, 170, 189, 304, 194, 203, 203, 204, 211
- processes at the prompt: crush
- terminal queries answered: `ESC[14t`, `ESC[>q`, `ESC[?1004$p`, `ESC[?2026$p`, `ESC[?2027$p`, `ESC[c`
- Charm apt repo. OpenAI-type provider on 127.0.0.1:9, provider auto-update off, project init flag pre-written so the AGENTS.md dialog is skipped.

### Goose

- command: `goose session`, prompt marker: `Enter to send`
- cold 1107 ms, warm (ms): 150, 496, 140, 170, 184, 156, 184, 157, 164
- processes at the prompt: goose
- download_cli.sh with CONFIGURE=false. openai provider with OPENAI_HOST 127.0.0.1:9, telemetry opt-in pre-answered in config.yaml. Measured command is 'goose session' (bare 'goose' prints help).

### Aider

- command: `aider`, prompt marker: `\n> `
- cold 19991 ms, warm (ms): 2693, 2837, 2795, 2425, 3143, 2234, 2587, 3410, 2717
- processes at the prompt: aider
- terminal queries answered: `ESC[6n`
- aider.chat/install.sh (uv tool install, brings its own CPython). Model gpt-4o at 127.0.0.1:9; update check, analytics prompt, release notes and .gitignore prompt turned off in ~/.aider.conf.yml.

## Host

Host state was not recorded for this run; the load guard and this section came later.

