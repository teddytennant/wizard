# Bring your own model (BYOM)

Wizard runs on whatever model you point it at. For hosted models, `/provider add` registers any OpenAI-compatible endpoint or Anthropic (see [getting started](getting-started.md#using-a-cloud-or-remote-provider)); this page covers bringing your own *local* model weights.

The primary BYOM path is the full setup wizard: run `wizard --onboard` (the plain first run offers only the one-pick local path) and pick one of the two BYOM providers — llama.cpp for any GGUF, Ollama for any model tag. Onboarding records the choice; the first run materializes it: a missing known-tier GGUF is downloaded, and a missing Ollama tag is pulled through Ollama's API, both with visible progress. Custom weights are always your call, and Wizard's managed local option (onboarding's Local pick, or `WIZARD_LOCAL=1` at install) downloads only its own hardware-matched tier list — currently the [unsloth](https://huggingface.co/unsloth) GGUF requants of Qwen 3.5/3.6.

## Any GGUF with llama.cpp (the default local backend)

With the llama.cpp default there is no special installer: `llama-server` loads any GGUF directly. Run `wizard --onboard`, pick "BYOM — llama.cpp", and choose "Type a custom GGUF path…" in the model step; GGUFs already in `~/.wizard/models/` are listed first. Or point the provider at the file in `~/.wizard/config.toml` yourself:

```toml
[[providers]]
name = "local"
kind = "llamacpp"
base_url = "http://127.0.0.1:11435"
model = "my-coder-Q4_K_M"
gguf_path = "/home/you/.wizard/models/my-coder-Q4_K_M.gguf"
```

Wizard starts `llama-server` with that file automatically (see [getting started](getting-started.md#first-run)). Override per run with `WIZARD_GGUF_PATH=/path/to/model.gguf` (and `WIZARD_MODEL=<tag>` for the label).

Prefer a model that supports tool calling; Wizard spawns the server with `--jinja` so OpenAI-style tool calls work, and falls back to a prompt-based JSON tool protocol for models without native support.

## Any Ollama tag

Run `wizard --onboard` and pick "BYOM — Ollama". The model picker lists what an existing Ollama install already has pulled first (including models you created yourself), then the hardware-suggested tiers; "Type a custom tag…" takes anything else — a library model from [ollama.com/library](https://ollama.com/library) (`qwen3-coder:30b`, `deepseek-r1:32b`, …) or a registry tag under a user or org namespace (`myorg/internal-coder:latest`).

If the chosen tag is not installed yet, the first `wizard` run pulls it via Ollama's streaming API with a progress bar — no manual `ollama pull` step. This automatic pull only ever targets a local Ollama (loopback `base_url`); see [remote servers](#remote-servers).

Requirements:

- Tool calling is strongly recommended (Wizard probes for it and falls back to the JSON tool protocol without it)
- The chat template must be compatible with Ollama's `/api/chat` endpoint

### Custom models via Modelfile

To run your own weights under Ollama (a fine-tune, a GGUF on disk or Hugging Face), create the model yourself, then pick it in onboarding — it shows up in the picker as already pulled:

```dockerfile
# Modelfile.example
FROM /path/to/your-model.Q4_K_M.gguf
PARAMETER temperature 0.6
PARAMETER num_ctx 131072
SYSTEM You are a careful assistant. Use tools precisely.
```

```bash
ollama create my-coder -f Modelfile.example
wizard --onboard   # pick "BYOM — Ollama", then my-coder
```

A `FROM` line can also point at a Hugging Face URL:

```dockerfile
FROM https://huggingface.co/SomeOrg/SomeModel/resolve/main/model-Q4_K_M.gguf
PARAMETER temperature 0.6
PARAMETER num_ctx 65536
```

### Manual configuration

You can also edit config by hand; Ollama is selected with an explicit provider entry:

```toml
# ~/.wizard/config.toml
mode = "genie"

[[providers]]
name = "local"
kind = "ollama"
base_url = "http://127.0.0.1:11434"
model = "my-custom-model"
```

Verify the model works:

```bash
ollama run my-custom-model "write a hello world in Rust"
wizard -p "list files in the current directory"
```

## The installer's BYOM flavor

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_BYOM=1 bash
```

With `WIZARD_BYOM=1`, the installer:

1. Installs Ollama if needed and starts it
2. Installs the `wizard` binary (same as the default flavor)
3. Lays down the [default loadout](loadout.md) (browser MCP + subagents), each file only if absent

No model is chosen at install time: the first `wizard` run opens onboarding, and the tag you pick there is pulled on first run. For headless/non-interactive installs, set `WIZARD_MODEL=<tag>`: the installer pulls that tag and writes the config itself (a fresh config gets a full `[[providers]]` Ollama entry; an existing config keeps everything else and only has its `model =` line(s) updated), so no onboarding is needed.

The old `install-byom.sh` URL is still wired up: it is a thin shim that fetches `install.sh` and runs it with `WIZARD_BYOM=1`, passing all other `WIZARD_*` variables through (plus the shim-only `WIZARD_INSTALLER_REF` to pick which ref to fetch `install.sh` from, default `main`).

## Model requirements

Wizard expects models that support:

| Capability | Required | Notes |
|------------|----------|-------|
| Tool calling | Recommended | Native preferred; Wizard falls back to a prompt-based JSON tool protocol when a model lacks native support |
| Streaming chat | Yes | llama-server's `/v1/chat/completions` or Ollama's `/api/chat` |
| Context ≥ 32K | Recommended | 128K+ preferred for large codebases. Note that a `llama-server` Wizard spawns itself is started with `--ctx-size 16384` regardless; a server you run yourself, or Ollama, is not capped by Wizard |
| Code quality | Recommended | Coding-oriented models perform best |

Native tool calling varies by model, so Wizard probes for it at startup: models that advertise tools support get native function calling, others fall back to a prompt-based JSON tool protocol. A probe that fails outright (server not up yet, a proxy that answers oddly) also falls back, so a native-capable model behind a flaky endpoint can end up on the weaker path for the session. The JSON path is less reliable on weaker models, so prefer one with solid native tool calling.

### Remote servers

Point Wizard at a model server on another machine and it connects instead of spawning (Wizard only starts `llama-server` for loopback URLs):

```toml
[[providers]]
name = "gpu-box"
kind = "llamacpp"
base_url = "http://gpu-server.local:8080"
model = "Qwen3.6-27B-Q4_K_M"
```

For a remote Ollama instance, use an explicit provider entry too:

```toml
[[providers]]
name = "gpu-box"
kind = "ollama"
base_url = "http://gpu-server.local:11434"
model = "qwen3.6:27b"
```

The automatic first-run pull applies only to loopback URLs — Wizard never downloads models onto a remote server. Ensure the model is loaded/pulled on that server yourself.

## Disclaimer

BYOM lets you choose any GGUF or Ollama-compatible model. Wizard does not ship or maintain model weights, including the ones its own tier list points at — those are third-party requants hosted on Hugging Face. You are responsible for compliance with the model's license and acceptable use terms.

The default `install.sh` one-liner downloads no model weights; Wizard's managed local setup downloads only the tiers listed in `src/hardware.rs`.

## Switching back to the managed tiers

Run `wizard --onboard` and pick the recommended tier — it is downloaded (llama.cpp) or pulled (Ollama) on the next run. Or re-run the installer with the local flavor: it downloads the VRAM-matched GGUF and leaves an existing config untouched, so update `model` / `gguf_path` in the provider entry afterwards (this needs `WIZARD_BUILD_FROM_SOURCE=1` today, per the note above):

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_LOCAL=1 bash
```
