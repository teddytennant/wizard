# Getting started

Wizard is a Rust terminal UI whose backend is the NexAU code agent (run over a Python
bridge). You build it from source, point it at a model provider once through onboarding,
and chat.

## Prerequisites

- A **Rust** toolchain, 2024 edition (`rustup` recommended).
- [**uv**](https://github.com/astral-sh/uv) for the Python bridge environment.
- A model provider: an xAI / OpenAI / OpenRouter key (or xAI account sign-in), or a local
  OpenAI-compatible server (llama.cpp / Ollama).

## Build

```bash
git clone https://github.com/teddytennant/wizard
cd wizard

uv sync                  # builds .venv with NexAU pinned (from pyproject.toml)
cargo build --release    # → ./target/release/wizard
# or: cargo install --path .
```

> **Keep the checkout in place.** The binary finds the bridge script
> (`backend/nexau_bridge.py`), the virtualenv (`.venv`), and the agent definition
> (`agent/`) relative to the crate's build-time path. If you move or delete the repo after
> `cargo install`, the bridge won't launch. You can override the bridge interpreter/script
> with `python` / `bridge_script` in `~/.wizard/config.toml`.

> The repo's `install.sh` curl one-liner targets the old prebuilt-binary design and does
> not apply to this build; it is being reworked for the build-from-source + uv flow above.

## First run

The first launch (or `wizard --onboard` at any time) opens a two-stage setup:

1. **Pick a provider preset:**
   - **xAI (sign in with your account)** — OAuth, no API key
   - **xAI (Grok) — API key**
   - **OpenAI**
   - **OpenRouter**
   - **Local (llama.cpp / Ollama)** — no key
   - **Custom (OpenAI-compatible)**
2. **Confirm the endpoint, wire API, model, key, and working directory.** The form is
   prefilled from the preset. For xAI sign-in, the browser OAuth flow runs instead of a
   key prompt.

Setup writes `~/.wizard/config.toml`. Then you're in the chat TUI.

```bash
wizard                       # current directory is the agent's working dir
wizard --cwd /path/to/proj   # run the agent from a different directory
wizard --model grok-4        # override the model for this session
wizard --onboard             # re-run setup
```

## Configuration

`~/.wizard/config.toml` (written by onboarding):

```toml
model    = "grok-4.3"
base_url = "https://api.x.ai/v1"
api_type = "openai_chat_completion"
api_key  = "sk-..."                # or: api_key_env = "XAI_API_KEY"
auth     = "api_key"               # or "xai_oauth"
workdir  = "/path/to/project"
mode     = "genie"
```

- **`api_type`** (NexAU's `LLM_API_TYPE`): `openai_chat_completion` (default),
  `openai_responses`, `anthropic_chat_completion`, or `gemini_rest`.
- A key is read from `api_key_env` (an env var) when set, otherwise from inline `api_key`.
  Local endpoints need none.

### xAI account sign-in

Instead of a key, sign in with your xAI account:

```bash
wizard login xai     # PKCE browser flow; tokens refresh automatically
```

Onboarding's "xAI (sign in with your account)" preset runs the same flow and sets
`auth = "xai_oauth"`.

### Local models

Pick **Local (llama.cpp / Ollama)** in onboarding and point `base_url` at your server's
OpenAI-compatible endpoint (e.g. `http://127.0.0.1:8080/v1` for llama.cpp's `llama-server`,
or `http://127.0.0.1:11434/v1` for Ollama). Wizard does **not** download models or manage
a server — start your own and give Wizard the URL and model tag.

## First chat

Type a request and press Enter. The agent streams its thinking and text, and each shell
command shows up as a tool card. Useful keys and commands:

- `/help` — list commands and keys
- `/model [tag]` — show or switch the model
- `/clear` — reset the conversation
- `/quit` — exit
- `@path` — inline a file's contents into your prompt (Tab completes the path)

See [commands.md](commands.md) for custom `/commands` and `@file` references, and
[ahe-evolve.md](ahe-evolve.md) for self-evolve.

## Troubleshooting

- **"bridge failed to start" / "bridge closed before it was ready":** check
  `~/.wizard/logs/bridge.log`. Usually a missing `uv sync` (no `.venv`), a moved checkout,
  or bad credentials.
- **Onboarding reopens on every launch:** the config can't authenticate (e.g. an
  `api_key_env` var that isn't set, or no key). Re-run `wizard --onboard` or fix the key.
- **Incompatible config warning:** a pre-NexAU `config.toml` is ignored and setup re-runs;
  let it rewrite the file.
