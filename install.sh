#!/usr/bin/env bash
#
# Wizard installer — one script, four flavors.
#
#   curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
#
# Default (no flags):
#   1. Detect OS and CPU architecture
#   2. Download the `wizard` binary from GitHub releases
#   3. Lay down the default loadout: ~/.wizard/mcp.toml (Playwright browser MCP)
#      and ~/.wizard/subagents/*.toml (reviewer, researcher, tester, documenter)
#      — each file only if absent, never overwriting
#
# No model, no model runtime, no config.toml: the first `wizard` run opens
# onboarding, which asks which provider to use. Picking "Local" there is one
# step — wizard detects your hardware, downloads a fitting GGUF, and installs
# and manages llama-server itself (or reuses an existing Ollama install).
#
# Flavors (mutually exclusive):
#   WIZARD_LOCAL=1    preinstall the local stack non-interactively (headless
#                     boxes, provisioning scripts):
#                       1. Install llama.cpp's `llama-server` if absent, using
#                          the GPU: on NVIDIA it compiles a CUDA build (when
#                          nvcc is present); on other GPUs it installs a Vulkan
#                          loader and uses the prebuilt Vulkan build; CPU build
#                          otherwise. Upgrades an earlier CPU-only build in place
#                       2. Select a model tier based on available VRAM (or
#                          system RAM on CPU-only)
#                       3. Download the matching Qwen3 GGUF (Q4_K_M) from
#                          Hugging Face
#                       4. Write ~/.wizard/config.toml (never clobbers an
#                          existing one)
#                     No server is started here: wizard launches llama-server
#                     itself on first run. WIZARD_USE_OLLAMA=1 is the
#                     Ollama-based variant (install Ollama, start it, pull the
#                     auto-tiered model) and implies this flavor — no need to
#                     also set WIZARD_LOCAL.
#   WIZARD_BYOM=1     bring your own model — install Ollama if absent, start
#                     it, and install the binary. Model choice happens in
#                     wizard's onboarding on the first run, which pulls the
#                     tag you pick. Set WIZARD_MODEL=<tag> for a headless
#                     install: the tag is pulled and the config written here,
#                     no prompts. You choose the model: Wizard does not ship,
#                     endorse, or maintain third-party model weights; you are
#                     responsible for their licenses.
#   WIZARD_MINIMAL=1  binary only — like the default but also skips the
#                     loadout; the first `wizard` run starts onboarding
#
# Environment variables:
#   WIZARD_INSTALL_DIR           where to place the binary    (default /usr/local/bin;
#                                ~/.local/bin on NixOS; $PREFIX/bin on Termux)
#   WIZARD_LOCAL                 1 = preinstall the llama.cpp stack and a model
#                                    (see above)               (default 0)
#   WIZARD_MINIMAL               1 = minimal install (see above)        (default 0)
#   WIZARD_BYOM                  1 = bring-your-own-model install (see above)
#                                    (default 0)
#   WIZARD_BESPOKE               deprecated alias for WIZARD_MINIMAL
#   WIZARD_MODEL                 local flavors: force a specific model tier
#                                (default auto-detected; with WIZARD_BYOM=1:
#                                pull this tag and write the config instead of
#                                deferring to onboarding)
#   WIZARD_SKIP_MODEL_PULL       1 = local flavors: skip the model download (default 0)
#   WIZARD_SKIP_LLAMACPP_INSTALL 1 = WIZARD_LOCAL: llama-server managed elsewhere (default 0)
#   WIZARD_LLAMACPP_NO_CUDA      1 = never compile a CUDA llama-server; use the
#                                    prebuilt Vulkan/CPU build instead (default 0)
#   WIZARD_USE_OLLAMA            1 = local flavor on Ollama instead of llama.cpp
#                                    (implies WIZARD_LOCAL)    (default 0)
#   WIZARD_SKIP_OLLAMA_INSTALL   1 = Ollama managed elsewhere (default 0)
#   WIZARD_PROFILE               which plugins to build in: minimal, pi, server,
#                                default or full. Unset (the default) installs
#                                the published binary, which is the `default`
#                                profile. Anything else is a different cargo
#                                feature set, so it is built from source here.
#                                  minimal  one API key and git — CI containers,
#                                           second machines
#                                  pi       a local model, no cloud provider,
#                                           no JS backend — Raspberry Pi, small ARM
#                                  server   the stock build without the P2P mesh
#                                           — headless boxes
#                                  default  every backend and every tool, no
#                                           window (what a release binary is)
#                                  full     default plus the GUI, in one binary
#                                `wizard plugin profiles` prints this list off
#                                an installed binary. See docs/plugins.md.
#                                NOTE: unrelated to WIZARD_MINIMAL, which is
#                                about what the installer sets up, not about
#                                which plugins the binary has.
#   WIZARD_WITH_TOOLCHAIN        1 = eagerly install a Rust toolchain for deep evolve (default 0)
#   WIZARD_NATIVE                1 = also install the native GUI: a second binary,
#                                    `wizard-native` (built --features native),
#                                    which is the only build that can open the
#                                    window with `wizard gui`. Needs no
#                                    system packages on either OS. `wizard` itself
#                                    is untouched. Unsupported on Termux, and
#                                    there is no static musl build of it.
#                                    (WIZARD_APP is the old name, still honored.)
#                                    (default 0)
#   WIZARD_VERSION               release tag to install, e.g. v0.4.0 (default: the
#                                latest release). Pins the download to
#                                releases/download/<tag>/ — use it for
#                                reproducible installs or to roll back
#   WIZARD_REPO                  owner/repo to install from   (default teddytennant/wizard)
#   WIZARD_MIRROR                download mirror to try before GitHub Releases,
#                                e.g. https://dl.example.com. Empty (the
#                                default) means no mirror: GitHub is used
#                                directly. "off", "none" or "0" also disable it.
#                                Assets are read from <mirror>/<tag>/<asset>;
#                                the tag always comes from GitHub, so a stale
#                                mirror cannot pin you to an old release. Any
#                                mirror failure falls back to GitHub, and the
#                                script says which one served the download.
#                                A mirror-served file is verified exactly like a
#                                GitHub-served one — same signature, same digest,
#                                same refusals                          (default "")
#   WIZARD_REF                   git ref/tag when building from source
#                                (default: latest release tag, falling back to
#                                main only when the repo has no release)
#   WIZARD_BUILD_FROM_SOURCE     1 = build from source instead of downloading a
#                                    release (default 0; forced on Termux)


set -euo pipefail

# --- NixOS / Termux detection -------------------------------------------
# Defined early so the install-dir default below can branch on them. NixOS is
# not an FHS distro: prebuilt glibc binaries can't find /lib64/ld-linux and
# /usr/local/bin isn't on PATH, so the installer selects the static musl
# asset and installs to ~/.local/bin instead. Termux is Android/Bionic: no
# prebuilt gnu/musl asset runs, there is no sudo, and the only writable
# install location on PATH is $PREFIX/bin — so the installer forces a source
# build and lands the binary there.
is_nixos() {
    [ -f /etc/NIXOS ] && return 0
    [ -r /etc/os-release ] && grep -qiE '^ID=nixos' /etc/os-release
}

is_termux() {
    # TERMUX_VERSION is set by the app; PREFIX points at the Termux usr tree;
    # the filesystem path is the last-resort probe for non-interactive shells.
    [ -n "${TERMUX_VERSION:-}" ] && return 0
    [ -n "${TERMUX_APP_PID:-}" ] && return 0
    case "${PREFIX:-}" in
        *com.termux*) return 0 ;;
    esac
    [ -d /data/data/com.termux/files/usr ]
}

# --- defaults -----------------------------------------------------------

# /usr/local/bin is the right default on FHS distros, but not on NixOS (not on
# PATH, wrong place for an FHS binary) or Termux (no sudo, $PREFIX/bin is the
# real bin dir). An explicit WIZARD_INSTALL_DIR override always wins.
if [ -z "${WIZARD_INSTALL_DIR:-}" ]; then
    if is_termux; then
        WIZARD_INSTALL_DIR="${PREFIX:-/data/data/com.termux/files/usr}/bin"
    elif is_nixos; then
        WIZARD_INSTALL_DIR="$HOME/.local/bin"
    else
        WIZARD_INSTALL_DIR="/usr/local/bin"
    fi
fi
WIZARD_LOCAL="${WIZARD_LOCAL:-0}"
WIZARD_MINIMAL="${WIZARD_MINIMAL:-0}"
WIZARD_BYOM="${WIZARD_BYOM:-0}"
WIZARD_MODEL="${WIZARD_MODEL:-}"
WIZARD_SKIP_MODEL_PULL="${WIZARD_SKIP_MODEL_PULL:-0}"
WIZARD_SKIP_LLAMACPP_INSTALL="${WIZARD_SKIP_LLAMACPP_INSTALL:-0}"
WIZARD_LLAMACPP_NO_CUDA="${WIZARD_LLAMACPP_NO_CUDA:-0}"
WIZARD_USE_OLLAMA="${WIZARD_USE_OLLAMA:-0}"
WIZARD_SKIP_OLLAMA_INSTALL="${WIZARD_SKIP_OLLAMA_INSTALL:-0}"
WIZARD_WITH_TOOLCHAIN="${WIZARD_WITH_TOOLCHAIN:-0}"
WIZARD_NATIVE="${WIZARD_NATIVE:-0}"
WIZARD_VERSION="${WIZARD_VERSION:-}"
WIZARD_REPO="${WIZARD_REPO:-teddytennant/wizard}"
WIZARD_REF="${WIZARD_REF:-}"
# Off unless you set it. The mirror is a bandwidth optimisation in front of
# GitHub Releases, and a default pointing at a host that does not answer would
# make every install pay a failed request and a fallback warning to gain
# nothing. Ship a default here only once the host is real; until then the
# people who have one set WIZARD_MIRROR.
WIZARD_MIRROR="${WIZARD_MIRROR:-}"
WIZARD_BUILD_FROM_SOURCE="${WIZARD_BUILD_FROM_SOURCE:-0}"

# WIZARD_APP is the old name for the graphical install. It installed
# `wizard-desktop`, a webview window over the loopback GUI server, which this
# release deleted; WIZARD_NATIVE installs the iced window that replaced it.
# Honored as a deprecated alias so an existing provisioning script still gets a
# window rather than silently getting nothing.
if [ "${WIZARD_APP:-0}" = "1" ]; then WIZARD_NATIVE=1; fi

# Termux cannot run the published gnu/musl release binaries (Android/Bionic).
# Force a source build unless the user already asked for one; never try the
# native GUI (no display server, and no prebuilt asset for Bionic).
if is_termux; then
    if [ "$WIZARD_BUILD_FROM_SOURCE" != "1" ]; then
        WIZARD_BUILD_FROM_SOURCE=1
    fi
    if [ "$WIZARD_NATIVE" = "1" ]; then
        WIZARD_NATIVE=0
    fi
fi

# WIZARD_BESPOKE is the old name for the minimal install; honored as a deprecated alias.
if [ "${WIZARD_BESPOKE:-0}" = "1" ]; then WIZARD_MINIMAL=1; fi

REPO="${WIZARD_REPO}"
# WIZARD_VERSION pins the release; otherwise follow the latest. Accept the tag
# with or without the leading v (releases are tagged v<X.Y.Z>).
if [ -n "$WIZARD_VERSION" ]; then
    case "$WIZARD_VERSION" in
        v*) ;;
        *) WIZARD_VERSION="v${WIZARD_VERSION}" ;;
    esac
    RELEASE_BASE="https://github.com/${WIZARD_REPO}/releases/download/${WIZARD_VERSION}"
else
    RELEASE_BASE="https://github.com/${WIZARD_REPO}/releases/latest/download"
fi
# The minisign public key wizard releases are signed with. It is the exact key
# line published at the repository root as wizard-release.pub, and the exact key
# compiled into the binary (src/update.rs), so the installer and `wizard update`
# trust one key and a test asserts the two copies have not drifted. Editing this
# line is editing what this script will install.
WIZARD_RELEASE_PUBKEY="RWQVojXCTN+B/wZ//qcSQpiznLrxQd6DKKnhovJjGLrjWk2Gfftu3Gbi"
# Set once `checksums.txt` in $TMP_DIR has been fetched *and* its signature has
# verified, so a second asset does not re-verify the same file.
CHECKSUMS_VERIFIED=0

# The release tag this run is installing, resolved once (see
# resolve_release_tag) and empty when it could not be determined at all. Two
# things read it: the mirror, which is addressed per release, and the signature
# check, which requires the signed trusted comment to name this tag.
RESOLVED_TAG=""
TAG_RESOLVED=0

# Download-mirror state, all resolved once on the first asset fetch.
# MIRROR_BASE is the full per-release base URL (<mirror>/<tag>) or empty when
# there is no mirror to try; MIRROR_RESOLVED guards the one-time resolution;
# MIRROR_WARNED keeps the fallback notice to one line per run; DOWNLOAD_SOURCE
# names whoever served the last asset, so the install can say which one it used.
MIRROR_BASE=""
MIRROR_RESOLVED=0
MIRROR_WARNED=0
DOWNLOAD_SOURCE=""

LLAMACPP_REPO="ggml-org/llama.cpp"
LLAMACPP_URL="http://127.0.0.1:11435"
LLAMA_BIN_DIR="$HOME/.wizard/bin"
MODELS_DIR="$HOME/.wizard/models"
OLLAMA_URL="http://127.0.0.1:11434"

OS=""
ARCH=""
MODEL=""
GGUF_FILE=""
GGUF_URL=""
GGUF_PATH=""
MEM_GB=0
MEM_SOURCE=""
BINARY_INSTALLED=0
INSTALLED_PATH=""
PLACED_PATH=""
NATIVE_INSTALLED=0
NATIVE_PATH=""
NATIVE_BIN=""

TMP_DIR="$(mktemp -d)"
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT

# --- output helpers -----------------------------------------------------

say()  { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- build profiles (WIZARD_PROFILE) ------------------------------------
#
# A profile is a named plugin set: five answers to "what kind of machine is
# this", each one a cargo feature list. Wizard ships eighteen plugin features
# and every one of them can be left out; asking somebody to decide eighteen
# times while a curl pipe is running is how a feature list stays decorative.
#
# The same table is in src/plugins/profile.rs, which is where the rationale for
# each name lives. It has to be in two places: this script is fetched and piped
# to bash by people who have no checkout, so it cannot read a file from the
# repository, and that module cannot be consulted before the binary it is
# compiled into exists. The Rust test `the_installer_agrees_about_every_profile`
# sources this script and diffs the answers, so the two copies cannot drift
# without a red test.
#
# Nothing below runs unless WIZARD_PROFILE is set. A stock install is untouched.

# Cargo's `default` feature list, alphabetically. Restated rather than read out
# of Cargo.toml for the reason above: at this point there is no checkout to read
# it from, and the profile has to be resolved before the clone.
WIZARD_DEFAULT_FEATURES="acp,fleet,gateway,graph,mcp,mesh,plugin-js,provider-anthropic,provider-chatgpt,provider-cloudflare,provider-llamacpp,provider-ollama,provider-openai,provider-xai,tool-git,tool-json,tool-publish,tool-web"

# The default list with the named features removed, comma-joined.
#
# A filter over the whole list rather than a substring delete, because a feature
# that another kept feature enables is not removed by dropping it from the list:
# `graph = ["mesh"]`, so `server` has to drop both or cargo turns the mesh back
# on. Written with `if` rather than `[ ... ] && x=1` because callers run under
# `set -e`, where a test that is simply false is a failed command.
profile_default_minus() {
    local out="" f d skip
    for f in $(printf '%s' "$WIZARD_DEFAULT_FEATURES" | tr ',' ' '); do
        skip=0
        for d in "$@"; do
            if [ "$f" = "$d" ]; then skip=1; fi
        done
        if [ "$skip" = "0" ]; then
            if [ -z "$out" ]; then out="$f"; else out="${out},${f}"; fi
        fi
    done
    printf '%s' "$out"
}

# The features one profile resolves to, comma-joined. Non-zero for a name that
# is not a profile, which is how the caller tells a typo from a valid set.
profile_features() {
    case "${1:-}" in
        minimal) printf 'provider-anthropic,provider-openai,tool-git' ;;
        pi)      printf 'provider-llamacpp,provider-ollama,tool-git' ;;
        server)  profile_default_minus graph mesh ;;
        default) printf '%s' "$WIZARD_DEFAULT_FEATURES" ;;
        full)    printf '%s,native' "$WIZARD_DEFAULT_FEATURES" ;;
        *)       return 1 ;;
    esac
}

# The cargo flags that build one profile.
#
# `default` prints nothing, and the emptiness is the whole opt-in promise: a
# stock run has to invoke exactly the command it invoked before profiles
# existed, which it does by being handed no flags rather than by being handed
# the default list spelled out. `--no-default-features --features <every
# default>` is one feature resolution away from the stock build and the
# difference would be invisible until it bit.
profile_cargo_flags() {
    case "${1:-}" in
        default) printf '' ;;
        full)    printf -- '--features native' ;;
        minimal|pi|server)
            printf -- '--no-default-features --features %s' "$(profile_features "$1")" ;;
        *) return 1 ;;
    esac
}

WIZARD_PROFILE="${WIZARD_PROFILE:-}"
# Spliced into `cargo build --release` unquoted, so an empty value adds no
# argument at all. Set only by the block below.
CARGO_FEATURE_FLAGS=""
if [ -n "$WIZARD_PROFILE" ]; then
    profile_features "$WIZARD_PROFILE" >/dev/null 2>&1 \
        || die "WIZARD_PROFILE='${WIZARD_PROFILE}' is not a profile — pick one of: minimal, pi, server, default, full (see docs/plugins.md)"
    CARGO_FEATURE_FLAGS="$(profile_cargo_flags "$WIZARD_PROFILE")"
    # A profile is a *build*, and the published release assets are all the
    # default one, so anything else has to be compiled here. `default` is the
    # exception and stays on the download path, which is what makes
    # WIZARD_PROFILE=default a no-op rather than a slow no-op.
    if [ "$WIZARD_PROFILE" != "default" ]; then
        WIZARD_BUILD_FROM_SOURCE=1
    fi
    # Not a conflict, but it is almost always a mistake: `full` puts the window
    # inside the one `wizard` binary, and WIZARD_NATIVE installs a second binary
    # called `wizard-native` that also has it. Doing both compiles iced twice.
    if [ "$WIZARD_PROFILE" = "full" ] && [ "$WIZARD_NATIVE" = "1" ]; then
        warn "WIZARD_PROFILE=full already builds the window into 'wizard'; WIZARD_NATIVE=1 will build it a second time as 'wizard-native'"
    fi
fi

# --- input validation ---------------------------------------------------

# WIZARD_MODEL becomes a model tag on an `ollama pull` command line, a lookup
# key for a GGUF download, and a TOML string in ~/.wizard/config.toml. Refuse
# anything that is not tag-shaped, here, before a single byte is installed —
# a value that only blows up in write_config blows up *after* the binary is in
# place, which is the worst moment to abort. Real tags look like `qwen3.5:9b`
# or `vendor/model-name@v2`; a quote, a backslash or a shell metacharacter
# does not belong in one.
if [ -n "${WIZARD_MODEL:-}" ]; then
    case "$WIZARD_MODEL" in
        *[!A-Za-z0-9._:/@+-]*)
            die "WIZARD_MODEL='${WIZARD_MODEL}' contains characters a model tag cannot have (allowed: letters, digits, . _ : / @ + -)"
            ;;
    esac
fi

# --- platform detection -------------------------------------------------

detect_platform() {
    local os arch
    os="$(uname -s)"
    case "$os" in
        Linux)  OS="linux" ;;
        Darwin) OS="macos" ;;
        *)
            die "unsupported operating system: $os (Wizard supports Linux and macOS)"
            ;;
    esac

    arch="$(uname -m)"
    case "$arch" in
        x86_64 | amd64)  ARCH="x86_64" ;;
        aarch64 | arm64) ARCH="aarch64" ;;
        *)
            die "unsupported CPU architecture: $arch (need x86_64 or aarch64)"
            ;;
    esac

    say "Platform: ${OS}/${ARCH}"
}

require_curl() {
    command -v curl >/dev/null 2>&1 || die "curl is required but was not found on PATH"
}

nixos_banner() {
    printf '\n'
    say "NixOS detected."
    warn "The supported, idiomatic way to run Wizard on NixOS is Nix, not this script:"
    printf '\n' >&2
    printf '    nix run github:%s              # run without installing\n' "$WIZARD_REPO" >&2
    printf '    nix profile install github:%s  # add to your profile\n' "$WIZARD_REPO" >&2
    printf '    # or add the flake as an input to your system/home configuration\n' >&2
    printf '\n' >&2
    warn "Proceeding with a static musl binary instead → ${WIZARD_INSTALL_DIR}"
    warn "Set WIZARD_INSTALL_DIR to override the install location."
    printf '\n'
}

termux_banner() {
    printf '\n'
    say "Termux detected (Android)."
    warn "No prebuilt Wizard release runs on Termux (Bionic libc, no FHS loader)."
    warn "Building from source into ${WIZARD_INSTALL_DIR}."
    warn "Recommended packages first:"
    printf '\n' >&2
    printf '    pkg install rust git clang make pkg-config openssl curl\n' >&2
    printf '\n' >&2
    warn "Use Termux's rust package — not rustup. A leftover ~/.cargo/bin with"
    warn "no default toolchain shadows pkg's cargo and breaks the build; fix with:"
    printf '\n' >&2
    printf '    rm -rf ~/.cargo ~/.rustup\n' >&2
    printf '\n' >&2
    warn "Local GGUF / stock llama-server and Ollama curl installs are not supported here;"
    warn "use a cloud provider in onboarding, or put a Termux-built llama-server on PATH."
    warn "The native GUI (WIZARD_NATIVE) is skipped — use the TUI."
    printf '\n'
}

# --- llama.cpp ----------------------------------------------------------

llamacpp_asset_url() {
    # $1 = release asset variant, e.g. "ubuntu-x64" or "ubuntu-vulkan-x64".
    # Picks the newest release that actually carries the asset — the most
    # recent tag can still be mid-upload and missing some platforms.
    local url tag
    url="$(curl -fsSL "https://api.github.com/repos/${LLAMACPP_REPO}/releases?per_page=8" 2>/dev/null \
        | grep -o "https://[^\"]*/llama-b[0-9]*-bin-${1}\.tar\.gz" | head -n1 || true)"
    if [ -n "$url" ]; then
        printf '%s' "$url"
        return
    fi
    # API unavailable (rate limit, proxy): derive the tag from the
    # /releases/latest redirect and verify the constructed URL exists.
    tag="$(curl -fsI -o /dev/null -w '%{redirect_url}' \
        "https://github.com/${LLAMACPP_REPO}/releases/latest" 2>/dev/null || true)"
    tag="${tag##*/}"
    if [ -z "$tag" ] || [ "$tag" = "latest" ]; then
        return 1
    fi
    url="https://github.com/${LLAMACPP_REPO}/releases/download/${tag}/llama-${tag}-bin-${1}.tar.gz"
    curl -fsI -o /dev/null "$url" 2>/dev/null || return 1
    printf '%s' "$url"
}

have_vulkan_loader() {
    command -v vulkaninfo >/dev/null 2>&1 && return 0
    ldconfig -p 2>/dev/null | grep -q 'libvulkan\.so'
}

gpu_present() {
    case "$MEM_SOURCE" in
        "GPU VRAM"*) return 0 ;;
        *) return 1 ;;
    esac
}

# Best-effort: on a GPU box with no Vulkan loader, install one so the prebuilt
# Vulkan build of llama.cpp can actually use the GPU. The NVIDIA/AMD drivers
# ship the Vulkan ICD; only the loader (libvulkan) is usually missing — exactly
# the case on hosted notebooks (Colab) where the default install otherwise falls
# back to a CPU build and inference crawls. Never fatal: if no loader can be
# installed, the install proceeds on CPU with a warning.
ensure_vulkan_loader() {
    gpu_present || return 0
    have_vulkan_loader && return 0

    say "GPU detected but no Vulkan loader — installing one so llama.cpp can use the GPU ..."
    local sudo=""
    if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
        sudo="sudo"
    fi
    if command -v apt-get >/dev/null 2>&1; then
        $sudo apt-get update -qq >/dev/null 2>&1 || true
        $sudo apt-get install -y -qq libvulkan1 mesa-vulkan-drivers >/dev/null 2>&1 || true
    elif command -v dnf >/dev/null 2>&1; then
        $sudo dnf install -y vulkan-loader mesa-vulkan-drivers >/dev/null 2>&1 || true
    elif command -v yum >/dev/null 2>&1; then
        $sudo yum install -y vulkan-loader mesa-vulkan-drivers >/dev/null 2>&1 || true
    elif command -v pacman >/dev/null 2>&1; then
        $sudo pacman -Sy --noconfirm vulkan-icd-loader >/dev/null 2>&1 || true
    elif command -v zypper >/dev/null 2>&1; then
        $sudo zypper --non-interactive install libvulkan1 >/dev/null 2>&1 || true
    elif command -v apk >/dev/null 2>&1; then
        $sudo apk add --no-cache vulkan-loader >/dev/null 2>&1 || true
    fi

    if have_vulkan_loader; then
        say "Vulkan loader installed — using the GPU build of llama.cpp"
    else
        warn "could not install a Vulkan loader automatically — llama.cpp will run on CPU"
        warn "install one (e.g. 'apt-get install libvulkan1') and re-run for GPU acceleration"
    fi
}

# Whether the llama.cpp build Wizard installed is a GPU (Vulkan) build. Recorded
# in a .variant marker at install time; absent for installs predating it (and
# for external installs), which read as "not a GPU build" so a GPU box upgrades.
installed_llamacpp_is_gpu_build() {
    local marker="$HOME/.wizard/llama.cpp/.variant"
    [ -f "$marker" ] && grep -qE 'vulkan|cuda' "$marker"
}

# An NVIDIA GPU that nvidia-smi can see.
nvidia_gpu_present() {
    command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1
}

# Whether to build llama.cpp from source with CUDA. llama.cpp ships no prebuilt
# Linux CUDA binary, and the prebuilt Vulkan build cannot see an NVIDIA GPU on
# images that lack the Vulkan ICD (e.g. Colab) — so CUDA, compiled on the box,
# is the only reliable GPU path for NVIDIA. Requires nvcc already present (the
# full CUDA toolkit is multi-GB and not something an installer should pull).
should_build_cuda() {
    [ "$WIZARD_LLAMACPP_NO_CUDA" = "1" ] && return 1
    nvidia_gpu_present || return 1
    command -v nvcc >/dev/null 2>&1
}

# Best-effort install of the tools needed to compile llama.cpp (cmake, a C/C++
# compiler, git). Returns non-zero if any remain missing afterwards.
ensure_build_tools() {
    if ! { command -v cmake >/dev/null 2>&1 && command -v git >/dev/null 2>&1 \
        && { command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; }; }; then
        local sudo=""
        if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
            sudo="sudo"
        fi
        if command -v apt-get >/dev/null 2>&1; then
            $sudo apt-get update -qq >/dev/null 2>&1 || true
            $sudo apt-get install -y -qq cmake build-essential git >/dev/null 2>&1 || true
        elif command -v dnf >/dev/null 2>&1; then
            $sudo dnf install -y cmake gcc gcc-c++ make git >/dev/null 2>&1 || true
        elif command -v pacman >/dev/null 2>&1; then
            $sudo pacman -Sy --noconfirm cmake base-devel git >/dev/null 2>&1 || true
        fi
    fi
    command -v cmake >/dev/null 2>&1 && command -v git >/dev/null 2>&1 \
        && { command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; }
}

# Best-effort install of a C linker, which `cargo build` needs even though no
# C is being written: rustc shells out to `cc` to link, so a machine with a
# Rust toolchain and no compiler gets several minutes into the build and then
# fails on `linker \`cc\` not found`.
#
# Separate from ensure_build_tools() rather than reusing it, because that one
# also demands cmake for llama.cpp and would refuse a machine that has a
# perfectly good compiler but no cmake. Alpine is handled here and not there
# because the source build is the only path a musl host has (see the asset
# sanity check in download_binary), so apk is on the critical path.
#
# Returns non-zero if a linker is still missing afterwards, and names the
# package to install rather than leaving the caller to read cargo's output.
ensure_c_linker() {
    if command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; then
        return 0
    fi
    local sudo=""
    if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
        sudo="sudo"
    fi
    say "No C linker found; installing one (cargo needs it to link) ..."
    if command -v apt-get >/dev/null 2>&1; then
        $sudo apt-get update -qq >/dev/null 2>&1 || true
        $sudo apt-get install -y -qq build-essential >/dev/null 2>&1 || true
    elif command -v dnf >/dev/null 2>&1; then
        $sudo dnf install -y gcc >/dev/null 2>&1 || true
    elif command -v pacman >/dev/null 2>&1; then
        $sudo pacman -Sy --noconfirm base-devel >/dev/null 2>&1 || true
    elif command -v apk >/dev/null 2>&1; then
        $sudo apk add --no-cache build-base >/dev/null 2>&1 || true
    elif command -v zypper >/dev/null 2>&1; then
        $sudo zypper install -y gcc >/dev/null 2>&1 || true
    fi
    command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1
}

# Build llama-server from source with CUDA and install it under
# ~/.wizard/llama.cpp. Returns non-zero on any failure so the caller can fall
# back to the prebuilt Vulkan/CPU path.
build_llamacpp_cuda() {
    say "Building llama.cpp with CUDA for your NVIDIA GPU — this takes a few minutes ..."
    if ! ensure_build_tools; then
        warn "missing build tools (cmake/compiler/git) and could not install them — skipping CUDA build"
        return 1
    fi

    local src="${TMP_DIR}/llamacpp-cuda-src" jobs
    jobs="$(nproc 2>/dev/null || echo 4)"
    rm -rf "$src"
    if ! git clone --depth=1 https://github.com/"${LLAMACPP_REPO}".git "$src" >/dev/null 2>&1; then
        warn "could not clone llama.cpp for the CUDA build"
        return 1
    fi

    # Configure for the GPUs on this box (CMAKE_CUDA_ARCHITECTURES=native needs
    # CMake >= 3.24); retry without it on older CMake.
    if ! cmake -S "$src" -B "$src/build" -DCMAKE_BUILD_TYPE=Release \
        -DGGML_CUDA=ON -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF \
        -DCMAKE_CUDA_ARCHITECTURES=native >/dev/null 2>&1; then
        if ! cmake -S "$src" -B "$src/build" -DCMAKE_BUILD_TYPE=Release \
            -DGGML_CUDA=ON -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF >/dev/null 2>&1; then
            warn "CUDA cmake configure failed (is the CUDA toolkit complete?)"
            return 1
        fi
    fi
    if ! cmake --build "$src/build" --config Release -j "$jobs" --target llama-server >/dev/null 2>&1; then
        warn "CUDA build failed"
        return 1
    fi

    local bin
    bin="$(find "$src/build" -type f -name llama-server | head -n1 || true)"
    if [ -z "$bin" ] || ! "$bin" --version >/dev/null 2>&1; then
        warn "the CUDA-built llama-server did not run"
        return 1
    fi

    local dest="$HOME/.wizard/llama.cpp"
    rm -rf "$dest"
    mkdir -p "$dest"
    # Keep the whole bin/ tree: the build's shared libraries (libggml-cuda.so,
    # …) must sit beside llama-server for its $ORIGIN runpath to resolve them.
    cp -R "$(dirname "$bin")"/. "$dest/"
    printf 'cuda-source\n' >"${dest}/.variant"
    mkdir -p "$LLAMA_BIN_DIR"
    ln -sfn "${dest}/llama-server" "${LLAMA_BIN_DIR}/llama-server"
    say "Installed CUDA llama-server to ${dest}"
    return 0
}

llamacpp_variants() {
    # Candidate release-asset variants, preferred first. llama.cpp ships no
    # Linux CUDA release asset; Vulkan is the prebuilt GPU backend (and it
    # falls back to CPU at runtime when no usable GPU is present), so try it
    # when a GPU and a Vulkan loader were detected, with plain CPU as the
    # safe fallback.
    local suffix="x64"
    [ "$ARCH" = "aarch64" ] && suffix="arm64"
    # macOS ships a single per-arch build with the Metal backend baked in.
    if [ "$OS" = "macos" ]; then
        printf 'macos-%s\n' "$suffix"
        return
    fi
    case "$MEM_SOURCE" in
        "GPU VRAM"*)
            if have_vulkan_loader; then
                printf 'ubuntu-vulkan-%s\n' "$suffix"
            fi
            ;;
    esac
    printf 'ubuntu-%s\n' "$suffix"
}

expose_llama_server() {
    # wizard looks for llama-server on PATH when it starts the server
    # itself, so link it next to the wizard binary when possible.
    local src="${LLAMA_BIN_DIR}/llama-server"
    if [ ! -e "${WIZARD_INSTALL_DIR}/llama-server" ]; then
        if [ -d "$WIZARD_INSTALL_DIR" ] && [ -w "$WIZARD_INSTALL_DIR" ]; then
            ln -sfn "$src" "${WIZARD_INSTALL_DIR}/llama-server"
        elif command -v sudo >/dev/null 2>&1; then
            say "Need elevated permissions to link llama-server into ${WIZARD_INSTALL_DIR}"
            sudo ln -sfn "$src" "${WIZARD_INSTALL_DIR}/llama-server" || true
        fi
    fi
    if ! command -v llama-server >/dev/null 2>&1; then
        case ":$PATH:" in
            *":${LLAMA_BIN_DIR}:"*) ;;
            *) warn "${LLAMA_BIN_DIR} is not on your PATH — add it so wizard can find llama-server" ;;
        esac
    fi
}

install_llamacpp() {
    if [ "$WIZARD_SKIP_LLAMACPP_INSTALL" = "1" ]; then
        say "Skipping llama.cpp install (WIZARD_SKIP_LLAMACPP_INSTALL=1)"
        return
    fi
    # On Termux, stock llama.cpp release assets target Ubuntu glibc and will
    # not run on Bionic. Prefer an existing llama-server; otherwise tell the
    # user how to build one and skip — cloud providers still work.
    if is_termux; then
        if command -v llama-server >/dev/null 2>&1; then
            say "llama-server already on PATH ($(command -v llama-server)) — wizard will use it"
        else
            warn "Termux cannot use the prebuilt llama-server releases (Ubuntu/glibc)."
            warn "Build llama.cpp inside Termux and put llama-server on PATH, e.g.:"
            warn "    pkg install clang cmake git"
            warn "    git clone https://github.com/${LLAMACPP_REPO}.git ~/llama.cpp && cd ~/llama.cpp"
            warn "    cmake -B build -DCMAKE_BUILD_TYPE=Release -DLLAMA_CURL=OFF && cmake --build build -j --target llama-server"
            warn "    ln -sfn \"\$PWD/build/bin/llama-server\" \"\$PREFIX/bin/llama-server\""
            warn "Skipping automatic llama.cpp install for now."
        fi
        return
    fi
    # On NixOS, never compile from source or drop a prebuilt FHS binary — use an
    # existing llama-server if present, otherwise point the user at Nix.
    if is_nixos; then
        if command -v llama-server >/dev/null 2>&1; then
            say "llama-server already on PATH ($(command -v llama-server)) — wizard will use it"
        else
            warn "On NixOS, install llama.cpp declaratively instead of compiling it here:"
            warn "    nix profile install nixpkgs#llama-cpp"
            warn "then re-run (or add it to your system/home configuration). Skipping for now."
        fi
        return
    fi
    # Decide the GPU strategy up front (needs to know whether a GPU is present).
    # NVIDIA → compile a CUDA build (the only reliable NVIDIA path; no prebuilt
    # CUDA asset exists and the Vulkan prebuilt can't see NVIDIA without an ICD).
    # Otherwise, on a GPU box, install a Vulkan loader and use the Vulkan
    # prebuilt. The strategy is non-empty only when a GPU build is actually
    # achievable, so the upgrade check below never churns on un-accelerable boxes.
    [ -n "$MEM_SOURCE" ] || detect_memory
    local gpu_strategy=""
    if gpu_present; then
        if should_build_cuda; then
            gpu_strategy="cuda"
        else
            ensure_vulkan_loader
            have_vulkan_loader && gpu_strategy="vulkan"
        fi
    fi

    # An existing install Wizard manages lives under ~/.wizard/llama.cpp. Upgrade
    # a CPU-only build to a GPU build when one is achievable; otherwise leave it.
    if [ -x "$HOME/.wizard/llama.cpp/llama-server" ]; then
        if [ -n "$gpu_strategy" ] && ! installed_llamacpp_is_gpu_build; then
            say "GPU detected but the installed llama-server is a CPU build — reinstalling a GPU build (${gpu_strategy})"
        else
            say "llama-server already installed at $HOME/.wizard/llama.cpp/llama-server"
            expose_llama_server
            return
        fi
    elif command -v llama-server >/dev/null 2>&1; then
        # An external llama-server (brew, nix, hand-built): never clobber it.
        say "llama-server already installed ($(command -v llama-server)) — leaving it as is"
        return
    fi

    # NVIDIA: compile CUDA; on success we're done, otherwise fall back to prebuilt.
    if [ "$gpu_strategy" = "cuda" ]; then
        if build_llamacpp_cuda; then
            expose_llama_server
            return
        fi
        warn "falling back to a prebuilt llama-server (Vulkan/CPU)"
        ensure_vulkan_loader
    fi

    say "Installing llama-server (llama.cpp official releases) ..."
    local variant url archive dir bin dest
    for variant in $(llamacpp_variants); do
        url="$(llamacpp_asset_url "$variant" || true)"
        if [ -z "$url" ]; then
            warn "no llama.cpp release asset found for ${variant}"
            continue
        fi
        archive="${TMP_DIR}/llamacpp-${variant}.tar.gz"
        say "Downloading ${url##*/} ..."
        if ! curl -fL --progress-bar -o "$archive" "$url"; then
            warn "download failed for ${url##*/}"
            continue
        fi
        dir="${TMP_DIR}/llamacpp-${variant}"
        mkdir -p "$dir"
        if ! tar -xzf "$archive" -C "$dir"; then
            warn "could not extract ${url##*/}"
            continue
        fi
        bin="$(find "$dir" -type f -name llama-server | head -n1 || true)"
        if [ -z "$bin" ]; then
            warn "no llama-server binary inside ${url##*/}"
            continue
        fi
        chmod 755 "$bin"
        # Sanity check before keeping it — a Vulkan build without a usable
        # loader (or a glibc mismatch) fails here, and we try the next variant.
        if ! "$bin" --version >/dev/null 2>&1; then
            warn "the ${variant} build does not run on this system — trying the next variant"
            continue
        fi
        # Keep the whole release tree: llama-server resolves its shared
        # libraries via an \$ORIGIN runpath, so the .so files must stay next
        # to the real binary. PATH only needs the symlink.
        dest="$HOME/.wizard/llama.cpp"
        rm -rf "$dest"
        mkdir -p "$dest"
        cp -R "$(dirname "$bin")"/. "$dest/"
        # Record the variant so a later run knows whether this is a GPU build.
        printf '%s\n' "$variant" >"${dest}/.variant"
        mkdir -p "$LLAMA_BIN_DIR"
        ln -sfn "${dest}/llama-server" "${LLAMA_BIN_DIR}/llama-server"
        say "Installed llama-server to ${dest} (${variant} build)"
        expose_llama_server
        return
    done

    warn "could not install a prebuilt llama-server for ${OS}/${ARCH}"
    warn "install it yourself — wizard will start it automatically once it is on PATH:"
    printf '\n' >&2
    printf '    brew install llama.cpp                  # Homebrew / Linuxbrew\n' >&2
    printf '    nix profile install nixpkgs#llama-cpp   # Nix / NixOS\n' >&2
    printf '    https://github.com/%s — build from source\n' "$LLAMACPP_REPO" >&2
    printf '\n' >&2
}

# --- ollama (WIZARD_USE_OLLAMA=1 or WIZARD_BYOM=1) ------------------------

ollama_running() {
    curl -fsS --max-time 3 "${OLLAMA_URL}/api/tags" >/dev/null 2>&1
}

install_ollama() {
    if [ "$WIZARD_SKIP_OLLAMA_INSTALL" = "1" ]; then
        say "Skipping Ollama install (WIZARD_SKIP_OLLAMA_INSTALL=1)"
        return
    fi
    if command -v ollama >/dev/null 2>&1; then
        say "Ollama already installed"
        return
    fi
    # On NixOS the curl|sh Ollama installer drops an FHS binary that won't run —
    # require a declarative install instead.
    if is_nixos; then
        die "On NixOS, install Ollama declaratively rather than via the curl installer — e.g. 'nix profile install nixpkgs#ollama' (or set services.ollama.enable = true), then re-run."
    fi
    # Ollama's official installer targets desktop Linux/macOS, not Termux.
    if is_termux; then
        die "Ollama's curl installer is not supported on Termux. Use a cloud provider in onboarding, or install a Termux-native runtime yourself and point Wizard at it."
    fi
    say "Installing Ollama (official install script) ..."
    curl -fsSL https://ollama.com/install.sh | sh \
        || die "Ollama installation failed — install it manually from https://ollama.com/download and re-run"
}

start_ollama() {
    if ollama_running; then
        say "Ollama server is running at ${OLLAMA_URL}"
        return
    fi

    if ! command -v ollama >/dev/null 2>&1; then
        if [ "$WIZARD_SKIP_OLLAMA_INSTALL" = "1" ]; then
            warn "Ollama is neither installed nor reachable at ${OLLAMA_URL}; continuing anyway (WIZARD_SKIP_OLLAMA_INSTALL=1)"
            return
        fi
        die "ollama binary not found after install — check the Ollama installation"
    fi

    say "Starting Ollama server ..."
    if command -v systemctl >/dev/null 2>&1 \
        && systemctl list-unit-files ollama.service >/dev/null 2>&1; then
        if [ "$(id -u)" -eq 0 ]; then
            systemctl start ollama || true
        elif command -v sudo >/dev/null 2>&1; then
            sudo systemctl start ollama || true
        fi
    fi

    if ! ollama_running; then
        mkdir -p "$HOME/.wizard/logs"
        nohup ollama serve >"$HOME/.wizard/logs/ollama.log" 2>&1 &
    fi

    local _try
    for _try in $(seq 1 30); do
        if ollama_running; then
            say "Ollama server is up"
            return
        fi
        sleep 1
    done
    die "Ollama server did not come up at ${OLLAMA_URL} within 30s — try 'ollama serve' manually, then re-run"
}

# --- model tier selection -----------------------------------------------

is_uint() {
    # True if $1 is a non-empty string of digits (safe for arithmetic).
    case "$1" in
        '' | *[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

detect_memory() {
    # Prefer GPU VRAM (largest single GPU — the model must fit on one card),
    # fall back to system RAM as a heuristic on CPU-only machines.
    # On total detection failure, leave MEM_SOURCE empty so the caller can
    # fall back to the smallest tier instead of dying.

    # NVIDIA: nvidia-smi can exist but print nothing or garbage (driver
    # mismatch, headless cloud images) — only trust a plain number.
    if command -v nvidia-smi >/dev/null 2>&1; then
        local vram_mib
        vram_mib="$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits 2>/dev/null \
            | sort -nr | head -n1 | tr -d '[:space:]' || true)"
        if is_uint "$vram_mib" && [ "$vram_mib" -gt 0 ]; then
            MEM_GB=$((vram_mib / 1024))
            MEM_SOURCE="GPU VRAM (nvidia-smi)"
            return
        fi
        warn "nvidia-smi is present but did not report usable VRAM (driver mismatch?) — ignoring it"
    fi

    # AMD: rocm-smi if present, else the amdgpu sysfs VRAM counter (bytes).
    if command -v rocm-smi >/dev/null 2>&1; then
        local vram_b
        vram_b="$(rocm-smi --showmeminfo vram --csv 2>/dev/null \
            | awk -F, '$2 ~ /^[0-9]+$/ {print $2}' | sort -nr | head -n1 || true)"
        if is_uint "$vram_b" && [ "$vram_b" -gt 0 ]; then
            MEM_GB=$((vram_b / 1024 / 1024 / 1024))
            MEM_SOURCE="GPU VRAM (rocm-smi)"
            return
        fi
        warn "rocm-smi is present but did not report usable VRAM — ignoring it"
    fi
    local sysfs_file vram_b best=0
    for sysfs_file in /sys/class/drm/card[0-9]*/device/mem_info_vram_total; do
        [ -r "$sysfs_file" ] || continue
        vram_b="$(cat "$sysfs_file" 2>/dev/null || true)"
        if is_uint "$vram_b" && [ "$vram_b" -gt "$best" ]; then
            best="$vram_b"
        fi
    done
    if [ "$best" -gt 0 ]; then
        MEM_GB=$((best / 1024 / 1024 / 1024))
        MEM_SOURCE="GPU VRAM (sysfs amdgpu)"
        return
    fi

    # macOS: Apple Silicon shares unified memory between CPU and the Metal GPU,
    # so total RAM is the right tiering signal (and the Metal-backed llama-server
    # can address most of it). sysctl reports it in bytes.
    #
    # hw.memsize is the exact physical byte count, unlike /proc/meminfo's
    # MemTotal below, which already excludes what the kernel keeps for itself:
    # an "8 GB" Linux laptop reads 7 GB there while an 8 GB Mac would read a
    # flat 8 here, clear the 8 GB tier boundary and be handed a model macOS
    # will not let it load. Net off the same 6% (src/hardware.rs's
    # OS_RESERVED_PERCENT) so both readings mean the same thing, and MEM_GB is
    # usable memory rather than the machine's nameplate figure.
    if [ "$OS" = "macos" ]; then
        local mem_b
        mem_b="$(sysctl -n hw.memsize 2>/dev/null || true)"
        if is_uint "$mem_b" && [ "$mem_b" -gt 0 ]; then
            # Multiply before dividing: `mem_b / 100` first would throw away
            # the remainder of a byte count, and shellcheck flags it (SC2017).
            MEM_GB=$((mem_b * 94 / 100 / 1024 / 1024 / 1024))
            MEM_SOURCE="unified memory (Apple Silicon)"
            return
        fi
    fi

    local mem_kb
    mem_kb="$(awk '/^MemTotal:/ {print $2}' /proc/meminfo 2>/dev/null || true)"
    if is_uint "$mem_kb" && [ "$mem_kb" -gt 0 ]; then
        MEM_GB=$((mem_kb / 1024 / 1024))
        MEM_SOURCE="system RAM (no GPU detected)"
        # In a container MemTotal reports the host's RAM; cap it with the
        # cgroup memory limit (v2 then v1) when one applies. "max" (v2) and
        # huge sentinels (v1's PAGE_COUNTER_MAX, here >= 1<<60) mean no limit.
        local cgroup_file limit_b limit_gb
        for cgroup_file in /sys/fs/cgroup/memory.max /sys/fs/cgroup/memory/memory.limit_in_bytes; do
            [ -r "$cgroup_file" ] || continue
            limit_b="$(cat "$cgroup_file" 2>/dev/null || true)"
            is_uint "$limit_b" || continue
            [ "$limit_b" -lt 1152921504606846976 ] || continue
            limit_gb=$((limit_b / 1024 / 1024 / 1024))
            if [ "$limit_gb" -lt "$MEM_GB" ]; then
                MEM_GB="$limit_gb"
                MEM_SOURCE="system RAM (cgroup limit)"
            fi
        done
        return
    fi

    MEM_GB=0
    MEM_SOURCE=""
}

select_model() {
    if [ -n "$WIZARD_MODEL" ]; then
        MODEL="$WIZARD_MODEL"
        say "Model forced via WIZARD_MODEL: ${MODEL}"
        return
    fi

    detect_memory
    if [ -z "$MEM_SOURCE" ]; then
        MODEL="qwen3.5:4b"
        warn "could not detect GPU VRAM or system RAM — falling back to the smallest model tier"
        say "Selected model tier: ${MODEL} (override with WIZARD_MODEL=<tag>)"
        return
    fi
    say "Detected ${MEM_GB} GB of ${MEM_SOURCE} usable by models"

    # These boundaries are the shell copy of src/hardware.rs's tier table
    # (suggest_ollama_model / suggest_gguf_model); the two are pinned together
    # by install_sh_tier_table_matches_the_rust_tier_table. When they drift the
    # installer downloads one model and wizard's preflight refuses it.
    if [ "$MEM_GB" -ge 24 ]; then
        MODEL="qwen3.6:35b"
    elif [ "$MEM_GB" -ge 18 ]; then
        MODEL="qwen3.6:27b"
    elif [ "$MEM_GB" -ge 8 ]; then
        MODEL="qwen3.5:9b"
    else
        # Under 8 GB the 9B does not fit: ~6 GB of weights plus the KV cache
        # and compute buffers llama-server allocates on top leaves nothing for
        # the OS, so it is OOM-killed while loading rather than merely slow.
        MODEL="qwen3.5:4b"
        warn "less than 8 GB usable, using the smallest tier (${MODEL}); it will run on CPU / partial offload and may be slow"
    fi
    say "Selected model tier: ${MODEL}"
}

gguf_for_model() {
    # Map a model tier tag to its Q4_K_M GGUF on Hugging Face. Leaves
    # GGUF_FILE/GGUF_URL empty for tags with no known download.
    GGUF_FILE=""
    GGUF_URL=""
    case "$1" in
        qwen3.6:35b)
            GGUF_FILE="Qwen3.6-35B-A3B-UD-Q4_K_M.gguf"
            GGUF_URL="https://huggingface.co/unsloth/Qwen3.6-35B-A3B-GGUF/resolve/main/${GGUF_FILE}"
            ;;
        qwen3.6:27b)
            GGUF_FILE="Qwen3.6-27B-Q4_K_M.gguf"
            GGUF_URL="https://huggingface.co/unsloth/Qwen3.6-27B-GGUF/resolve/main/${GGUF_FILE}"
            ;;
        qwen3.5:9b)
            GGUF_FILE="Qwen3.5-9B-Q4_K_M.gguf"
            GGUF_URL="https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/resolve/main/${GGUF_FILE}"
            ;;
        qwen3.5:4b)
            GGUF_FILE="Qwen3.5-4B-Q4_K_M.gguf"
            GGUF_URL="https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/resolve/main/${GGUF_FILE}"
            ;;
    esac
}

download_gguf() {
    gguf_for_model "$MODEL"
    if [ -z "$GGUF_FILE" ]; then
        warn "no known GGUF download for '${MODEL}' — set gguf_path in ~/.wizard/config.toml to your own .gguf file"
        return
    fi
    GGUF_PATH="${MODELS_DIR}/${GGUF_FILE}"
    if [ -f "$GGUF_PATH" ]; then
        say "Model already downloaded: ${GGUF_PATH}"
        return
    fi
    if [ "$WIZARD_SKIP_MODEL_PULL" = "1" ]; then
        say "Skipping model download (WIZARD_SKIP_MODEL_PULL=1)"
        return
    fi
    mkdir -p "$MODELS_DIR"
    say "Downloading ${GGUF_FILE} from Hugging Face (several GB — this can take a while) ..."
    # -C - resumes a partial download from an interrupted earlier run.
    if ! curl -fL -C - --progress-bar -o "${GGUF_PATH}.partial" "$GGUF_URL"; then
        die "failed to download ${GGUF_URL} — check connectivity and disk space, then re-run (the download resumes)"
    fi
    mv "${GGUF_PATH}.partial" "$GGUF_PATH"
    say "Saved ${GGUF_PATH}"
}

pull_model() {
    if [ "$WIZARD_SKIP_MODEL_PULL" = "1" ]; then
        say "Skipping model pull (WIZARD_SKIP_MODEL_PULL=1)"
        return
    fi
    if ! command -v ollama >/dev/null 2>&1; then
        warn "ollama binary not found; skipping model pull — run 'ollama pull ${MODEL}' yourself"
        return
    fi
    say "Pulling ${MODEL} from the Ollama library (this can take a while) ..."
    ollama pull "$MODEL" \
        || die "failed to pull ${MODEL} — check connectivity, then run 'ollama pull ${MODEL}' manually"
}

# --- wizard binary ------------------------------------------------------

place_binary() {
    # $1 = path to the extracted binary, $2 = name to install it as (default
    # "wizard"; the native GUI build goes in beside it as "wizard-native").
    # Sets PLACED_PATH to where it landed.
    local src="$1" name="${2:-wizard}"
    chmod 755 "$src"

    if [ -d "$WIZARD_INSTALL_DIR" ] && [ -w "$WIZARD_INSTALL_DIR" ]; then
        install -m 755 "$src" "${WIZARD_INSTALL_DIR}/${name}"
    elif [ ! -e "$WIZARD_INSTALL_DIR" ] && mkdir -p "$WIZARD_INSTALL_DIR" 2>/dev/null; then
        install -m 755 "$src" "${WIZARD_INSTALL_DIR}/${name}"
    elif command -v sudo >/dev/null 2>&1; then
        say "Need elevated permissions to write to ${WIZARD_INSTALL_DIR}"
        sudo mkdir -p "$WIZARD_INSTALL_DIR"
        sudo install -m 755 "$src" "${WIZARD_INSTALL_DIR}/${name}"
    else
        local fallback="$HOME/.local/bin"
        warn "${WIZARD_INSTALL_DIR} is not writable and sudo is unavailable — installing to ${fallback} instead"
        mkdir -p "$fallback"
        install -m 755 "$src" "${fallback}/${name}"
        case ":$PATH:" in
            *":${fallback}:"*) ;;
            *) warn "${fallback} is not on your PATH — add it to your shell profile" ;;
        esac
        PLACED_PATH="${fallback}/${name}"
        return
    fi
    PLACED_PATH="${WIZARD_INSTALL_DIR}/${name}"
}

# The newest published release tag for $REPO, or empty if it cannot be
# determined. The releases API first; on a rate limit or a proxy that eats it,
# the /releases/latest redirect, which needs no API budget.
latest_release_tag() {
    local tag
    tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
        | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' | head -n1 \
        | sed -E 's/.*"([^"]+)"$/\1/' || true)"
    if [ -z "$tag" ]; then
        tag="$(curl -fsI -o /dev/null -w '%{redirect_url}' \
            "https://github.com/${REPO}/releases/latest" 2>/dev/null || true)"
        tag="${tag##*/}"
        [ "$tag" = "latest" ] && tag=""
    fi
    printf '%s' "$tag"
}

# Resolve the release tag this run is installing into RESOLVED_TAG: the pinned
# WIZARD_VERSION if there is one, otherwise whatever GitHub calls the latest
# release. Once per run, because two callers need the same answer and
# disagreeing about which release is being installed is exactly the confusion
# the signature check below exists to catch — and because otherwise every
# caller pays another API request.
#
# Sets a variable rather than printing one: `$(release_tag)` would run the body
# in a subshell, where the cache it just filled dies with it and the caller sees
# an empty RESOLVED_TAG.
#
# RESOLVED_TAG is left empty when neither source could name a tag, which also
# means resolve_mirror_base skipped the mirror.
resolve_release_tag() {
    [ "$TAG_RESOLVED" = "1" ] && return 0
    TAG_RESOLVED=1
    if [ -n "$WIZARD_VERSION" ]; then
        RESOLVED_TAG="$WIZARD_VERSION"
    else
        RESOLVED_TAG="$(latest_release_tag)"
    fi
}

# Resolve WIZARD_MIRROR into MIRROR_BASE, once. Empty MIRROR_BASE means "no
# mirror", which is both the default and what every unusable setting degrades
# to: a mirror is an optimisation, so a bad one must cost a warning, never an
# install.
#
# The release tag comes from GitHub even when the mirror is on, and the mirror
# is then read at <mirror>/<tag>/. That is deliberate: it keeps GitHub the
# authority on *which* version you get, so a mirror that stopped updating can
# only fail to answer (and be fallen back from), never quietly hold you on an
# old release. It also means the client never reads the mutable /latest/ prefix,
# which exists on the mirror for humans and scripts that want a URL that does
# not change.
resolve_mirror_base() {
    [ "$MIRROR_RESOLVED" = "1" ] && return 0
    MIRROR_RESOLVED=1

    local host="$WIZARD_MIRROR" tag
    # Case-insensitively, and with `tr` rather than bash 4's ${x,,}: macOS still
    # ships bash 3.2 and this script runs there. Same off-switches as
    # `mirror_root` in src/update.rs, so one documented spelling works on both.
    case "$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')" in
        "" | 0 | off | none | false) return 0 ;;
    esac
    case "$host" in
        *://*) ;;
        *) host="https://${host}" ;;
    esac
    host="${host%/}"

    resolve_release_tag
    tag="$RESOLVED_TAG"
    if [ -z "$tag" ]; then
        warn "could not determine the latest release tag, so the download mirror (${host}) is skipped — using GitHub releases"
        return 0
    fi
    MIRROR_BASE="${host}/${tag}"
    say "Download mirror: ${MIRROR_BASE} (GitHub releases remain the fallback)"
}

download_release_asset() {
    # $1 = release asset name, $2 = output path. Sets DOWNLOAD_SOURCE to
    # whoever served it.
    #
    # The mirror is tried first and *any* failure falls back to GitHub, which
    # stays the source of truth. Nothing about verification changes with the
    # source: this is the only function that fetches a release file, so the
    # signature check on checksums.txt and the sha256 check on every tarball
    # are the same checks on the same bytes whichever host answered.
    #
    # On the GitHub leg, plain curl covers public releases; on a private repo
    # the unauthenticated asset URL returns plain 404, so fall back to an
    # authenticated `gh release download` when the gh CLI is available.
    local asset="$1" out="$2"

    resolve_mirror_base
    if [ -n "$MIRROR_BASE" ]; then
        if curl -fsSL -o "$out" "${MIRROR_BASE}/${asset}" 2>/dev/null; then
            DOWNLOAD_SOURCE="the mirror at ${MIRROR_BASE}"
            return 0
        fi
        rm -f "$out"
        if [ "$MIRROR_WARNED" = "0" ]; then
            MIRROR_WARNED=1
            warn "the download mirror did not serve ${asset} (${MIRROR_BASE}) — falling back to GitHub releases"
        fi
    fi

    if curl -fsSL -o "$out" "${RELEASE_BASE}/${asset}" 2>/dev/null; then
        DOWNLOAD_SOURCE="GitHub releases (${REPO})"
        return 0
    fi
    rm -f "$out"
    if command -v gh >/dev/null 2>&1; then
        # Pass the pinned tag when set; without one gh picks the latest release.
        if gh release download ${WIZARD_VERSION:+"$WIZARD_VERSION"} \
            --repo "$REPO" --pattern "$asset" \
            --output "$out" 2>/dev/null; then
            DOWNLOAD_SOURCE="GitHub releases (${REPO}, authenticated via gh)"
            return 0
        fi
        rm -f "$out"
    fi
    return 1
}

# --- release verification -----------------------------------------------
#
# Every downloaded tarball is checked against the release's checksums.txt, and
# checksums.txt is itself checked against its detached minisign signature
# (checksums.txt.minisig) under the key above. Both are mandatory: this script
# has no flag, variable or fallback that installs a binary it could not verify,
# because it is run as `curl | bash` and the binary it places is then run as
# you. The way out of a failure is never to skip a check, it is to build from
# source (WIZARD_BUILD_FROM_SOURCE=1), which trusts the git history instead.

# The tail of every refusal, so the message ends in something to do.
verify_hint() {
    printf 'install minisign (Debian/Ubuntu: sudo apt install minisign; macOS: brew install minisign; Alpine: apk add minisign; Nix: nix-shell -p minisign) or a python3, and re-run, or build from source with WIZARD_BUILD_FROM_SOURCE=1'
}

# Where a verifier hides when it is installed but not on PATH, searched after
# PATH and never instead of it. macOS is the whole reason this list exists:
# Homebrew's openssl@3 is keg-only, so it is installed but deliberately never
# linked onto PATH, and a `curl | bash` run inherits whatever PATH the terminal
# had — which on a fresh machine can miss /opt/homebrew/bin entirely.
VERIFIER_EXTRA_PATHS="/opt/homebrew/bin
/usr/local/bin
/opt/local/bin
/home/linuxbrew/.linuxbrew/bin
/opt/homebrew/opt/openssl@3/bin
/usr/local/opt/openssl@3/bin
/opt/homebrew/opt/openssl@1.1/bin
/usr/local/opt/openssl@1.1/bin
/opt/local/libexec/openssl3/bin"

# Print the path to $1: PATH first, then the locations above. Prints nothing and
# returns 1 when it is in none of them.
find_tool() {
    local name="$1" dir found
    found="$(command -v "$name" 2>/dev/null || true)"
    if [ -n "$found" ]; then
        printf '%s\n' "$found"
        return 0
    fi
    while IFS= read -r dir; do
        [ -n "$dir" ] || continue
        if [ -x "${dir}/${name}" ]; then
            printf '%s\n' "${dir}/${name}"
            return 0
        fi
    done <<EOF
${VERIFIER_EXTRA_PATHS}
EOF
    return 1
}

# True when the openssl at $1 can check an ed25519 signature over raw bytes and
# hash with blake2b-512, which is what minisign's two algorithms need. Probed
# rather than assumed: macOS ships LibreSSL as `openssl`, and it has neither, so
# without this a missing feature would be reported as a bad signature.
openssl_can_verify() {
    local bin="$1"
    [ -n "$bin" ] && [ -x "$bin" ] || return 1
    "$bin" pkeyutl -help 2>&1 | grep -q -- '-rawin' || return 1
    "$bin" dgst -blake2b512 </dev/null >/dev/null 2>&1
}

# Print the path to an openssl that passes the probe above, PATH first and then
# the extra locations. On a Mac the one on PATH is Apple's LibreSSL and fails
# the probe, while a Homebrew openssl@3 sitting off PATH passes it, so this
# looks past the first openssl it finds rather than giving up on it.
find_capable_openssl() {
    local dir bin
    bin="$(command -v openssl 2>/dev/null || true)"
    if openssl_can_verify "$bin"; then
        printf '%s\n' "$bin"
        return 0
    fi
    while IFS= read -r dir; do
        [ -n "$dir" ] || continue
        if openssl_can_verify "${dir}/openssl"; then
            printf '%s\n' "${dir}/openssl"
            return 0
        fi
    done <<EOF
${VERIFIER_EXTRA_PATHS}
EOF
    return 1
}

# Print the path to a python3 that can run verify_signature_python. Probed and
# not just found, because /usr/bin/python3 on a Mac without the Command Line
# Tools is a stub that runs nothing: it exits non-zero, and this notices.
find_python() {
    local bin
    bin="$(find_tool python3 || true)"
    [ -n "$bin" ] || return 1
    # Apple's /usr/bin/python3 is a stub. Without the Command Line Tools it runs
    # nothing, and running it is how a GUI installer prompt appears in front of
    # somebody who typed a curl | bash. xcode-select answers the same question
    # quietly, so it is asked first and only about that one path.
    if [ "$bin" = "/usr/bin/python3" ] && command -v xcode-select >/dev/null 2>&1; then
        xcode-select -p >/dev/null 2>&1 || return 1
    fi
    "$bin" -c 'import hashlib; hashlib.blake2b(b"").digest()' >/dev/null 2>&1 || return 1
    printf '%s\n' "$bin"
}

# minisign verification with openssl ($3), for hosts that have no minisign. A
# .minisig is four lines: an untrusted comment, base64(algorithm, key id and a
# 64-byte ed25519 signature), a trusted comment, and base64 of a second
# signature over (signature followed by trusted comment). "ED" signs a blake2b-512
# prehash of the file, the legacy "Ed" signs the file itself.
verify_signature_openssl() {
    local file="$1" sig="$2" openssl="$3" work="${TMP_DIR}/minisig" algorithm pub_id sig_id
    rm -rf "$work"
    mkdir -p "$work" || return 1

    printf '%s\n' "$WIZARD_RELEASE_PUBKEY" | "$openssl" base64 -d -A >"${work}/pub.bin" 2>/dev/null || return 1
    [ "$(wc -c <"${work}/pub.bin")" -eq 42 ] || return 1
    sed -n 2p "$sig" | "$openssl" base64 -d -A >"${work}/sig.bin" 2>/dev/null || return 1
    [ "$(wc -c <"${work}/sig.bin")" -eq 74 ] || return 1

    # The key id is a hint about which key to reach for, never the check.
    pub_id="$(dd if="${work}/pub.bin" bs=1 skip=2 count=8 2>/dev/null | od -An -tx1 | tr -d ' \n')"
    sig_id="$(dd if="${work}/sig.bin" bs=1 skip=2 count=8 2>/dev/null | od -An -tx1 | tr -d ' \n')"
    [ "$pub_id" = "$sig_id" ] || return 1

    # An ed25519 SubjectPublicKeyInfo is a fixed 12-byte header plus the raw
    # key, which is how openssl is handed a key minisign stored bare.
    printf '\060\052\060\005\006\003\053\145\160\003\041\000' >"${work}/key.der" || return 1
    tail -c 32 "${work}/pub.bin" >>"${work}/key.der" || return 1
    tail -c 64 "${work}/sig.bin" >"${work}/sig.raw" || return 1

    algorithm="$(dd if="${work}/sig.bin" bs=1 count=2 2>/dev/null)"
    case "$algorithm" in
        ED) "$openssl" dgst -blake2b512 -binary "$file" >"${work}/message.bin" 2>/dev/null || return 1 ;;
        Ed) cp "$file" "${work}/message.bin" || return 1 ;;
        *)  return 1 ;;
    esac
    "$openssl" pkeyutl -verify -pubin -inkey "${work}/key.der" -keyform DER \
        -rawin -sigfile "${work}/sig.raw" -in "${work}/message.bin" >/dev/null 2>&1 || return 1

    # The trusted comment is inside the signed envelope; verifying it is what
    # keeps it from being an unauthenticated field riding along in a signed file.
    {
        cat "${work}/sig.raw"
        sed -n 3p "$sig" | sed 's/^trusted comment: //' | tr -d '\n'
    } >"${work}/global.bin"
    sed -n 4p "$sig" | "$openssl" base64 -d -A >"${work}/global.sig" 2>/dev/null || return 1
    "$openssl" pkeyutl -verify -pubin -inkey "${work}/key.der" -keyform DER \
        -rawin -sigfile "${work}/global.sig" -in "${work}/global.bin" >/dev/null 2>&1
}

# The same check again with python3 ($3) and nothing else, for a host that has
# neither minisign nor an openssl that can do this — which is a Mac as it comes
# out of the box, and was an install that could not proceed at all.
#
# ed25519 verification is written out here rather than imported: `hashlib` is in
# the standard library and gives us sha512 and blake2b, but nothing in it does
# curve arithmetic, and `cryptography` is a third-party wheel this script cannot
# assume. What follows is the verify half of RFC 8032 over the standard library,
# checking the same two signatures over the same bytes as the two paths above.
# There is no secret here and nothing to leak by timing: every input is a public
# release file, and the only outcome is yes or no.
verify_signature_python() {
    local file="$1" sig="$2" python="$3"
    "$python" - "$WIZARD_RELEASE_PUBKEY" "$sig" "$file" <<'PYTHON_VERIFY' >/dev/null 2>&1
import base64, hashlib, sys

P = 2**255 - 19
Q = 2**252 + 27742317777372353535851937790883648493
D = -121665 * pow(121666, P - 2, P) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)


def recover_x(y, sign):
    """The x that goes with a compressed y, or None if the point is not on the curve."""
    if y >= P:
        return None
    x2 = (y * y - 1) * pow(D * y * y + 1, P - 2, P) % P
    if x2 == 0:
        return None if sign else 0
    x = pow(x2, (P + 3) // 8, P)
    if (x * x - x2) % P != 0:
        x = x * SQRT_M1 % P
    if (x * x - x2) % P != 0:
        return None
    if x % 2 != sign:
        x = P - x
    return x


# Extended coordinates (X, Y, Z, T), which keep the addition below branch-free.
G_Y = 4 * pow(5, P - 2, P) % P
G_X = recover_x(G_Y, 0)
G = (G_X, G_Y, 1, G_X * G_Y % P)


def add(p1, p2):
    a = (p1[1] - p1[0]) * (p2[1] - p2[0]) % P
    b = (p1[1] + p1[0]) * (p2[1] + p2[0]) % P
    c = 2 * p1[3] * p2[3] * D % P
    dd = 2 * p1[2] * p2[2] % P
    e, f, g, h = b - a, dd - c, dd + c, b + a
    return (e * f % P, g * h % P, f * g % P, e * h % P)


def mul(s, point):
    out = (0, 1, 1, 0)
    while s > 0:
        if s & 1:
            out = add(out, point)
        point = add(point, point)
        s >>= 1
    return out


def equal(p1, p2):
    if (p1[0] * p2[2] - p2[0] * p1[2]) % P != 0:
        return False
    return (p1[1] * p2[2] - p2[1] * p1[2]) % P == 0


def decompress(data):
    if len(data) != 32:
        return None
    y = int.from_bytes(data, "little")
    sign = y >> 255
    y &= (1 << 255) - 1
    x = recover_x(y, sign)
    return None if x is None else (x, y, 1, x * y % P)


def verify(public, message, signature):
    if len(signature) != 64:
        return False
    key = decompress(public)
    if key is None:
        return False
    r = decompress(signature[:32])
    if r is None:
        return False
    s = int.from_bytes(signature[32:], "little")
    if s >= Q:
        return False
    h = int.from_bytes(hashlib.sha512(signature[:32] + public + message).digest(), "little") % Q
    return equal(mul(s, G), add(r, mul(h, key)))


def decode(line, size):
    raw = base64.b64decode(line.strip(), validate=True)
    if len(raw) != size:
        raise ValueError("wrong length")
    return raw


try:
    public_key = decode(sys.argv[1], 42)
    lines = open(sys.argv[2], "r").read().splitlines()
    signature = decode(lines[1], 74)
    global_signature = base64.b64decode(lines[3].strip(), validate=True)
    payload = open(sys.argv[3], "rb").read()
except Exception:
    sys.exit(1)

if public_key[:2] != b"Ed":
    sys.exit(1)
# The key id is a hint about which key to reach for, never the check.
if signature[2:10] != public_key[2:10]:
    sys.exit(1)

if signature[:2] == b"ED":
    message = hashlib.blake2b(payload).digest()
elif signature[:2] == b"Ed":
    message = payload
else:
    sys.exit(1)

key, raw = public_key[10:], signature[10:]
if not verify(key, message, raw):
    sys.exit(1)

# The trusted comment is inside the signed envelope; verifying it is what keeps
# it from being an unauthenticated field riding along in a signed file.
comment = lines[2]
if not comment.startswith("trusted comment: "):
    sys.exit(1)
if not verify(key, raw + comment[len("trusted comment: "):].encode(), global_signature):
    sys.exit(1)
sys.exit(0)
PYTHON_VERIFY
}

# Check $1 against the detached signature $2. Three outcomes, because the fixes
# differ: 0 verified, 1 the signature is wrong, 2 this host has nothing that
# can check one. Two and one are both fatal to the caller.
#
# Three ways to check the same two signatures, tried in order of how much of the
# work is somebody else's audited code: minisign itself, then an openssl that
# has ed25519 and blake2b, then the standard-library implementation above. The
# third one exists because the first two are both absent from a stock Mac, which
# made every macOS install fail here with nothing to install and no way to skip
# the check — the refusal was correct and the host was simply unserved.
verify_signature() {
    local bin
    if bin="$(find_tool minisign)"; then
        "$bin" -Vqm "$1" -x "$2" -P "$WIZARD_RELEASE_PUBKEY" >/dev/null 2>&1 || return 1
        return 0
    fi
    if bin="$(find_capable_openssl)"; then
        verify_signature_openssl "$1" "$2" "$bin" || return 1
        return 0
    fi
    if bin="$(find_python)"; then
        verify_signature_python "$1" "$2" "$bin" || return 1
        return 0
    fi
    return 2
}

# True when the trusted comment $1 names the release tag $2.
#
# A whole-word match rather than a fixed wording, mirroring binds_to_tag() in
# src/update.rs so the installer and `wizard update` agree: the release workflow
# signs `-t "wizard <tag> checksums, signed by the wizard release key"`, and that
# sentence must be free to change without breaking verification. What it cannot
# do is match a *different* release's comment, which is the whole point.
comment_names_tag() {
    local comment="$1" tag="$2" words
    [ -n "$tag" ] || return 1
    # Everything that is not part of a tag becomes a separator, so the tag has
    # to sit between two of them: v1.0 must not match "v1.0.0".
    words=" $(printf '%s' "$comment" | sed 's/[^A-Za-z0-9._-]/ /g') "
    case "$words" in
        *" $tag "*) return 0 ;;
    esac
    return 1
}

# Require the signature just verified over $1 (a .minisig) to have been made for
# the release being installed. Fatal on a mismatch: nothing is installed.
#
# Without this, a signature only proves that *some* release was signed by the
# release key, never which one. Asset names carry no version, so a host serving
# <mirror>/v2.0.0/ can answer with v1.0.0's genuine, key-signed checksums.txt,
# signature and tarball: the key id matches, both signatures verify, and every
# digest matches its own checksums.txt. The user is moved to whichever earlier
# release the attacker prefers — one with a known hole, say — which is a signed
# downgrade, and it is what would make SECURITY.md's "a mirror cannot hold you on
# an old release" false. The mirror is tried before GitHub, so this is the
# installer's exposure, not a theoretical one.
#
# Line 3 of a .minisig is the trusted comment, and it is signed data by the time
# this runs: minisign -V checks the global signature over it, and
# verify_signature_openssl checks the same signature itself, so both verification
# paths have already authenticated the bytes being read here.
require_signature_names_tag() {
    local sig="$1" tag comment
    resolve_release_tag
    tag="$RESOLVED_TAG"
    if [ -z "$tag" ]; then
        # No tag could be resolved, so there is nothing to compare against —
        # and, because resolve_mirror_base reads the same cached answer, no
        # mirror was consulted either: these bytes came from GitHub's own
        # /releases/latest/download. Say so rather than implying a check ran.
        warn "could not determine the release tag, so the signature was not checked against a specific release (GitHub's latest release served these files; set WIZARD_VERSION=<tag> to pin one)"
        return 0
    fi
    comment="$(sed -n 3p "$sig" | sed 's/^trusted comment: //')"
    if ! comment_names_tag "$comment" "$tag"; then
        die "this signature was made for a different release: its signed comment reads \"${comment}\", not ${tag} (served by ${DOWNLOAD_SOURCE:-the release host}). A host that answers a request for one release with another release's genuinely signed files is trying to move you off the version you asked for; nothing was installed. If a new release published seconds ago, retry; otherwise pin one with WIZARD_VERSION=<tag>, or build from source with WIZARD_BUILD_FROM_SOURCE=1"
    fi
}

# Die if this script carries the placeholder instead of a real signing key.
#
# Called from two places on purpose. verify_release_checksums() is the
# authoritative gate — it sits ahead of every download that must be verified,
# including the mirror path, and it is what makes the refusal unbypassable.
# main() calls it too, before any asset is fetched, so a user on an unsigned
# release learns that in a second rather than after an 8 MB download. One
# function rather than two copies of the message, because a security refusal
# whose two wordings drift apart teaches the reader to trust neither.
refuse_placeholder_key() {
    case "$WIZARD_RELEASE_PUBKEY" in
        RELEASE-SIGNING-KEY-NOT-YET-GENERATED*)
            die "this install.sh carries no release signing key, so it cannot verify any release; build from source with WIZARD_BUILD_FROM_SOURCE=1, or use an install.sh from a release that has one"
            ;;
    esac
}

# Fetch the release's checksums.txt and verify its signature, once per run.
# Leaves the verified file at ${TMP_DIR}/checksums.txt. Every failure is fatal:
# an unverified checksums.txt is not a checksums.txt.
verify_release_checksums() {
    local sums="${TMP_DIR}/checksums.txt" sig="${TMP_DIR}/checksums.txt.minisig" rc=0
    if [ "$CHECKSUMS_VERIFIED" = "1" ]; then
        return 0
    fi

    refuse_placeholder_key
    [ -f "$sums" ] || download_release_asset "checksums.txt" "$sums" \
        || die "the release published no checksums.txt (or it could not be downloaded), so its binaries cannot be verified; retry, pin a different release with WIZARD_VERSION=<tag>, or build from source with WIZARD_BUILD_FROM_SOURCE=1"
    download_release_asset "checksums.txt.minisig" "$sig" \
        || die "the release published no checksums.txt.minisig, so its checksums cannot be authenticated; retry, pin a different release with WIZARD_VERSION=<tag>, or build from source with WIZARD_BUILD_FROM_SOURCE=1"

    verify_signature "$sums" "$sig" || rc=$?
    case "$rc" in
        0) ;;
        2) die "no way to check the release signature on this host: no minisign, no openssl with ed25519 and blake2b (macOS ships LibreSSL, which has neither), and no python3; $(verify_hint)" ;;
        *) die "release signature verification FAILED: checksums.txt does not match its signature under the wizard release key; the download is corrupted or tampered with, and nothing was installed" ;;
    esac
    # Signed by the release key *and* signed for this release; announced only
    # once both hold, so the line never claims more than was checked.
    require_signature_names_tag "$sig"
    say "Release checksums.txt signature verified (minisign${RESOLVED_TAG:+, signed for ${RESOLVED_TAG}})"
    CHECKSUMS_VERIFIED=1
}

verify_checksum() {
    # $1 = path to the downloaded tarball, $2 = asset name. Fatal on anything
    # that leaves the tarball unverified, including a host with no sha256 tool.
    local tarball="$1" asset="$2" sums="${TMP_DIR}/checksums.txt" expected actual
    verify_release_checksums
    # `sha256sum` writes `<hex>  <name>`, or `<hex> *<name>` in binary mode.
    expected="$(awk -v a="$asset" '$2 == a || $2 == "*" a {print $1; exit}' "$sums" || true)"
    if [ -z "$expected" ]; then
        die "the release's signed checksums.txt has no entry for ${asset}, so it cannot be verified; pin a different release with WIZARD_VERSION=<tag>, or build from source with WIZARD_BUILD_FROM_SOURCE=1"
    fi
    # sha256sum on Linux; macOS ships `shasum -a 256` instead.
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tarball" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$tarball" | awk '{print $1}')"
    else
        die "no sha256 tool on PATH, so ${asset} cannot be verified; install coreutils (Debian/Ubuntu: sudo apt install coreutils; Alpine: apk add coreutils) or perl's shasum, or build from source with WIZARD_BUILD_FROM_SOURCE=1"
    fi
    if [ "$actual" != "$expected" ]; then
        die "checksum mismatch for ${asset} (expected ${expected}, got ${actual}) — the download may be corrupted or tampered with; aborting"
    fi
    say "Checksum and signature verified for ${asset}"
}

download_binary() {
    # Termux cannot run published gnu/musl assets — never attempt the download.
    if is_termux; then
        warn "skipping prebuilt download on Termux (no Android/Bionic release asset)"
        return
    fi
    say "Downloading wizard binary (${REPO}) ..."
    local asset bin assets
    # macOS ships a single per-arch Mach-O asset. On Linux, NixOS can't run the
    # glibc (gnu) binary — no dynamic loader at the FHS path — so prefer the
    # static musl asset there. Elsewhere try gnu first but keep musl as a
    # fallback: if the gnu binary fails its sanity check (loader/glibc mismatch
    # on an old or unusual host), the loop drops to the static musl build.
    if [ "$OS" = "macos" ]; then
        assets="wizard-${ARCH}-apple-darwin.tar.gz"
    elif is_nixos; then
        assets="wizard-${ARCH}-unknown-linux-musl.tar.gz wizard-${ARCH}-unknown-linux-gnu.tar.gz wizard-linux-${ARCH}.tar.gz"
    else
        assets="wizard-${ARCH}-unknown-linux-gnu.tar.gz wizard-${ARCH}-unknown-linux-musl.tar.gz wizard-linux-${ARCH}.tar.gz"
    fi
    for asset in $assets; do
        if download_release_asset "$asset" "${TMP_DIR}/${asset}"; then
            verify_checksum "${TMP_DIR}/${asset}" "${asset}"
            # Unpack each asset into its own directory and search only that.
            # Extracting them all into a shared TMP_DIR and taking the first
            # `find` hit lets a rejected binary answer for the asset tried
            # after it, in whatever order the directory happens to walk —
            # which would install the very binary the sanity check below
            # just refused. `fetch_native_binary` unpacks per-asset for the
            # same reason.
            local unpack="${TMP_DIR}/unpack-${asset}"
            rm -rf "$unpack"
            mkdir -p "$unpack" || continue
            tar -xzf "${TMP_DIR}/${asset}" -C "$unpack" || continue
            bin="$(find "$unpack" -type f -name wizard | head -n1 || true)"
            if [ -z "$bin" ]; then
                warn "no wizard binary inside ${asset}"
                continue
            fi
            chmod 755 "$bin"
            # Sanity check before installing — catches a corrupt download or
            # a glibc mismatch instead of declaring success with a dud binary.
            if ! "$bin" --version >/dev/null 2>&1; then
                warn "the binary from ${asset} does not run on this system"
                continue
            fi
            place_binary "$bin"
            INSTALLED_PATH="$PLACED_PATH"
            BINARY_INSTALLED=1
            say "Installed wizard to ${INSTALLED_PATH} (from ${DOWNLOAD_SOURCE})"
            return
        fi
    done

    warn "could not download a prebuilt wizard binary for ${OS}/${ARCH}"
    warn "(a 404 here also happens when ${REPO} is private — 'gh auth login' enables an authenticated download)"
    warn "you can build it from source instead (requires a Rust toolchain):"
    printf '\n' >&2
    printf '    git clone https://github.com/%s ~/.wizard/src\n' "$REPO" >&2
    printf '    cd ~/.wizard/src && cargo build --release\n' >&2
    printf '    install -m 755 target/release/wizard %s/wizard\n' "$WIZARD_INSTALL_DIR" >&2
    printf '\n' >&2
}

# --- rust toolchain (optional, for deep evolve) -------------------------

# True when $1 (default: cargo on PATH) runs and can print a version.
# A rustup *proxy* can exist on PATH while no default toolchain is
# configured — `command -v cargo` succeeds, `cargo --version` does not.
cargo_works() {
    local c="${1:-cargo}"
    if [ "$c" = "cargo" ]; then
        command -v cargo >/dev/null 2>&1 || return 1
    else
        [ -x "$c" ] || [ -f "$c" ] || return 1
    fi
    "$c" --version >/dev/null 2>&1
}

# Put $1 first on PATH. When it is not ~/.cargo/bin, drop ~/.cargo/bin so a
# broken rustup shim cannot shadow a working Termux/distro cargo/rustc.
prefer_bin_dir() {
    local dir="$1"
    local newpath="$dir"
    local p
    local oifs="$IFS"
    IFS=':'
    # shellcheck disable=SC2086
    for p in $PATH; do
        [ -z "$p" ] && continue
        [ "$p" = "$dir" ] && continue
        if [ "$dir" != "$HOME/.cargo/bin" ] && [ "$p" = "$HOME/.cargo/bin" ]; then
            continue
        fi
        newpath="$newpath:$p"
    done
    IFS="$oifs"
    export PATH="$newpath"
}

# Walk PATH (and ~/.cargo/bin) for an executable cargo that actually runs.
# Prints the absolute path on stdout. Returns 1 when none work.
find_working_cargo() {
    local dir candidate
    local oifs="$IFS"
    IFS=':'
    # shellcheck disable=SC2086
    for dir in $PATH; do
        [ -z "$dir" ] && continue
        candidate="$dir/cargo"
        if cargo_works "$candidate"; then
            IFS="$oifs"
            printf '%s' "$candidate"
            return 0
        fi
    done
    IFS="$oifs"
    candidate="$HOME/.cargo/bin/cargo"
    if cargo_works "$candidate"; then
        printf '%s' "$candidate"
        return 0
    fi
    return 1
}

ensure_rust_toolchain() {
    # Do NOT prepend ~/.cargo/bin before probing. A partial rustup install
    # leaves cargo/rustc shims there that fail with:
    #   rustup could not choose a version of cargo to run
    # and would shadow a working Termux (`pkg install rust`) or distro cargo.
    local cargo_path dir
    if cargo_path="$(find_working_cargo)"; then
        dir="$(cd "$(dirname "$cargo_path")" && pwd)"
        prefer_bin_dir "$dir"
        say "Rust toolchain already present (cargo found at ${cargo_path})"
        return
    fi

    # rustup present but no default toolchain — common after a interrupted
    # install or a hand-copied ~/.cargo. Skip this on Termux: rustup's
    # default host triples target glibc desktop Linux, not Android/Bionic.
    if ! is_termux; then
        local ru=""
        if command -v rustup >/dev/null 2>&1; then
            ru="$(command -v rustup)"
        elif [ -x "$HOME/.cargo/bin/rustup" ]; then
            ru="$HOME/.cargo/bin/rustup"
        fi
        if [ -n "$ru" ]; then
            say "Found rustup without a working cargo — running 'rustup default stable' ..."
            if "$ru" default stable; then
                prefer_bin_dir "$HOME/.cargo/bin"
                if cargo_path="$(find_working_cargo)"; then
                    say "Rust toolchain ready (rustup default stable)"
                    return
                fi
            fi
            warn "'rustup default stable' did not yield a working cargo; will try a fresh rustup install"
        fi
    fi

    if is_termux; then
        die "No working Rust toolchain found on Termux.

Install the Termux package toolchain (not rustup):

    pkg install rust git clang make pkg-config openssl curl

If a broken rustup install is shadowing it, remove the shims and retry:

    rm -rf \"\$HOME/.cargo\" \"\$HOME/.rustup\"

Then re-run the Wizard installer."
    fi

    say "Installing minimal Rust toolchain via rustup ..."
    curl -fsSL https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path \
        || die "rustup installation failed — install Rust manually from https://rustup.rs"
    prefer_bin_dir "$HOME/.cargo/bin"
    cargo_works cargo \
        || die "cargo not found after rustup install — check ~/.cargo/bin"
    say "Rust toolchain installed under ~/.cargo"
}

install_toolchain() {
    if [ "$WIZARD_WITH_TOOLCHAIN" != "1" ]; then
        return
    fi
    say "Ensuring Rust toolchain for deep evolve (WIZARD_WITH_TOOLCHAIN=1) ..."
    ensure_rust_toolchain
}

# --- build from source --------------------------------------------------

resolve_source_ref() {
    # Prefer the newest published release tag so a source build compiles a
    # known-good, CI-passed commit instead of the moving tip of main. An
    # explicit WIZARD_REF always wins; main is the last resort, used only
    # when the repo has no published release at all.
    local tag
    if [ -n "$WIZARD_REF" ]; then
        printf '%s' "$WIZARD_REF"
        return
    fi
    if [ -n "$WIZARD_VERSION" ]; then
        printf '%s' "$WIZARD_VERSION"
        return
    fi
    tag="$(latest_release_tag)"
    if [ -n "$tag" ]; then
        printf '%s' "$tag"
    else
        warn "no published release found for ${REPO} — building from main (unreviewed tip; set WIZARD_REF to pin a ref)"
        printf 'main'
    fi
}

build_from_source() {
    command -v git >/dev/null 2>&1 \
        || die "git is required to build from source but was not found on PATH"
    local ref
    ref="$(resolve_source_ref)"
    say "Building wizard from source (${WIZARD_REPO}@${ref}) ..."
    local src_dir="${TMP_DIR}/wizard-src"
    git clone --depth 1 --branch "$ref" \
        "https://github.com/${WIZARD_REPO}" "$src_dir" \
        || die "git clone failed — check WIZARD_REPO (${WIZARD_REPO}) and the ref (${ref})"
    ensure_c_linker \
        || die "a C linker (cc) is required to build from source and none could be installed — install your distro's compiler package (build-essential on Debian/Ubuntu, gcc on Fedora, base-devel on Arch, build-base on Alpine) and re-run"
    ensure_rust_toolchain
    # Unquoted on purpose: empty means no argument, which is the stock build.
    # See the WIZARD_PROFILE block near the top for why that has to be the
    # literal command and not the default feature list spelled out.
    # shellcheck disable=SC2086
    say "Running cargo build --release ${CARGO_FEATURE_FLAGS} (this may take several minutes) ..."
    # shellcheck disable=SC2086
    ( cd "$src_dir" && cargo build --release $CARGO_FEATURE_FLAGS ) \
        || die "cargo build --release ${CARGO_FEATURE_FLAGS} failed — see output above for details"
    local bin="${src_dir}/target/release/wizard"
    [ -f "$bin" ] \
        || die "build succeeded but target/release/wizard not found in ${src_dir}"
    "$bin" --version >/dev/null 2>&1 \
        || die "the built binary does not run ('wizard --version' failed) — the ${ref} ref may be broken"
    place_binary "$bin"
    INSTALLED_PATH="$PLACED_PATH"
    BINARY_INSTALLED=1
    say "Installed wizard (built from source) to ${INSTALLED_PATH}"
}

# --- native GUI (WIZARD_NATIVE=1) ---------------------------------------

# `wizard gui` opens an iced window in the agent's own process. iced
# is behind an off-by-default cargo feature (`Cargo.toml` `[features]` says
# why), so it needs its own build — and that build ships as a *separate*
# binary, `wizard-native`, which never replaces `wizard`.
#
# Two binaries rather than one, for the same reason the webview shell this
# replaced had two, minus the webview's reason. There is no shared library to
# be missing here: the asset links only libc, libm and libgcc_s, because
# `tiny-skia` means no wgpu and winit reaches X11 and Wayland through `dlopen`
# at the moment a window opens. What is still true is that the native asset
# exists only for the gnu and darwin targets — `dlopen` from a fully static
# musl binary does not work — so on a host where `wizard` itself is the musl
# build, replacing it with this one would trade a binary that runs anywhere for
# a binary that runs here. Keeping them apart means the graphical build is
# strictly additive.

native_asset_name() {
    case "$OS" in
        macos) printf 'wizard-native-%s-apple-darwin.tar.gz' "$ARCH" ;;
        # No musl native asset: see above.
        *)     printf 'wizard-native-%s-unknown-linux-gnu.tar.gz' "$ARCH" ;;
    esac
}

# Sets NATIVE_BIN to a runnable native-GUI binary; returns nonzero if one
# could not be obtained. Mirrors download_binary / build_from_source.
#
# The path comes back in a global, and the caller must never wrap this in
# `$(…)`. verify_checksum below reaches die() on a digest mismatch, an
# unsignable release or a signature that does not verify, and inside a command
# substitution that `exit 1` ends only the subshell: the install would carry on,
# report "no runnable native build", and finish 0 after refusing a tampered
# asset. Nothing unverified would be installed either way, but "every failure
# aborts" (SECURITY.md) would be false and the user would be told the wrong
# reason. This is the only place in the script where a die() sits under a
# function that a caller could capture, so it is kept uncapturable instead.
fetch_native_binary() {
    local asset bin src_dir
    NATIVE_BIN=""
    if [ "$WIZARD_BUILD_FROM_SOURCE" = "1" ]; then
        src_dir="${TMP_DIR}/wizard-src"
        [ -d "$src_dir" ] || return 1
        say "Building the native GUI from source (--features native) ..."
        ( cd "$src_dir" && cargo build --release --features native ) || return 1
        bin="${src_dir}/target/release/wizard"
    else
        asset="$(native_asset_name)"
        download_release_asset "$asset" "${TMP_DIR}/${asset}" || return 1
        verify_checksum "${TMP_DIR}/${asset}" "$asset"
        local unpack="${TMP_DIR}/native"
        mkdir -p "$unpack"
        tar -xzf "${TMP_DIR}/${asset}" -C "$unpack" || return 1
        bin="$(find "$unpack" -type f -name wizard | head -n1 || true)"
    fi
    [ -n "$bin" ] && [ -f "$bin" ] || return 1
    chmod 755 "$bin"
    # `--version` returns before any window is opened, so this proves the
    # binary links and reaches main without needing a display.
    "$bin" --version >/dev/null 2>&1 || return 1
    NATIVE_BIN="$bin"
}

install_native_gui() {
    [ "$WIZARD_NATIVE" = "1" ] || return 0

    printf '\n'
    say "Native GUI (WIZARD_NATIVE=1): the agent in its own window"

    if is_termux; then
        warn "the native GUI is not supported on Termux (no display server, no Bionic asset)"
        warn "use the TUI ('wizard') — skipping WIZARD_NATIVE"
        return 0
    fi

    # A verification failure never lands here: verify_checksum aborts the whole
    # install rather than returning, so reaching this branch means the asset
    # could not be fetched, held no wizard binary, or does not run on this host.
    if ! fetch_native_binary; then
        warn "could not install the native GUI for ${OS}/${ARCH} (no native asset could be fetched, or the build inside it does not run here)"
        warn "use the TUI ('wizard') — 'wizard gui' needs this build, and the browser GUI is gone"
        return 0
    fi

    place_binary "$NATIVE_BIN" "wizard-native"
    NATIVE_PATH="$PLACED_PATH"
    NATIVE_INSTALLED=1
    say "Installed wizard-native to ${NATIVE_PATH}"
}

# --- config -------------------------------------------------------------

# Rewrite the first `model = …` line of $1 to name the model $2, in place.
#
# awk reading the tag out of the environment, rather than sed with it spliced
# into the script: the tag is user input (WIZARD_MODEL), `|` was this sed's own
# delimiter — so WIZARD_MODEL='a|b' made sed exit non-zero and, under `set -e`,
# aborted the installer *after* the binary was already in place — and `&` and
# `\1` are replacement syntax in every sed there is. Nothing is interpolated
# here, so the tag is used literally whatever it contains.
#
# The first line only. `s|…|…|` with no address rewrites every match, so a
# config with two `[[providers]]` blocks had *both* models replaced while the
# caller said "other settings preserved". The caller has already established
# there is exactly one.
rewrite_model_line() {
    local cfg="$1" tmp
    tmp="${cfg}.wizard-new.$$"
    if ! WIZARD_NEW_MODEL="$2" awk '
        BEGIN { done = 0 }
        done == 0 && /^[[:space:]]*model[[:space:]]*=/ {
            printf "model = \"%s\"\n", ENVIRON["WIZARD_NEW_MODEL"]
            done = 1
            next
        }
        { print }
    ' "$cfg" >"$tmp"; then
        rm -f "$tmp"
        return 1
    fi
    # Truncate and rewrite rather than `mv`, so the file keeps its own mode.
    cat "$tmp" >"$cfg"
    rm -f "$tmp"
}

write_config() {
    local cfg="$HOME/.wizard/config.toml"
    mkdir -p "$HOME/.wizard"
    # Only the local flavors and a BYOM install with WIZARD_MODEL set write a
    # config. The default, minimal, and plain BYOM flavors leave it to
    # onboarding on the first `wizard` run (BYOM's model choice lives there).
    if [ "$WIZARD_MINIMAL" = "1" ] \
        || { [ "$WIZARD_LOCAL" != "1" ] && [ "$WIZARD_USE_OLLAMA" != "1" ] \
            && { [ "$WIZARD_BYOM" != "1" ] || [ -z "$MODEL" ]; }; }; then
        if [ -f "$cfg" ]; then
            say "A config already exists at ${cfg} — leaving it untouched"
            say "Run 'wizard --onboard' to reconfigure from scratch"
        else
            say "No config written — the first 'wizard' run starts onboarding"
        fi
        return
    fi
    if [ -f "$cfg" ]; then
        if [ "$WIZARD_BYOM" = "1" ]; then
            # Don't clobber an existing config — only record the chosen model.
            #
            # A `model =` line belongs to a provider block, and a config can
            # declare several. Rewriting all of them would repoint providers
            # the user never mentioned at a model they may not serve, so more
            # than one is a case this script refuses to guess at rather than
            # get wrong quietly.
            local model_lines
            model_lines="$(grep -cE '^[[:space:]]*model[[:space:]]*=' "$cfg" || true)"
            if [ "$model_lines" -eq 0 ]; then
                printf 'model = "%s"\n' "$MODEL" >>"$cfg"
                say "Added model = \"${MODEL}\" to the existing config (other settings preserved)"
            elif [ "$model_lines" -eq 1 ]; then
                rewrite_model_line "$cfg" "$MODEL" \
                    || die "could not update the model in ${cfg}"
                say "Updated model = \"${MODEL}\" in the existing config (other settings preserved)"
            else
                say "Existing config at ${cfg} declares ${model_lines} models, one per provider — leaving it untouched"
                say "Set the model with /provider inside wizard, or edit ${cfg}"
            fi
            return
        fi
        say "Existing config found at ${cfg} — leaving it untouched"
        say "To switch models or providers, edit it or use /provider inside wizard"
        return
    fi
    say "Writing ${cfg}"
    if [ "$WIZARD_USE_OLLAMA" = "1" ] || [ "$WIZARD_BYOM" = "1" ]; then
        cat >"$cfg" <<EOF
# Wizard configuration — see https://github.com/${REPO}
active_provider = "local"
mode = "genie"
# 0 = no step limit: a turn runs until the model is done.
max_steps = 0

[[providers]]
name = "local"
kind = "ollama"
base_url = "${OLLAMA_URL}"
model = "${MODEL}"
EOF
        return
    fi
    # llama-server ignores the request model name; the GGUF stem keeps
    # wizard's labels meaningful. gguf_path lets wizard start the server.
    local model_name="$MODEL"
    if [ -n "$GGUF_FILE" ]; then
        model_name="${GGUF_FILE%.gguf}"
    fi
    cat >"$cfg" <<EOF
# Wizard configuration — see https://github.com/${REPO}
active_provider = "local"
mode = "genie"
# 0 = no step limit: a turn runs until the model is done.
max_steps = 0

[[providers]]
name = "local"
kind = "llamacpp"
base_url = "${LLAMACPP_URL}"
model = "${model_name}"
EOF
    if [ -n "$GGUF_PATH" ]; then
        printf 'gguf_path = "%s"\n' "$GGUF_PATH" >>"$cfg"
    fi
}

# --- default loadout ------------------------------------------------------
# Browser MCP + subagent roster, written into ~/.wizard/. The canonical
# source for these files is the repo's loadout/ directory (loadout/mcp.toml,
# loadout/subagents/*.toml); they are embedded here as verbatim heredocs so
# the curl|bash one-liner works without a repo checkout. When you change one
# side, change the other — keep the two in sync.

loadout_file() {
    # $1 = destination path, $2 = short label; the file body arrives on
    # stdin (heredoc). Never overwrites: an existing file always wins.
    local dest="$1" label="$2"
    if [ -f "$dest" ]; then
        say "Existing ${dest} — leaving it untouched"
        return
    fi
    cat >"$dest"
    say "Installed ${label} (${dest})"
}

install_loadout() {
    if [ "$WIZARD_MINIMAL" = "1" ]; then
        say "Minimal install: skipping the default loadout (browser MCP, subagents)"
        return
    fi
    say "Laying down the default loadout (browser MCP + subagents) ..."
    mkdir -p "$HOME/.wizard/subagents"

    loadout_file "$HOME/.wizard/mcp.toml" "MCP servers: Playwright browser" <<'EOF'
# Wizard MCP server declarations — installed to ~/.wizard/mcp.toml
#
# Part of Wizard's default loadout. This directory (loadout/) is the canonical
# source; install.sh embeds a verbatim copy as a heredoc so the curl|bash
# one-liner works without a repo checkout — keep the two in sync.
#
# Each [[server]] is a Model Context Protocol server whose tools merge into
# Wizard's tool registry. New servers (or edits here) become active the next
# time Wizard starts, or immediately when you run /reload in the TUI.
#
# The Playwright MCP server below gives Wizard a real browser: navigate, click,
# type, and snapshot tools for reading pages, filling forms, and computer-use
# style tasks. It is spawned over stdio as `npx -y @playwright/mcp@latest`, so
# it requires Node and `npx` on your PATH. If Node is missing, this server is
# skipped with a warning at startup and the rest of Wizard works normally —
# install Node, then `/reload`.

[[server]]
name = "playwright"
transport = "stdio"
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
EOF

    loadout_file "$HOME/.wizard/subagents/reviewer.toml" "subagent: reviewer" <<'EOF'
name = "reviewer"
description = "Code-review specialist. Reads a diff or set of files and reports correctness bugs, security issues, and style problems. Read-only: never edits, runs, or commits anything."

# Read/search/git tools only — the reviewer inspects, it does not change code.
tool_scope = ["read_file", "list_files", "search_files", "git_status", "git_diff"]


system_prompt = """
You are the reviewer subagent of Wizard, a local agent. Your one job is
to review code and report findings. You cannot and must not edit, run, or
commit anything — you only have read, search, and git-inspection tools.

Method:
1. Establish scope. If reviewing changes, run `git_diff` (and `git_status` for
   untracked files) to see exactly what changed. If reviewing specific files,
   read them. Read enough surrounding context to judge each change correctly —
   a diff hunk in isolation lies.
2. Look, in priority order, for:
   - Correctness bugs: wrong logic, off-by-one, unhandled error/None/null
     paths, race conditions, resource leaks, broken invariants.
   - Security issues: injection, unvalidated input, secrets in code, unsafe
     deserialization, path traversal, missing authz checks.
   - API/contract breaks: changed signatures, behavior that callers depend on.
   - Tests: are the changes covered? Do existing tests still hold?
   - Clarity and style: naming, dead code, duplication, missing error context —
     reported last and clearly marked as lower priority.
3. For each finding give: file and line, severity (blocker / should-fix /
   nit), what is wrong, and a concrete suggested fix.

Be specific and honest. Do not invent problems to seem thorough; if the change
is clean, say so. Do not rubber-stamp; if you are unsure whether something is a
bug, say what would confirm it. End with a short verdict: APPROVE,
APPROVE-WITH-NITS, or REQUEST-CHANGES, followed by the findings list.
"""
EOF

    loadout_file "$HOME/.wizard/subagents/researcher.toml" "subagent: researcher" <<'EOF'
name = "researcher"
description = "Web research specialist. Uses the Playwright browser (MCP) to read pages, follow links, and gather facts, then reports a concise sourced summary. Use for questions that need current, external information."

# No tool_scope: the researcher gets the parent's full tool set, which includes
# the Playwright browser MCP tools (navigate / click / type / snapshot) shipped
# in mcp.toml. Without scope it can reach those browser tools.


system_prompt = """
You are the researcher subagent of Wizard, a local agent. Your job is to
answer a question using information from the web and report back. You have a
real browser available through the Playwright MCP tools (navigate, click, type,
snapshot, and related). Use them — do not claim you cannot browse.

Method:
1. Plan a couple of search angles for the question. Navigate to a search engine
   or directly to a likely-authoritative source (official docs, release notes,
   the project's own repo) and read the page via a snapshot.
2. Follow links and open additional pages as needed. Prefer primary sources
   (official documentation, source repositories, standards) over blog
   recaps. Cross-check anything surprising against a second source.
3. Extract the specific facts that answer the question. Note version numbers,
   dates, and exact quotes where precision matters.

If the browser tools are unavailable (Node/npx not installed, server failed to
start), say so plainly and report whatever you could determine from your own
knowledge, clearly labeled as un-verified — do not fabricate page contents or
citations.

Report concisely: lead with the direct answer, then the supporting findings,
then the URLs you actually visited as sources. Distinguish what you confirmed
from a source versus what you inferred. Never invent a source or a quote.
"""
EOF

    loadout_file "$HOME/.wizard/subagents/tester.toml" "subagent: tester" <<'EOF'
name = "tester"
description = "Test specialist. Runs the project's test suite, diagnoses failures, and fixes them — editing code or tests as appropriate — until the suite passes or the failure is clearly explained."

# Can read, search, edit/write, and run commands. No git tools: the tester
# fixes and verifies; committing is the parent's decision.
tool_scope = ["read_file", "write_file", "edit_file", "list_files", "search_files", "execute"]


system_prompt = """
You are the tester subagent of Wizard, a local agent. Your job is to get
the project's tests passing — or to explain precisely why they cannot pass.

Method:
1. Discover how this project is tested. Look for the build/test commands in
   AGENTS.md, WIZARD.md, README, or the manifest (Cargo.toml, package.json,
   pyproject.toml, Makefile, etc.). Common commands: `cargo test`, `npm test`,
   `pytest`, `go test ./...`, `make test`.
2. Run the suite with `execute` and read the full output. Identify the first
   real failure (compile/lint errors before test assertions).
3. Diagnose the root cause by reading the failing test and the code under test.
   Decide whether the bug is in the implementation or in the test itself, and
   fix the correct one. Do not delete or weaken a test to make it pass, and do
   not assert behavior the code never promised — fix the real defect.
4. Re-run the suite after each change. Iterate until it is green.

Rules:
- Make the smallest change that correctly fixes the failure.
- Never fabricate a passing result. If the suite still fails, report the exact
  failing tests and error output, your diagnosis, and what you changed.
- If a failure is environmental (missing dependency, no network, missing
  toolchain) and you cannot resolve it, say so explicitly rather than masking it.

Report: the command you ran, the final pass/fail state with counts, what you
changed and why, and any failures left unresolved with their cause.
"""
EOF

    loadout_file "$HOME/.wizard/subagents/documenter.toml" "subagent: documenter" <<'EOF'
name = "documenter"
description = "Documentation specialist. Writes and updates READMEs, docs pages, and code comments so they accurately match the code. Edits prose and docs, never application logic."

# Read/search to understand the code, edit/write to produce docs. No execute or
# git: the documenter writes documentation, it does not run or commit code.
tool_scope = ["read_file", "write_file", "edit_file", "list_files", "search_files"]


system_prompt = """
You are the documenter subagent of Wizard, a local agent. Your job is to
produce documentation that is accurate, clear, and matched to the actual code:
READMEs, docs pages, usage examples, and doc comments.

Voice:
- Sound human-written. Short sentences. Plain words. No filler.
- No slop: skip hype, throat-clearing, and vague claims ("powerful", "seamless",
  "robust", "in today's world", "it's worth noting"). Cut anything that does not
  help the reader do or understand something.
- Be concise. Prefer one clear paragraph or a tight list over a long essay.
- Do not use em dashes (—) unless the user explicitly asks for them. Use commas,
  periods, colons, or parentheses instead.

Method:
1. Read the relevant code and any existing docs before writing a word. Your
   documentation must describe what the code actually does, not what it ought
   to do. Trace function signatures, public APIs, config keys, CLI flags, and
   defaults to their source.
2. Match the existing documentation's voice, structure, and formatting. Reuse
   established headings and conventions; do not invent a new style. Still apply
   the Voice rules above unless the surrounding docs clearly require otherwise.
3. Write for the reader: lead with what the thing is and how to use it, then
   details. Prefer concrete, runnable examples over abstract description.

Rules:
- Never document behavior you have not verified in the source. Do not invent
  flags, options, return values, or benchmarks. If something is ambiguous, note
  the ambiguity rather than guessing.
- Keep examples correct and minimal. Every command or snippet you show should
  actually work as written.
- Edit only documentation and comments. Do not change application logic; if you
  notice a code bug while documenting, report it rather than fixing it.

Report: which files you wrote or updated, and a one-line summary of each change.
"""
EOF

    if ! command -v npx >/dev/null 2>&1; then
        warn "Node/npx not found on PATH — the Playwright browser server will be skipped at startup."
        warn "Install Node (https://nodejs.org), then run /reload in Wizard to activate the browser."
    fi
}

# --- main ---------------------------------------------------------------

main() {
    say "Wizard installer"
    if [ "$WIZARD_MINIMAL" = "1" ] && [ "$WIZARD_BYOM" = "1" ]; then
        die "WIZARD_MINIMAL=1 and WIZARD_BYOM=1 conflict — pick one: minimal installs the binary only (onboarding on first run), BYOM also sets up Ollama"
    fi
    if [ "$WIZARD_LOCAL" = "1" ] && [ "$WIZARD_MINIMAL" = "1" ]; then
        die "WIZARD_LOCAL=1 and WIZARD_MINIMAL=1 conflict — pick one: local preinstalls llama.cpp and an auto-tiered model, minimal installs the binary only (onboarding on first run)"
    fi
    if [ "$WIZARD_LOCAL" = "1" ] && [ "$WIZARD_BYOM" = "1" ]; then
        die "WIZARD_LOCAL=1 and WIZARD_BYOM=1 conflict — pick one: local preinstalls llama.cpp with an auto-tiered model, BYOM sets up Ollama and leaves the model choice to onboarding"
    fi
    require_curl
    detect_platform

    if is_termux; then
        termux_banner
    elif is_nixos; then
        nixos_banner
    fi

    # Soft-skip local-stack flavors on Termux: stock llama/Ollama installers
    # cannot deliver a working runtime here. Still install the binary so the
    # user can pick a cloud provider in onboarding.
    if is_termux; then
        if [ "$WIZARD_LOCAL" = "1" ] && [ "$WIZARD_USE_OLLAMA" != "1" ]; then
            warn "WIZARD_LOCAL=1 on Termux: skipping stock llama.cpp preinstall (no matching prebuilt)."
            warn "Cloud providers work; for on-device models build llama-server yourself (see banner)."
            WIZARD_LOCAL=0
        fi
        if [ "$WIZARD_BYOM" = "1" ] || [ "$WIZARD_USE_OLLAMA" = "1" ]; then
            warn "Ollama install flavors are not supported on Termux — installing the binary only."
            WIZARD_BYOM=0
            WIZARD_USE_OLLAMA=0
            WIZARD_LOCAL=0
        fi
    fi

    # Refuse an unsignable release *before* any of the flavors below, not just
    # before the binary download further down.
    #
    # The flavors are where the bytes are: WIZARD_LOCAL installs llama.cpp and
    # pulls a hardware-tiered GGUF, which is several gigabytes and can be
    # twenty; WIZARD_USE_OLLAMA installs Ollama and pulls a model. Refusing
    # after that is refusing after the expensive part, and the first version of
    # this guard did exactly that — it sat below the flavor block, so
    # `WIZARD_LOCAL=1` downloaded a model and then declined to install the
    # binary that would have used it.
    #
    # Skipped when building from source, which needs no signature at all.
    if [ "$WIZARD_BUILD_FROM_SOURCE" != "1" ]; then
        refuse_placeholder_key
    fi

    if [ "$WIZARD_MINIMAL" = "1" ]; then
        say "Minimal install (WIZARD_MINIMAL=1): binary only — no model runtime, model, config, or loadout"
    elif [ "$WIZARD_BYOM" = "1" ]; then
        say "BYOM install (WIZARD_BYOM=1): Ollama + binary — model choice happens in onboarding"
        install_ollama
        start_ollama
        # WIZARD_MODEL=<tag> is the headless path: pull the tag and write the
        # config here, no onboarding needed. Without it, no model and no
        # config: the first `wizard` run opens onboarding, which pulls the
        # tag you pick.
        if [ -n "$WIZARD_MODEL" ]; then
            MODEL="$WIZARD_MODEL"
            say "Model set via WIZARD_MODEL: ${MODEL}"
            pull_model
        fi
    elif [ "$WIZARD_USE_OLLAMA" = "1" ]; then
        say "Using Ollama as the local provider (WIZARD_USE_OLLAMA=1)"
        install_ollama
        start_ollama
        select_model
        pull_model
    elif [ "$WIZARD_LOCAL" = "1" ]; then
        say "Local install (WIZARD_LOCAL=1): llama.cpp runtime + hardware-tiered model"
        install_llamacpp
        select_model
        download_gguf
    else
        say "Default install: binary + loadout — pick a provider in onboarding on first run"
    fi

    if [ "$WIZARD_BUILD_FROM_SOURCE" = "1" ]; then
        build_from_source
    else
        # The early refusal above has already run for this path;
        # verify_release_checksums() refuses again on the way past, and that
        # one is the authoritative gate.
        download_binary
        if [ "$BINARY_INSTALLED" != "1" ]; then
            say "No prebuilt binary found; falling back to building from source ..."
            build_from_source
        fi
    fi

    install_toolchain
    write_config
    install_loadout
    install_native_gui

    printf '\n'
    if [ "$NATIVE_INSTALLED" = "1" ]; then
        say "Native GUI installed. Open the window with: wizard-native gui"
    fi
    if [ "$BINARY_INSTALLED" = "1" ]; then
        if is_termux; then
            say "Done. Run 'wizard' from Termux — pick a cloud provider in onboarding (or a Termux-built local runtime)."
        elif [ "$WIZARD_MINIMAL" = "1" ]; then
            say "Done. Run 'wizard' to start onboarding (pick your model, provider, and gateway)."
        elif [ "$WIZARD_BYOM" = "1" ] && [ -z "$MODEL" ]; then
            say "Done. Run 'wizard' — pick your Ollama model in onboarding; it is pulled on first run."
        elif [ "$WIZARD_BYOM" = "1" ] || [ "$WIZARD_USE_OLLAMA" = "1" ]; then
            say "Done. Run: wizard"
        elif [ "$WIZARD_LOCAL" = "1" ]; then
            say "Done. Run: wizard — it starts llama-server with your model automatically."
        else
            say "Done. Run 'wizard' — it asks which provider to use (Local is one pick: it downloads a model sized to your hardware and sets up llama.cpp for you)."
        fi
    else
        say "Setup finished, but the wizard binary was NOT installed — see the build-from-source steps above."
    fi
}

# Sourced with WIZARD_SELFTEST=1, the script stops here: every function above is
# defined and nothing is installed, so the suite can drive the download and
# verification helpers directly against a stub `curl` (see the installer tests in
# `src/update.rs`). Any other invocation runs the installer, which is what the
# `curl | bash` one-liner does.
if [ "${WIZARD_SELFTEST:-0}" != "1" ]; then
    main "$@"
fi
