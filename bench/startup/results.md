# Startup benchmark

Measured 2026-09-11 on 11th Gen Intel(R) Core(TM) i7-11700K @ 3.60GHz, 16 CPUs, kernel 7.0.0, Docker version 29.4.0, build v29.4.0. One ubuntu:24.04 container per agent, 10 pty starts each, container network bridge.

Rule for every agent: installed as its README says, default config, phone-home switched off where the agent offers a switch, model endpoint pointed at a dead local port (127.0.0.1:9). What each one was told is under its row.

| agent | version | command | install | bundle | warm start (median) | warm p90 | cold start | `--version` | RSS | PSS | peak RSS | CPU at 3 s |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| wizard | wizard 3.1 | `wizard` | 26 MB | 26 MB | 6 ms | 7 ms | 135 ms | 1 ms | 20 MB | 19 MB | 20 MB | 0 ms |
| Claude Code | 2.1.268 (Claude Code) | `claude` | 219 MB | 219 MB | 268 ms | 277 ms | 1035 ms | 8 ms | 241 MB | 240 MB | 256 MB | 600 ms |
| Codex CLI | codex-cli 0.154.0 | `codex` | 553 MB | 339 MB | 34 ms | 36 ms | 511 ms | 33 ms | 179 MB | 178 MB | 179 MB | 140 ms |
| OpenCode | 1.18.30 | `opencode` | 185 MB | 185 MB | 2847 ms | 2868 ms | 6230 ms | 506 ms | 786 MB | 785 MB | 961 MB | 6810 ms |
| Crush | crush version v0.93.1 | `crush` | 96 MB | 95 MB | 80 ms | 82 ms | 445 ms | 45 ms | 81 MB | 81 MB | 81 MB | 130 ms |
| Goose | 1.50.0 | `goose session` | 315 MB | 315 MB | 54 ms | 57 ms | 414 ms | 5 ms | 72 MB | 71 MB | 72 MB | 70 ms |
| Aider | aider 0.86.2 | `aider` | 664 MB | 574 MB | 822 ms | 831 ms | 6747 ms | 643 ms | 200 MB | 196 MB | 200 MB | 3260 ms |

install: bytes the install added on top of the shared base image (sum of its docker layers), runtimes included (Node for the npm install of Codex, uv's CPython for Aider). bundle: the agent's own artifact (a binary, the npm package, the uv tool venv). warm start: fork+exec to the first frame that contains the agent's prompt marker, median and p90 of starts 2 to 10 in one container, page cache warm. cold start: start 1 in that container, before anything is in the page cache. All markers are the first frame with an input line, not a fully loaded agent: wizard's status bar still says connecting tools, Codex still says model loading, Goose is still loading extensions. The pty answers DA1 (VT220, no sixel), DSR, cell size, DECRQM and OSC color queries, and says yes to the kitty graphics probe. RSS/PSS: VmRSS and Pss summed over the process tree 3 s after the prompt; peak is VmHWM summed the same way; CPU is utime+stime over the same tree, so a slow wall time can be read as work or as waiting.

## Per agent

### wizard

- command: `wizard`, prompt marker: `❯ `
- cold 135 ms, warm (ms): 6, 6, 6, 6, 6, 7, 6, 6, 6
- processes at the prompt: wizard
- terminal queries answered: `ESC[16t`, `ESC[5n`, `ESC[c`, `ESC_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAAESC\`
- Default install.sh: the binary plus the loadout (~/.wizard/mcp.toml with a Playwright MCP run through npx, and four subagent files). The base image has no Node, so at every start that npx spawn fails at once and the first frame carries an MCP error banner; no Node process is in the tree and no npm lookup happens. On a machine with Node the same start forks npx and resolves @playwright/mcp against the registry, which is not measured here. Startup release check off via [update] notify = false.

### Claude Code

- command: `claude`, prompt marker: `❯`
- environment: `ANTHROPIC_BASE_URL=http://127.0.0.1:9 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_AUTOUPDATER=1`
- cold 1035 ms, warm (ms): 271, 298, 268, 266, 269, 266, 267, 267, 267
- processes at the prompt: claude
- terminal queries answered: `ESC[>0q`, `ESC[?2026$p`, `ESC[c`
- Native installer pinned to 2.1.268. Onboarding, API-key approval and folder trust pre-answered in ~/.claude.json. Dummy ANTHROPIC_API_KEY with ANTHROPIC_BASE_URL on the dead port; CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 and DISABLE_AUTOUPDATER=1 are its documented switches for the update check, feature flags and remote settings fetch.

### Codex CLI

- command: `codex`, prompt marker: `Ask Codex`
- cold 511 ms, warm (ms): 34, 40, 33, 33, 34, 35, 34, 34, 33
- processes at the prompt: node, codex
- terminal queries answered: `ESC[6n`, `ESC[c`, `ESC]10;?ESC\`, `ESC]11;?ESC\`
- npm -g install pinned to @openai/codex@0.154.0 on NodeSource Node 22; node is counted in the install size, the npm package (with the vendored native codex, codex-code-mode-host and rg binaries for this platform) as the bundle. The process tree is a node wrapper in front of the native binary, which is not there when Codex is installed from its GitHub release or Homebrew. Provider on 127.0.0.1:9, /work trusted, update check, analytics and plugin marketplace sync off in config.toml.

### OpenCode

- command: `opencode`, prompt marker: `Ask anything`
- environment: `OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_MODELS_FETCH=1`
- cold 6230 ms, warm (ms): 2867, 2854, 2849, 2815, 2820, 2847, 2825, 2835, 2868
- processes at the prompt: opencode
- terminal queries answered: `ESCP+q4d73ESC\`, `ESC[14t`, `ESC[6n`, `ESC[>0q`, `ESC[?1004$p`, `ESC[?1016$p`, `ESC[?2004$p`, `ESC[?2026$p`, `ESC[?2027$p`, `ESC[?2031$p`, `ESC[c`, `ESC]10;?`, `ESC]11;?`, `ESC]12;?`, `ESC]4;0;?`, `ESC]4;10;?`, `ESC]4;11;?`, `ESC]4;12;?`, `ESC]4;13;?`, `ESC]4;14;?`, `ESC]4;15;?`, `ESC]4;1;?`, `ESC]4;2;?`, `ESC]4;3;?`, `ESC]4;4;?`, `ESC]4;5;?`, `ESC]4;6;?`, `ESC]4;7;?`, `ESC]4;8;?`, `ESC]4;9;?`, `ESC_Gi=31337,s=1,v=1,a=q,t=d,f=24;AAAAESC\`
- opencode.ai/install pinned to 1.18.30. Custom @ai-sdk/openai-compatible provider on 127.0.0.1:9 in opencode.json, autoupdate off there and via OPENCODE_DISABLE_AUTOUPDATE, the models.dev catalog fetch off via OPENCODE_DISABLE_MODELS_FETCH. No telemetry switch is documented.

### Crush

- command: `crush`, prompt marker: `> Ready`
- environment: `CRUSH_DISABLE_PROVIDER_AUTO_UPDATE=1 CRUSH_DISABLE_METRICS=1`
- cold 445 ms, warm (ms): 80, 78, 85, 78, 81, 79, 81, 77, 80
- processes at the prompt: crush
- terminal queries answered: `ESC[14t`, `ESC[>q`, `ESC[?1004$p`, `ESC[?2026$p`, `ESC[?2027$p`, `ESC[c`, `ESC_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAAESC\`
- Charm apt repo pinned to crush=0.93.1. OpenAI-type provider on 127.0.0.1:9; Catwalk catalog fetch and metrics off both in crush.json and by environment; project init flag pre-written so the AGENTS.md dialog is skipped. The input placeholder is random (Ready for instructions, Ready..., Ready!), hence the marker.

### Goose

- command: `goose session`, prompt marker: `Enter to send`
- cold 414 ms, warm (ms): 48, 49, 58, 56, 55, 55, 53, 50, 52
- processes at the prompt: goose
- download_cli.sh pinned to v1.50.0 with CONFIGURE=false. openai provider with OPENAI_HOST 127.0.0.1:9; GOOSE_TELEMETRY_ENABLED false in config.yaml is its one switch. Measured command is 'goose session' because bare 'goose' prints help.

### Aider

- command: `aider`, prompt marker: `\n> `
- cold 6747 ms, warm (ms): 830, 820, 818, 830, 821, 823, 821, 831, 820
- processes at the prompt: aider
- terminal queries answered: `ESC[6n`
- uv tool install pinned to aider-chat==0.86.2 (PyPI; the GitHub releases page lags) with its own CPython 3.12. Model gpt-4o at 127.0.0.1:9; update check, analytics prompt, release notes and .gitignore prompt turned off in ~/.aider.conf.yml. The cold start includes aider's first-run synchronous import warm-up (it says so with --verbose) and the litellm model table fetch from raw.githubusercontent.com; the warm number is Python import time.

## Host

Load average at start 1.79 1.69 1.87 on 16 CPUs, PSI cpu `some avg10=0.00 avg60=0.01 avg300=0.01 total=49993023730`, 22.0 GB available of 33.4 GB, swap in use 1.4 GB, governor performance, RAPL package cap 65.0 W. run.sh refuses to start above load nproc/4 or under 8 GB available unless LOAD_OK=1. Page cache dropped before each agent's run 1 (DROP_CACHES=1).

