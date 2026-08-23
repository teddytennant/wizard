# Getting started

Wizard installs from one command and launches as a terminal UI agent. The default install puts down the binary and the [default loadout](loadout.md) (no model, no config); the first `wizard` run opens [onboarding](#first-run) to pick a provider. Local is one pick: Wizard detects your hardware, downloads a fitting GGUF, and sets up [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server` itself (or reuses an existing Ollama install), so no API key is needed. Or bring a key for any OpenAI-compatible endpoint (OpenAI, OpenRouter, Cloudflare Workers AI, Groq, vLLM, LM Studio, llama.cpp, Ollama), Anthropic, or xAI (API key or account sign-in). See [Using a cloud or remote provider](#using-a-cloud-or-remote-provider) and [Using Ollama instead](#using-ollama-instead).

## Install

The one-liner is:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

The installer:

1. Detects your OS and CPU architecture
2. Downloads the `wizard` binary from GitHub releases (or from a [mirror](#the-download-mirror), if you set one, falling back to GitHub), verifies the release's `checksums.txt` against its minisign signature (`checksums.txt.minisig`) under the public key inlined in the script, and verifies the tarball's SHA-256 against that file. Every failure aborts the install: no signature, a bad one, one from another key, no `checksums.txt`, no entry in it for that asset, a digest mismatch, or a host with neither `sha256sum` nor `shasum`. Checking a signature needs `minisign`, an OpenSSL that does ed25519 and blake2b, or `python3`, and the installer refuses rather than skip the check when it finds none of them. It looks past PATH for the first two (Homebrew's `openssl@3` is keg-only and never linked onto PATH), and the `python3` path is what carries macOS, where `openssl` is LibreSSL and does neither half of the check. `wizard update` applies the same rules with the key compiled in, so it needs no external tool. See [SECURITY.md](../SECURITY.md#release-signing)
3. Lays down the [default loadout](loadout.md): `~/.wizard/mcp.toml` (Playwright browser MCP) and `~/.wizard/subagents/*.toml` (reviewer, researcher, tester, documenter), each file only if it is not already present

It installs no model and writes no config; the first `wizard` run starts onboarding. To preinstall the local stack instead (non-interactive), set `WIZARD_LOCAL=1`:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_LOCAL=1 bash
```

With `WIZARD_LOCAL=1` the installer additionally:

1. Installs `llama-server` if it is not already on your `PATH`: on an NVIDIA GPU with `nvcc` present it compiles a CUDA build from source (llama.cpp publishes no Linux CUDA binary; skip with `WIZARD_LLAMACPP_NO_CUDA=1`), otherwise it downloads an official llama.cpp release (Vulkan build when a GPU and Vulkan loader are present, CPU build as the fallback). Either way the install lands in `~/.wizard/llama.cpp/` with a symlink at `~/.wizard/bin/llama-server`
2. Selects a model tier based on available VRAM
3. Downloads the matching Qwen 3 GGUF (Q4_K_M) from Hugging Face into `~/.wizard/models/` (resumable; re-running picks up where it left off)
4. Writes `~/.wizard/config.toml` (an existing config is never touched)

The installer does **not** start a model server; Wizard starts `llama-server` itself on first run.

### Install flavors

The same script has four mutually exclusive flavors:

| Install | What you get |
|---------|--------------|
| (default) | binary + loadout; no model, no config. The first `wizard` run starts [onboarding](#first-run) |
| `WIZARD_LOCAL=1` | the default plus a preinstalled local stack: llama.cpp runtime + VRAM-tiered Qwen GGUF + `config.toml` |
| `WIZARD_MINIMAL=1` | binary only: no loadout either; onboarding on first run as with the default |
| `WIZARD_BYOM=1` | Ollama runtime + binary + loadout; model choice happens in onboarding, which pulls the tag you pick on first run (or set `WIZARD_MODEL=<tag>` to pull + write the config headlessly); see [byom.md](byom.md) |

`WIZARD_USE_OLLAMA=1` is the Ollama variant of the local flavor (installs Ollama, starts it, pulls the same auto-tiered model) and implies it: no need to also set `WIZARD_LOCAL`. Combining `WIZARD_LOCAL`, `WIZARD_MINIMAL`, or `WIZARD_BYOM` is an error. `WIZARD_BESPOKE=1` is a deprecated alias for `WIZARD_MINIMAL=1`; it's stricter than the old bespoke flavor, which still installed the model runtime. Minimal installs nothing but the binary and leaves everything to onboarding.

### Platforms

| Platform | Notes |
|----------|-------|
| Linux x86_64 / aarch64 | Prebuilt glibc and static-musl binaries; the installer prefers musl on NixOS |
| macOS Apple Silicon / Intel | Same `curl … \| bash`; prebuilt binaries for both architectures; Metal-backed `llama-server` for the local stack |
| Termux (Android) | Supported via on-device **source build** into `$PREFIX/bin`. No matching prebuilt release (Bionic libc). The native GUI, stock llama.cpp Ubuntu assets, and the Ollama curl installer are skipped; use a cloud provider, or put a Termux-built `llama-server` on `PATH` |
| Windows | Not supported natively; use WSL2 |

The installer downloads the prebuilt binary matching your OS and architecture, verifies the release signature and the binary's checksum, and falls back to a source build when no prebuilt asset is available. On Termux it always builds from source (no matching prebuilt). Everywhere else the one-liner installs a signed release binary.

### Termux (Android)

Wizard runs as the TUI inside [Termux](https://termux.dev). There is no Android APK and no `aarch64-linux-android` release asset in any release; the phone compiles Wizard itself. The one-liner still works: Termux always takes the source-build path.

```bash
pkg install rust git clang make pkg-config openssl curl
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

What the installer does on Termux:

1. Detects Termux (`TERMUX_VERSION` / `$PREFIX`) and sets `WIZARD_INSTALL_DIR=$PREFIX/bin` (no `sudo`)
2. Forces `WIZARD_BUILD_FROM_SOURCE=1` and skips prebuilt download, `WIZARD_NATIVE`, stock llama.cpp assets, and the Ollama curl installer
3. Clones the repo, runs `cargo build --release`, and installs `wizard` next to your other Termux packages

First run opens onboarding: pick a **cloud** provider (API key or sign-in). On-device GGUF via the one-click Local path is not wired to a Termux-native llama.cpp build yet; if you already have a Termux-built `llama-server` on `PATH` (or in `~/.wizard/bin`), Wizard will use it.

Update by re-running the same one-liner: on Termux it always builds from source, so it clones the newest release tag afresh and reinstalls over the old binary. The installer's own clone lives in a `mktemp -d` directory that its exit trap deletes, so there is nothing to `git pull` afterwards — if you want a checkout that stays put, clone one yourself and rebuild in it:

```bash
git clone https://github.com/teddytennant/wizard ~/wizard-src   # once
cd ~/wizard-src && git pull && cargo build --release \
  && install -m755 target/release/wizard "$PREFIX/bin/wizard"
```

`wizard update` will not swap in a glibc Linux prebuilt on Termux; it builds the tag from source instead.

### Nix / NixOS

Wizard ships a flake, so on Nix you don't need the install script at all:

```bash
nix run github:teddytennant/wizard              # run without installing
nix profile install github:teddytennant/wizard  # add to your profile
```

The flake exposes `packages.default` (and `.wizard`), `apps.default`, `devShells.default` (Rust toolchain + `llama-cpp` for hacking on Wizard, plus the X11/Wayland libraries `--features native` opens a window with), `overlays.default`, and `homeModules.default` for wiring it into a Home Manager config (it was called `homeManagerModules.default` before 2.0.0, which is not an output name Nix recognizes, so `nix flake check` skipped it; that spelling is still exported as an alias, so an existing import keeps working). On NixOS the curl installer detects the system, points you at these commands, and, if you run it anyway, prefers the musl asset into `~/.local/bin` rather than `/usr/local/bin` (which isn't on the FHS path there). The published musl assets are not yet statically linked, so that prebuilt does not start; `wizard update` then builds the tag from source instead of leaving you on the old binary.

`packages.default` builds with default features, so it does **not** carry the native GUI — build one from a checkout of the flake with `cargo build --release --features native` inside `nix develop`.

### The GUI

Wizard has one graphical surface, `wizard gui`, and it needs a build with `--features native`. The browser GUI that used to be the other one — a loopback HTTP server on port 4680 and a JavaScript page — is deleted.

That leaves four ways to drive a machine you are not sitting at, none of which needs a window or a port: run the TUI over SSH, run `wizard -p '<prompt>'`, point an ACP editor at `wizard acp` over the same SSH connection, or run the [Telegram gateway](gateway.md) on the box.

The window is a separate build because iced is several hundred crates that a `wizard -p`, a `wizard acp` or a CI container never executes a line of, and because it cannot be linked into the static musl binary. Two ways to get one:

```bash
# a second binary beside `wizard`, from the release assets
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
  | WIZARD_NATIVE=1 bash
wizard-native gui

# or from a checkout
cargo build --release --features native
./target/release/wizard gui
```

`WIZARD_NATIVE=1` installs `wizard-native` next to `wizard` and never replaces it, so the plain binary keeps its runs-anywhere promise. The asset exists for glibc Linux and macOS on both architectures; there is no musl or Termux build of it. A plain binary asked for `wizard gui` says it has no window and prints these lines. `--native` is still accepted and ignored, so an alias written when there were two GUIs still opens the one there is. See [Native GUI](native-gui.md).

`WIZARD_NATIVE=1` downloads that asset; combined with `WIZARD_BUILD_FROM_SOURCE=1` it builds the window from the same checkout instead, and so does the `cargo build --release --features native` line above.

### Model tiers (automatic)

Picking Local in onboarding and the `WIZARD_LOCAL=1` / `WIZARD_USE_OLLAMA=1` flavors size the model to your hardware:

| Memory budget | GGUF downloaded | Ollama tag | Approx. size |
|---------------|-----------------|------------|--------------|
| ≥ 24 GB | `Qwen3.6-35B-A3B-UD-Q4_K_M.gguf` | `qwen3.6:35b` | ~20 GB (MoE, 36B total / 3B active: fast, but all weights must fit in memory) |
| 18–24 GB | `Qwen3.6-27B-Q4_K_M.gguf` | `qwen3.6:27b` | ~16 GB (dense) |
| 8–18 GB | `Qwen3.5-9B-Q4_K_M.gguf` | `qwen3.5:9b` | ~6 GB |
| < 8 GB | `Qwen3.5-4B-Q4_K_M.gguf` | `qwen3.5:4b` | ~3 GB |

Tiers are ordered so the model's total footprint fits in available memory. An MoE model still needs all expert weights resident, which is why the 35B lands in the top tier despite its small active-parameter count.

The 4B tier is the floor, and it exists because a 9B needs roughly 8 GB once `llama-server`'s KV cache and compute buffers are counted on top of the weights. An 8 GB laptop (about 7 GB usable after the kernel's reservation) cannot load one, so below the 8 GB boundary Wizard picks the 4B rather than handing you a model that gets OOM-killed on startup. Expect a 4B to need more steering than the larger tiers: it is the difference between a local option and no local option, not a peer of the 27B.

Wizard also refuses to start `llama-server` on a model that cannot fit: before spawning it checks the model's size against usable RAM, keeping 2 GB of headroom, and if it does not fit it names the largest tier that does (or tells you local inference will not work on this machine and points at the cloud providers, when nothing fits).

The budget is detected as: GPU VRAM via `nvidia-smi` for NVIDIA, `rocm-smi` for AMD, then the amdgpu sysfs counter (`/sys/class/drm/card*/device/mem_info_vram_total`); then Apple Silicon's unified memory (`sysctl hw.memsize`), where the GPU addresses the same pool as the CPU; then plain system RAM. On Linux the *system RAM* reading is capped by the cgroup memory limit when one is smaller, so a CPU-only container is tiered to its own limit rather than the host's RAM. If nothing can be detected at all, Wizard falls back to the smallest tier; override with `WIZARD_MODEL=<tag>`.

**One extra cap applies inside the binary, and `install.sh` is the exception.** When VRAM is the reading, both of onboarding's pickers cap it by usable system RAM, because the weights are staged through system memory while loading whichever runtime ends up holding them: a 24 GB card in an 8 GB host gets an 8 GB budget and the 9B row, and the explanation line reads "Detected 24 GB of GPU VRAM (nvidia-smi), capped by 8 GB of system RAM → …". The Ollama tag picker and the llama.cpp GGUF picker apply the same cap and produce the same tier. The usable-RAM figure the cap uses is the cgroup-capped one, so a GPU container with a 4 GB memory limit is tiered to 4 GB on both. When even the smallest tier does not fit the capped budget, both explanations end with "local inference will not work on this machine, so pick a cloud provider instead" rather than presenting the row as a recommendation.

`install.sh` does **not** do any of that. Its `detect_memory` returns the moment `nvidia-smi`, `rocm-smi`, or the amdgpu sysfs counter answers, before it ever reaches the `/proc/meminfo` branch that carries the cgroup cap, so a raw VRAM figure is what picks the tier. On a big-card, small-RAM host (or in a GPU container with a small cgroup limit), `WIZARD_LOCAL=1` / `WIZARD_USE_OLLAMA=1` selects the top tier and fetches ~20 GB that Wizard's own preflight then refuses to load. Set `WIZARD_MODEL` explicitly when installing on such a machine, or install with `WIZARD_SKIP_MODEL_PULL=1` and let `wizard --onboard` pick the capped tier.

### Installer environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `WIZARD_INSTALL_DIR` | `/usr/local/bin` (`~/.local/bin` on NixOS, `$PREFIX/bin` on Termux) | Where to place the `wizard` binary |
| `WIZARD_VERSION` | latest release | Release tag to install, e.g. `v2.0.0`; pin it for reproducible installs or to roll back to an earlier release. Also the ref the source build clones |
| `WIZARD_LOCAL` | `0` | Set to `1` to preinstall the llama.cpp stack and an auto-tiered model (conflicts with `WIZARD_MINIMAL` and `WIZARD_BYOM`) |
| `WIZARD_MINIMAL` | `0` | Set to `1` for the binary-only install; first run launches onboarding |
| `WIZARD_BYOM` | `0` | Set to `1` to set up Ollama and bring your own model, picked in onboarding unless `WIZARD_MODEL` is set (conflicts with `WIZARD_MINIMAL` and `WIZARD_LOCAL`) |
| `WIZARD_BESPOKE` | `0` | Deprecated alias for `WIZARD_MINIMAL` |
| `WIZARD_MODEL` | auto-detected | Local flavors: force a model tier (`qwen3.6:35b`, `qwen3.6:27b`, `qwen3.5:9b`, `qwen3.5:4b`); with `WIZARD_BYOM=1`, pull this tag and write the config instead of deferring to onboarding |
| `WIZARD_SKIP_MODEL_PULL` | `0` | Local flavors: set to `1` to skip the model download |
| `WIZARD_SKIP_LLAMACPP_INSTALL` | `0` | With `WIZARD_LOCAL=1`: set to `1` if `llama-server` is managed elsewhere |
| `WIZARD_LLAMACPP_NO_CUDA` | `0` | Set to `1` to never compile a CUDA `llama-server`; use the prebuilt Vulkan/CPU build instead |
| `WIZARD_USE_OLLAMA` | `0` | Set to `1` for the Ollama variant of the local flavor (implies `WIZARD_LOCAL`) |
| `WIZARD_SKIP_OLLAMA_INSTALL` | `0` | With Ollama flavors: Ollama is already managed elsewhere |
| `WIZARD_WITH_TOOLCHAIN` | `0` | Set to `1` to eagerly install a Rust toolchain for deep evolve |
| `WIZARD_NATIVE` | `0` | Set to `1` to also install `wizard-native`, a second binary built `--features native` — the only build that can open the window with `wizard gui`. Needs no system packages. No musl or Termux asset. `WIZARD_APP` is the old name and still works. See [Native GUI](native-gui.md) |
| `WIZARD_REPO` | `teddytennant/wizard` | `owner/repo` to install from: how a published fork ships itself |
| `WIZARD_MIRROR` | *(none)* | Download mirror to try before GitHub Releases, e.g. `https://dl.example.com`. See [The download mirror](#the-download-mirror). `off`, `none`, `0` and the empty string all mean "no mirror", which is the default |
| `WIZARD_REF` | latest release tag | Git ref/tag when building from source (falls back to `main` only when the repo has no release) |
| `WIZARD_BUILD_FROM_SOURCE` | `0` | Set to `1` to build from source instead of downloading a release binary |

### The download mirror

Release binaries come from GitHub Releases. A mirror can be put in front of it,
and both `install.sh` and `wizard update` read the same setting:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
  | WIZARD_MIRROR=https://dl.example.com bash

WIZARD_MIRROR=https://dl.example.com wizard update
```

Assets are read from `<mirror>/<tag>/<asset>` — `https://dl.example.com/v2.0.0/wizard-x86_64-unknown-linux-gnu.tar.gz`.
A mirror also carries a `latest/` prefix and a bare-target alias of each
tarball, so `https://dl.example.com/latest/x86_64-unknown-linux-gnu` is a URL
that does not change between releases and is easy to script against. Wizard
itself never reads that prefix (see below).

Four things are true of the mirror, and they are the reason it is safe to point
at one:

- **GitHub Releases stays the source of truth.** Any mirror failure — DNS, a
  connection error, a 404, a release the mirror has not picked up — falls back
  to GitHub, and both the installer and `wizard update` print which host served
  the download and why they moved on.
- **The mirror is verified exactly as strictly as GitHub.** The release
  signature on `checksums.txt` and the sha256 of every tarball are checked by
  the same code on the same bytes whichever host answered. A mirror that serves
  something the release key did not sign is *refused*, and refused loudly:
  Wizard does not quietly install from GitHub instead, because that would hide
  from you that your mirror is serving forged bytes.
- **The mirror does not decide which version you get.** The release tag is
  resolved from GitHub, and the mirror is read at that tag's prefix. A mirror
  that stopped updating can only fail to answer; it cannot hold you on an old
  release. That is also why the clients never read the mutable `latest/` prefix.
- **It is off unless you turn it on.** There is no default mirror host. A
  default pointing at a host that does not answer would make every install pay a
  failed request and a fallback warning to gain nothing.

### Runtime environment variables

These override `~/.wizard/config.toml` for a single run:

| Variable | Description |
|----------|-------------|
| `WIZARD_MODEL` | Override the model tag |
| `WIZARD_LLAMACPP_HOST` | Override the llama-server URL (default `http://127.0.0.1:11435`) |
| `WIZARD_GGUF_PATH` | Override the GGUF file Wizard uses when it starts `llama-server` |
| `WIZARD_OLLAMA_HOST` | Override `ollama_host` for explicitly configured Ollama providers (does not change the synthesized local default, which stays llama.cpp) |
| `WIZARD_SKIN` | Which coding agent's terminal chrome the TUI wears: `wizard` (default), `codex`, `grok`. Outranked by `[ui] skin` in `config.toml`, which is what `/ui` writes ([The interface](usage.md#the-interface)) |
| `WIZARD_COLOR` | Force a color depth instead of detecting one: `mono` (also `0`/`off`), `16` (also `1`/`on`), `256`, `truecolor`, or `auto`. A recognised value outranks `TERM`/`COLORTERM` sniffing, but **not** `NO_COLOR`: a non-empty `NO_COLOR` is mono whatever this says, so a `NO_COLOR` shell needs `NO_COLOR= wizard` to get color back ([Color depth](usage.md#color-depth)) |
| `WIZARD_LOG` | What gets written to `~/.wizard/logs/`, in `RUST_LOG` directive syntax (`RUST_LOG` itself is not read). Default `off,wizard=warn` ([Logs](logging.md)) |
| `WIZARD_TRUST_PROJECT` | `1` trusts an undecided project's `.wizard/hooks.toml` for this process only, for unattended runs ([Hooks](hooks.md#trusting-a-project-without-the-prompt)) |

## First run

```bash
wizard
```

With no config present (the default and minimal installs), the first launch opens onboarding: a Ratatui wizard that asks which provider to use (provider, model, messaging gateway, mode) and writes `~/.wizard/config.toml`. xAI (Grok) is listed first: account sign-in, then API key. Picking Local is one step: Wizard detects your hardware, downloads a GGUF sized to it, and installs and starts `llama-server` itself (or reuses an existing Ollama install). The other options take an API key: OpenRouter, Cloudflare Workers AI (GLM 5.2), OpenAI, Anthropic, a **More cloud providers** list (Google Gemini, DeepSeek, Groq, Mistral, Moonshot, Z.AI, MiniMax, Together, Fireworks, Cerebras), or any OpenAI-compatible endpoint. Alongside them sit two BYOM picks, llama.cpp (your own GGUF and server URL) and Ollama (any model tag, installed models are listed, and a missing tag is pulled automatically on first run), for bringing your own model. Re-run it any time with `wizard --onboard`.

With a config present (after onboarding, or a `WIZARD_LOCAL=1` install), launching Wizard with a local llama.cpp provider:

- Probes `llama-server`'s health endpoint (`GET http://127.0.0.1:11435/health`)
- If nothing answers, starts `llama-server` itself with your GGUF and waits (up to 60 s) for the model to load
- Opens the Ratatui interface in genie mode

The server Wizard starts is detached: it keeps serving after Wizard exits, so the next launch connects instantly. Its output goes to `~/.wizard/llama-server.log`, and its PID is recorded in `~/.wizard/llama-server.pid` so `/server stop` never kills anything else.

Auto-start requires two things: the provider's `base_url` points at this machine, and the provider carries a non-empty `gguf_path`. Otherwise Wizard prints exactly what to run by hand (`llama-server -m <model.gguf> --port 11435`). It is more forgiving than that sounds about the pieces themselves: `llama-server` is looked for on `PATH` and in `~/.wizard/bin` and `~/.wizard/llama.cpp`, and Wizard installs llama.cpp itself if it finds none; a `gguf_path` naming a file that is not there is downloaded when its filename is one of the tiers above, and is an error only when the name is one Wizard does not recognise. The remaining failure is the port already being in use by something that is not a `llama-server` Wizard started.

Manage the server from the TUI:

```
/server status   # ready / loading its model / not running
/server start    # start llama-server for the active provider
/server stop     # stop the server Wizard started (refuses anything else)
```

Type a task in natural language:

```
> Add error handling to the fetch_user function in src/api.rs
```

Wizard reads files, applies changes, runs tests, and shows git diffs.

**Enter** sends the message; **Shift+Enter** inserts a newline for multi-line prompts (the composer grows to fit, then scrolls). Shift+Enter needs a terminal that supports the keyboard-enhancement protocol. Wizard enables it on launch when available; where it isn't, **Alt+Enter** does the same thing. Pressing Enter while a turn is already running queues the message, it lands in the transcript and runs automatically when the current turn finishes (see [Queued user messages](usage.md#queued-user-messages)).

## Configuration

`~/.wizard/config.toml` as written by onboarding's Local pick (or a `WIZARD_LOCAL=1` install):

```toml
active_provider = "local"
mode = "genie"
# 0 = no step limit: a turn runs until the model stops calling tools.
max_steps = 0

[[providers]]
name = "local"
kind = "llamacpp"
base_url = "http://127.0.0.1:11435"
model = "Qwen3.6-27B-Q4_K_M"
gguf_path = "/home/you/.wizard/models/Qwen3.6-27B-Q4_K_M.gguf"
```

`gguf_path` is what lets Wizard start `llama-server` for you; without it (e.g. a server you run yourself, or on another machine) Wizard just connects to `base_url`. `gguf_path` only applies to `kind = "llamacpp"` providers, which never use an API key.

`max_steps` bounds one turn (a step is one model → tool → model round trip). `0`, the default, means no limit: the turn ends when the model stops calling tools. An interrupt (Esc), the `--max-hours` limit, and the circuit breaker still end a turn. Set a positive number to cap it instead; the turn then stops when the budget runs out and Wizard says so.

The installer also lays down `~/.wizard/mcp.toml` (Playwright browser MCP) and `~/.wizard/subagents/` (a four-subagent roster), each file only if absent; see [the default loadout](loadout.md). To move this state (config, skills, commands, subagents, scripted tools) to another machine, see [Sync](sync.md).

### Spinner verbs (`[ui]`)

While Wizard works, the chat-area spinner shows a wizard-flavored verb ("Conjuring…", "Scrying…", "Brewing…"): one is picked pseudo-randomly per busy period and held until the turn finishes, and the next turn draws a new one. Customize the list with an optional `[ui]` section:

```toml
[ui]
spinner_verbs = ["Pondering", "Musing", "Noodling"]
```

A non-empty list fully replaces the defaults; omitting the section or setting `spinner_verbs = []` keeps the built-in wizard verbs. The status bar (`step x · Ns`, or `step x/y` under a capped `max_steps`) and tool spinners are unaffected.

### Vim mode (`[ui]`)

Modal (vim-style) editing for the input line, like Claude Code's. Toggle it live with `/vim` (or `/settings → Vim mode`), or set it as the default:

```toml
[ui]
vim = true
```

The composer starts in **INSERT** (ordinary typing); **Esc** drops to **NORMAL**, where keys are motions and operators instead of text. The status bar shows the active mode and a block cursor marks NORMAL. Single-line vim:

- **Motions:** `h`/`l` left/right, `0`/`^`/`$` line ends, `w`/`b`/`e` by word, `j`/`k` recall input history. A count prefix repeats them (`3w`, `2x`).
- **Insert:** `i`/`a` before/after the cursor, `I`/`A` line start/end, `o`/`O` end/start (single-line analogs).
- **Edits:** `x`/`X` delete a char, `r` replace one, `d`/`c`/`y` operators with a motion (`dw`, `c$`, `ye`) or doubled for the whole line (`dd`/`cc`/`yy`), `D`/`C`/`s`/`S`, `p`/`P` paste, `u` undo.

The Ctrl readline chords (`Ctrl-A/E/U/W/K`, history, etc.) keep working in both modes, and **Enter** submits from either.

## Migrating from Ollama

The local default is llama.cpp; Ollama stays fully supported but is opt-in:

- Explicit `[[providers]]` entries with `kind = "ollama"` behave exactly as before.
- A legacy config that only sets top-level `model` / `ollama_host` now resolves to llama.cpp at `http://127.0.0.1:11435`; add an explicit Ollama provider (`/provider add local ollama http://127.0.0.1:11434 <model>`) to stay on Ollama.
- If the local backend isn't installed or can't start, Wizard falls back to bring-your-own-provider: any configured cloud provider, then `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `XAI_API_KEY` / `OPENROUTER_API_KEY` from the environment, then interactive setup.

To switch an existing install to llama.cpp, add a provider from the TUI and point it at a GGUF:

```
/provider add local-llamacpp llamacpp http://127.0.0.1:11435 Qwen3.6-27B-Q4_K_M
/provider use local-llamacpp
```

Then set `gguf_path` on that provider in `~/.wizard/config.toml` so Wizard can start the server for you. Or re-run onboarding: `wizard --onboard`.

## Using Ollama instead

Install with the Ollama flavor (installs Ollama, starts it, pulls the auto-tiered model):

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_USE_OLLAMA=1 bash
```

Or pick the local Ollama option in onboarding (`wizard --onboard`). Wizard speaks Ollama's native `/api/chat` for these providers, as before.

## Using a cloud or remote provider

Any OpenAI-compatible endpoint, OpenRouter, Cloudflare Workers AI, Anthropic, or xAI works. The simplest path is `/provider` inside the TUI: it opens a menu of your configured providers (Enter switches to one) with an **Add provider…** entry that walks you through each type. Pick xAI (API key or account sign-in), OpenRouter, Cloudflare Workers AI, OpenAI, Anthropic, one of the OpenAI-compatible presets (Google Gemini, DeepSeek, Groq, Mistral, Moonshot, Z.AI, MiniMax, Together, Fireworks, Cerebras), or an OpenAI-compatible custom endpoint; you type the API key inline (hidden) and it is stored in `~/.wizard/credentials.toml` (file mode 0600). xAI account sign-in runs the OAuth flow and adds the provider for you.

The same thing is scriptable with explicit arguments:

```
/provider add xai xai https://api.x.ai/v1 grok-4.6 XAI_API_KEY
/provider add openai openai https://api.openai.com/v1 gpt-5.6-sol OPENAI_API_KEY
/provider add gemini openai https://generativelanguage.googleapis.com/v1beta/openai gemini-3.5-flash GEMINI_API_KEY
/provider use xai
```

With `/provider add`, the last argument names the environment variable holding your API key (export it before launching, `export OPENAI_API_KEY=sk-...`); the key itself is never written to disk. The interactive menu instead stores the key in `~/.wizard/credentials.toml`. Onboarding offers the same choices interactively. When both exist for one provider, **the environment variable wins**: exporting a key for one run or one CI job overrides whatever was stored months ago, rather than being silently ignored. The default install puts down no local stack, so picking a cloud provider on first run is all there is to it.

### Using OpenRouter

OpenRouter serves hundreds of hosted models behind one OpenAI-compatible endpoint and one API key:

```
/provider add openrouter openrouter https://openrouter.ai/api/v1 openrouter/auto OPENROUTER_API_KEY
/provider use openrouter
```

`openrouter/auto` is OpenRouter's Auto Router, which picks a model per prompt; any `vendor/model` tag from openrouter.ai/models works instead. Wizard sends OpenRouter's recommended attribution headers (`HTTP-Referer`, `X-Title`) on every request.

### Using Cloudflare Workers AI

[Cloudflare Workers AI](https://developers.cloudflare.com/workers-ai/) serves open models (GLM, Llama, Qwen, …) on serverless GPUs behind an account-scoped OpenAI-compatible endpoint. It needs two things: your **account id** (Cloudflare dashboard → Workers AI, or `wrangler whoami`) and an **API token** with the Workers AI permission. The default model is **GLM 5.2** (`@cf/zai-org/glm-5.2`).

The interactive `/provider` menu is the easiest path: pick **Cloudflare Workers AI (API token)**, paste the account id (folded into the endpoint URL) then the token (stored in `~/.wizard/credentials.toml`). Scripted, the account id goes in the base URL:

```
export CLOUDFLARE_API_TOKEN=...
/provider add cloudflare cloudflare https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1 @cf/zai-org/glm-5.2 CLOUDFLARE_API_TOKEN
/provider use cloudflare
```

Any `@cf/...` text-generation tag works in place of the model (see [the catalog](https://developers.cloudflare.com/workers-ai/models/)); `/model` lists what your account can serve. Workers AI's OpenAI-compatible surface exposes only chat completions (no `/v1/models`), so Wizard discovers models and probes health against Cloudflare's native account catalog.

### Signing in with an xAI account

You can use xAI without an API key by signing in with your xAI account (OAuth 2.0 with PKCE). Pick **Add provider… → xAI (Grok) sign-in** from the `/provider` menu, or run it directly:

```bash
wizard --login xai     # or /login xai from inside the TUI
```

Wizard opens your browser, captures the callback on localhost, and stores the tokens in `~/.wizard/xai_oauth.json` (file mode 0600); the access token is refreshed automatically. On success it adds the `xai-oauth` provider and switches the live agent to it; no `/provider add` needed. The window can start the same flow from its settings sheet (see [Native GUI](native-gui.md)).

Note: xAI gates OAuth API access to certain SuperGrok plans. If requests come back with HTTP 403, use the API-key flavor (`kind = "xai"` with `XAI_API_KEY`) instead.

### Signing in with a ChatGPT account

ChatGPT subscription access is OAuth too (OpenAI's Codex backend, not the public Chat Completions API):

```bash
wizard --login chatgpt
```

Tokens land in `~/.wizard/chatgpt_oauth.json` (mode 0600). On success Wizard adds a `chatgptoauth` provider pointed at `chatgpt.com/backend-api/codex`. The window's settings sheet accepts `chatgpt` the same way as `xai`. You still need a plan that the Codex backend accepts; a failed exchange or 403 is the usual signal that the account is not eligible.

### Signing in over SSH

Both providers redirect to a fixed loopback address — `127.0.0.1:56121/callback` for xAI, `localhost:1455/auth/callback` for ChatGPT — and loopback belongs to whichever machine opens the browser. Over SSH that is your laptop, not the box running the sign-in, so the redirect never reaches the listener. `wizard --login` notices the remote session and prints both ways through:

1. **Forward the port.** From the machine with the browser:

   ```bash
   ssh -N -L 56121:127.0.0.1:56121 you@your-server   # 1455 for ChatGPT
   ```

   Leave it running, open the printed URL locally, and the redirect tunnels back to the listener.

2. **Paste the redirect back.** Open the printed URL anyway. The final redirect lands on a page that cannot connect — copy that address out of the browser's address bar and paste it at the `or paste the redirect URL here:` prompt. The bare `code=…` value on its own works too.

The paste prompt is only offered by `wizard --login`, which owns the terminal. Inside the TUI, `/login xai` still prints the port-forwarding command, but the tunnel is the only way through from there.

## Headless mode

Run a single task without the TUI:

```bash
wizard -p "find all TODO comments and list them by file"
```

Combine with sovereign mode for autonomous execution:

```bash
wizard --mode sovereign -p "implement JWT refresh tokens"
```

## Working in a project

`cd` into your repository before launching Wizard. It uses the current working directory as the project root.

For best results, add an `AGENTS.md` (or `WIZARD.md`) at the repo root with:

- Stack and versions
- Build and test commands
- Code style rules
- Directories that must not be edited

Example:

```markdown
# Agent Instructions

## Commands
- Build: `cargo build`
- Test: `cargo test`
- Lint: `cargo clippy`

## Rules
- Prefer minimal diffs
- Run tests after every change
- Do not edit generated files
```

## Updating

`wizard update` upgrades the binary in place: it downloads the latest release from GitHub, checks the published `checksums.txt` against its minisign signature, verifies the tarball's sha256 against that file, and swaps it in with an atomic rename. The change takes effect on the next `wizard` launch. With `WIZARD_MIRROR` set it tries that mirror first and falls back to GitHub on any failure, saying which one it used ([The download mirror](#the-download-mirror)); everything below applies unchanged to whichever host answered.

When this binary cannot verify a download (it was compiled with the placeholder signing key) or no published asset runs here (NixOS without a static musl loader, Termux), it clones the tag and `cargo build --release --locked` instead. That is the same trust as `WIZARD_BUILD_FROM_SOURCE=1` on the installer. A failed signature or digest is still fatal: it does not compile something else around a check that failed. Background auto-update (`[update].auto`) stays download-only, so it never starts a multi-minute compile unattended.

**Verification is mandatory and there is no way to skip it.** `checksums.txt` and its detached signature `checksums.txt.minisig` are both fetched *before* anything is downloaded, and each of these aborts the update with the current binary untouched:

- the release publishes no `checksums.txt.minisig`, or the signature does not verify against the release public key compiled into this binary, or it was made by a different key, or its trusted comment was edited. The key is published at [`wizard-release.pub`](https://github.com/teddytennant/wizard/blob/main/wizard-release.pub) and there is no flag, config key, or environment variable that skips this check ([SECURITY.md](../SECURITY.md#release-signing));
- the release publishes no `checksums.txt`, or it cannot be fetched (a 404 is reported as "this release published no checksums.txt"; any other status as a fetch failure worth retrying);
- none of the assets for this platform is listed in it. An individual asset that is absent is skipped without being downloaded, since some platforms publish only a musl or only a gnu build, but an unlisted asset is never fetched "anyway";
- the downloaded tarball's sha256 does not match the listed digest. A mismatch aborts the whole update rather than moving on to another asset: it means corruption or tampering, not a missing platform build.

Downloads are staged in `~/.wizard/update` (mode 0700), never the shared system temp directory, because on the `sudo` path below the staged file is what gets installed. An unverified file is never written over the binary and never executed.

```bash
wizard update              # download and install the latest release
wizard update --check      # report the current and latest version; install nothing
wizard update --to v2.0.0  # install a specific tag instead of the latest
wizard update --rollback   # restore the previous binary from <name>.bak (see below)
```

If the binary lives in a root-owned directory (e.g. `/usr/local/bin`), `wizard update` escalates with `sudo` when run in a terminal — twice: once to copy the current binary to `<name>.bak`, once to install the new one, so `sudo` may prompt for a password twice. In a non-interactive context it escalates neither and prints the exact `sudo install` command instead.

**`--rollback` has something to restore on both install paths.** Either way the live binary is copied to `<name>.bak` before the new one lands, and `wizard update --rollback` puts it back. When Wizard can write the install directory itself (`~/.local/bin`, the NixOS and Termux defaults, or a `WIZARD_INSTALL_DIR` you own) the backup and the restore are ordinary file operations; on a root-owned directory both go through `sudo install`, which is the second escalation above. `--rollback` fails only when there is no `<name>.bak` yet — nothing has been installed over on this machine — and it says so, naming the path it looked at. Deep evolve is stricter: it never escalates, so on an install path it cannot write it fails at the install step instead of swapping without a backup, and its own `<name>.prev` exists whenever the swap actually happened.

Wizard also checks for a newer release at startup (once every 24 hours, cached in `~/.wizard/update-check.json`). By default it just prints a one-line notice; nothing is downloaded until you run `wizard update`. Configure it with an `[update]` block in `~/.wizard/config.toml`:

```toml
[update]
notify = true                 # print a one-liner when a newer release exists (default)
auto = false                  # download + install newer releases in the background at startup
repo = "teddytennant/wizard"  # GitHub owner/repo to check (point a fork elsewhere)
interval_hours = 24           # hours between startup checks
```

With `auto = true` the new binary is fetched in the background and takes effect on the next launch (the running process is never hot-swapped); it is skipped when the install directory needs `sudo`, falling back to the notice. It goes through the same mandatory signature and checksum verification as `wizard update`, and a failure there is silent: the notice is all you get, and the binary is left alone.

Re-running the installer still works and leaves an existing `~/.wizard/config.toml` untouched:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
  | WIZARD_BUILD_FROM_SOURCE=1 bash
```

To change models, download a GGUF into `~/.wizard/models/` (Hugging Face hosts Q4_K_M quants of most open models), update `model` and `gguf_path` in `~/.wizard/config.toml`, then `/server stop` and `/server start` (or restart Wizard).

To install a specific release via the installer instead, or to roll back after an update, pin the tag:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
  | WIZARD_VERSION=v2.0.0 WIZARD_BUILD_FROM_SOURCE=1 bash
```

(`WIZARD_BUILD_FROM_SOURCE=1` builds that tag from source instead of
downloading its assets. `WIZARD_VERSION` names the ref the source build clones as well as the release
tag it would have downloaded, so it pins both flavors; `WIZARD_REF` overrides it
for the source build alone.)

## Uninstall

Everything Wizard installs lives in two places: the binary and its `~/.wizard/` state directory.

```bash
# stop a running llama-server first, if Wizard started one
kill "$(cat ~/.wizard/llama-server.pid)" 2>/dev/null

# the binary, its three rollback copies (.bak from `wizard update`, .prev from
# deep evolve, .undone from `wizard evolve undo <N>`), and the llama-server
# symlink next to it
sudo rm -f /usr/local/bin/wizard /usr/local/bin/wizard.bak /usr/local/bin/wizard.prev /usr/local/bin/wizard.undone /usr/local/bin/llama-server
# or, if it was installed to ~/.local/bin (NixOS, or no sudo at install time):
rm -f ~/.local/bin/wizard ~/.local/bin/wizard.bak ~/.local/bin/wizard.prev ~/.local/bin/wizard.undone ~/.local/bin/llama-server

# the managed runtime and models (large): llama.cpp tree, GGUFs, symlinks
rm -rf ~/.wizard/bin ~/.wizard/models ~/.wizard/llama.cpp
```

Removing the rest of `~/.wizard/` (config, credentials, sessions, loadout, evolution log) is optional: delete the whole directory with `rm -rf ~/.wizard` for a clean slate. If the installer set up Ollama (`WIZARD_USE_OLLAMA=1` / `WIZARD_BYOM=1`), that is a separate program; uninstall it per Ollama's own docs.

## Troubleshooting

### llama-server won't start

Check the log first:

```bash
tail -50 ~/.wizard/llama-server.log
```

Common causes: the GGUF at `gguf_path` is missing or truncated (re-run the installer with `WIZARD_LOCAL=1`; the download resumes), or the model doesn't fit in memory (see below).

### llama-server not found

Wizard looks for `llama-server` on `PATH`. The local setup (onboarding's Local pick, or a `WIZARD_LOCAL=1` install) links it into the install dir and `~/.wizard/bin/`; if neither is on your `PATH`, add one, or install llama.cpp yourself:

```bash
brew install llama.cpp                  # Homebrew / Linuxbrew
nix profile install nixpkgs#llama-cpp   # Nix / NixOS
```

### Server status says "loading its model"

GGUF loads take a while (tens of seconds for the larger tiers). `/server status` shows `ready` when it's done; Wizard waits up to 60 s automatically on startup.

### Out of memory

Wizard catches most of these before they happen: the preflight fit check refuses to spawn `llama-server` when the model is larger than usable RAM minus 2 GB of headroom, and when `llama-server` dies anyway with an allocator failure or an OOM kill in its log, the error quotes the log tail and appends a hint. Neither ever tells you to "pick something smaller" and leaves you to find it. The preflight names the largest tier that fits, or says local inference will not work on this machine when none does:

```
the model /home/you/.wizard/models/Qwen3.6-27B-Q4_K_M.gguf is ~16 GB but this machine has
only 7 GB of usable RAM, so llama-server would be killed loading it; run `wizard --onboard`
and pick Qwen3.5 4B (~3 GB, the largest tier that fits), or point `gguf_path` in
~/.wizard/config.toml at a model that does
```

The crash hint that follows an OOM kill is the more careful of the two, because by then a model the fit table approved has already died, so the table has been proved wrong for that model on that machine. It answers in one of four ways, depending on what is knowable:

- **A smaller tier fits.** It names that tier, never the one that just died: an 18 GB machine that OOMs on the 27B is sent to Qwen3.5 9B, not back to the 27B the fit table originally picked.
- **Nothing smaller exists, but tiers do fit this machine.** This is a 64 GB workstation whose own ~2 GB fine-tune was killed. It says there is no local tier smaller than the model that failed, and the way out is memory rather than a smaller download: free some (llama-server needs room for its KV cache and compute buffers on top of the weights) or move to a cloud provider. No tier is named, because every tier here is *larger* than what already failed.
- **Nothing in the tier table fits at all.** It says so plainly and points at the cloud providers.
- **Usable RAM cannot be read at all** (no `/proc/meminfo` and no working `sysctl hw.memsize`, which in practice means a stripped container mount namespace). Which tiers fit is then unknowable, so the preflight has nothing to compare against and does not fire, and the hint degrades to the generic `run 'wizard --onboard' to pick a smaller model` with no tier named. Nothing is wrong with your install; there is simply no reading to reason from.

`wizard --onboard` is the shortest path: it re-detects the memory budget, preselects the tier that matches it, and downloads that GGUF. To do it by hand, download the smaller GGUF from Hugging Face into `~/.wizard/models/` and update the provider entry in `~/.wizard/config.toml`:

```toml
[[providers]]
name = "local"
kind = "llamacpp"
base_url = "http://127.0.0.1:11435"
model = "Qwen3.5-4B-Q4_K_M"
gguf_path = "/home/you/.wizard/models/Qwen3.5-4B-Q4_K_M.gguf"
```

The 4B is the smallest tier Wizard knows how to download (~3 GB of weights, about 5 GB of RAM once the runtime's buffers are counted). If even that does not fit, local inference will not work on this machine: run `wizard --onboard` and pick a cloud provider, which needs no local RAM.

### Ollama not running (Ollama providers)

```bash
ollama serve
# or on systemd:
sudo systemctl start ollama
```

### Check Wizard logs

Wizard writes diagnostics to `~/.wizard/logs/<timestamp>-<pid>.jsonl`, never to the terminal. By default only Wizard's own warnings and errors are recorded; `WIZARD_LOG` widens that, in `RUST_LOG` directive syntax (`RUST_LOG` itself is not read):

```bash
WIZARD_LOG=wizard=debug wizard
tail -f ~/.wizard/logs/*.jsonl        # in another terminal: the log never prints to yours
```

Panics are always appended, with a backtrace, whatever the filter says. See [Logs](logging.md).

### Collect a bug report

```bash
wizard doctor --bundle
```

Writes the checks, your config (allowlisted), the newest session transcript, and the newest logs into `~/.wizard/bundles/doctor-<timestamp>/`, with credentials stripped. Read it before you attach it: the transcript is your own text. See [Doctor & status](doctor.md#bug-report-bundles).

## Next steps

- [The default loadout](loadout.md): the preconfigured browser MCP and subagent roster
- [Personality modes](modes.md): genie vs sovereign
- [Self-extension](evolve.md): how `/evolve` adds capabilities
- [Bring your own model](byom.md): any GGUF, or custom Ollama models
- [Sync](sync.md): move your config and skills to another machine as a signed bundle
- [Architecture](architecture.md): how Wizard works under the hood
