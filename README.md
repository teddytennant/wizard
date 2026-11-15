# Wizard

The fastest agent in your terminal. One Rust binary, any model, a desktop app, and a full harness that still finishes your task before the minimal ones do.

[![Wizard 3.5: the desktop app building Conway's Game of Life, the diff, and the result running in a terminal](demo/wizard-3.5.webp)](demo/wizard-3.5.mp4)

A 21 second cut. The [full two minutes](demo/wizard-3.5.mp4) has sound.

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

That's the install on macOS too. A release goes out most nights, and `wizard update` pulls it in place. Homebrew (`brew install teddytennant/tap/wizard`) works if you'd rather `brew upgrade`. Nix (`nix run github:teddytennant/wizard`), local models, Termux and building from source are in [Getting started](docs/getting-started.md#install).

## Faster where you wait

pi is about as lean as an agent gets: four tools and a short prompt, which makes it close to the floor on tokens. Wizard carries subagents, MCP, memory, skills, checkpoints, a completion review and a clock on top of that. On the same tasks with the same model it finished every one faster, used 43% fewer output tokens, and cost less than pi on the two longest.

<!-- BENCH:pi -->
| agent | passed | wall time per task | prompt tokens | output tokens | est. cost per task |
|---|---:|---:|---:|---:|---:|
| **wizard 3.5** | **16/16** | **262 s** | 522k | **12.2k** | $0.54 |
| pi 0.87.1 | 16/16 | 460 s | **377k** | 21.6k | **$0.43** |
<!-- /BENCH -->

Wall time is what you feel. Wizard was 1.8x faster on average and up to 3x on a single task. pi sends fewer prompt tokens, but most of Wizard's input is cache reads at a quarter of the price and its output is the expensive part it writes less of, so 38% more prompt tokens came out 25% more money on average and less than pi on the two longest tasks. The 8 tasks come from a real project's history: a commit that adds failing tests, then the commit that makes them pass. Two tries each on Grok 4.6, graded on the original tests. Wizard's row predates 3.5 deferring MCP tools, so the shipped default sends less than it shows. [Harness engineering](docs/harness-engineering.md) has how the defaults were found and [Token profiles](docs/token-profiles.md) has `--token-profile min`, whose first request (1,724 tokens) is smaller than pi's (1,817).

## Faster to start

<!-- BENCH:startup -->
| agent | warm start | cold start | RSS at the prompt | install |
|---|---:|---:|---:|---:|
| wizard 3.1 | 6 ms | 135 ms | 20 MB | 26 MB |
| Codex CLI 0.154.0 | 34 ms | 511 ms | 179 MB | 553 MB |
| Goose 1.50.0 | 54 ms | 414 ms | 72 MB | 315 MB |
| Crush 0.93.1 | 80 ms | 445 ms | 81 MB | 96 MB |
| Claude Code 2.1.268 | 268 ms | 1035 ms | 241 MB | 219 MB |
| Aider 0.86.2 | 822 ms | 6747 ms | 200 MB | 664 MB |
| OpenCode 1.18.30 | 2847 ms | 6230 ms | 786 MB | 185 MB |
<!-- /BENCH -->

Ten pty starts per agent in its own ubuntu:24.04 container, page cache dropped before the first. [How it was measured](bench/startup/results.md).

## Desktop app

Wizard GUI runs Wizard, Pi and Claude Code side by side across your projects and on machines you reach over SSH. It can reuse a ChatGPT or Grok sign-in it finds on disk. Installers for Linux, macOS and Windows come with every [release](https://github.com/teddytennant/wizard/releases), and `wizard gui` opens it. It is a fork of [Zeron](https://github.com/zeronsh/zeron). [gui/README.md](gui/README.md)

## Android

On your phone, open **[wizard-android.apk](https://github.com/teddytennant/wizard/releases/latest/download/wizard-android.apk)** and install it. It drives Wizard, Pi and Claude Code on your own machines over SSH, with a notification when a turn finishes. [android/README.md](android/README.md)

## Terminal-Bench

<!-- BENCH:tbench -->
| agent | model | resolved of 89 |
|---|---|---:|
| wizard 3.1.1 | Grok 4.6 | 66 (74.2%) |
| Terminus 2, same box | Grok 4.6 | 69 (77.5%) |
| Grok Build 1.0.24, same box | Grok 4.6 | 69 (77.5%) |
| Terminus 2, public reference (Artificial Analysis, on e2b) | Grok 4.6 | 88.4% |
<!-- /BENCH -->

One trial per task with every public copy of the benchmark blocked. [Per-task results](tbench/RESULTS.md).

## What else is in the box

Any model (xAI, OpenAI and ChatGPT sign-in, Anthropic, Gemini, DeepSeek, OpenRouter, Ollama, llama.cpp, anything OpenAI-compatible). A TUI, headless `wizard -p`, and `--continuous` missions. `/fusion` and `/ultra` for panels of models. `/evolve` for skills, MCP servers, Lua tools and subagents that go live on `/reload`. MCP in both directions, ACP for Zed, Neovim and Emacs, a Telegram gateway, and plain-markdown memory. Every page is in [docs/README.md](docs/README.md).

## Limits

No sandbox: tools run with your privileges, so read [SECURITY.md](SECURITY.md) before an autonomous run. Windows runs it under WSL2. Small local models misformat tool calls more than frontier ones.

## Build

```bash
git clone https://github.com/teddytennant/wizard && cd wizard
cargo build --release
```

## License

`MIT AND Apache-2.0`. Wizard's own code is MIT ([LICENSE-MIT](LICENSE-MIT)); terminal-UI code ported from OpenAI Codex and xAI grok-build stays Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE), [NOTICE](NOTICE)). Wizard GUI is MIT, from Zeron.
