#!/usr/bin/env bash
#
# Wizard installer — builds from source.
#
#   curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
#
# Wizard's backend is the NexAU code agent (AHE), reached over a Python bridge,
# so installing it means: clone the repo to a stable location, build the Python
# backend (uv sync → NexAU) and the Rust binary (cargo), and put `wizard` on
# PATH. The binary resolves its bridge/venv/agent from the clone via the
# baked-in build path, so THE CLONE MUST STAY IN PLACE — don't delete it.
#
# After install, run `wizard`; the first run opens onboarding (pick a provider:
# xAI sign-in, an API key, or a local llama.cpp/Ollama endpoint).
#
# Environment variables:
#   WIZARD_HOME    where to clone and keep the repo   (default ~/.local/share/wizard)
#   WIZARD_REPO    owner/repo to install from          (default teddytennant/wizard)
#   WIZARD_REF     git branch or tag to build          (default main)
#   WIZARD_NO_DOCKER_CHECK  1 = skip the optional Docker check (default 0)

set -euo pipefail

WIZARD_HOME="${WIZARD_HOME:-$HOME/.local/share/wizard}"
WIZARD_REPO="${WIZARD_REPO:-teddytennant/wizard}"
WIZARD_REF="${WIZARD_REF:-main}"
WIZARD_NO_DOCKER_CHECK="${WIZARD_NO_DOCKER_CHECK:-0}"

# --- output helpers -----------------------------------------------------

say()  { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- platform -----------------------------------------------------------

detect_platform() {
    local os
    os="$(uname -s)"
    case "$os" in
        Linux) ;;
        Darwin) die "macOS is not supported yet — Linux only for now. Sorry!" ;;
        *) die "unsupported operating system: $os (Linux only)" ;;
    esac
    say "Platform: $(uname -s)/$(uname -m)"
}

# --- prerequisites ------------------------------------------------------

ensure_git() {
    command -v git >/dev/null 2>&1 || die "git is required but was not found on PATH"
}

ensure_rust() {
    # rustup may have been installed without touching the shell profile.
    case ":${PATH}:" in
        *":$HOME/.cargo/bin:"*) ;;
        *) export PATH="$HOME/.cargo/bin:$PATH" ;;
    esac
    if command -v cargo >/dev/null 2>&1; then
        say "Rust toolchain present ($(command -v cargo))"
        return
    fi
    say "Installing the Rust toolchain via rustup ..."
    command -v curl >/dev/null 2>&1 || die "curl is required to install Rust"
    curl -fsSL https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path \
        || die "rustup install failed — install Rust manually from https://rustup.rs"
    export PATH="$HOME/.cargo/bin:$PATH"
    command -v cargo >/dev/null 2>&1 || die "cargo not found after rustup install"
    say "Rust installed under ~/.cargo"
}

ensure_uv() {
    case ":${PATH}:" in
        *":$HOME/.local/bin:"*) ;;
        *) export PATH="$HOME/.local/bin:$PATH" ;;
    esac
    if command -v uv >/dev/null 2>&1; then
        say "uv present ($(command -v uv))"
        return
    fi
    say "Installing uv (the Python package manager) ..."
    command -v curl >/dev/null 2>&1 || die "curl is required to install uv"
    curl -LsSf https://astral.sh/uv/install.sh | sh \
        || die "uv install failed — install it manually from https://docs.astral.sh/uv/"
    export PATH="$HOME/.local/bin:$PATH"
    command -v uv >/dev/null 2>&1 || die "uv not found after install — add ~/.local/bin to PATH"
    say "uv installed"
}

check_docker() {
    # Docker is only needed for `wizard evolve` (the local AHE evolution loop).
    [ "$WIZARD_NO_DOCKER_CHECK" = "1" ] && return
    if command -v docker >/dev/null 2>&1 && docker ps >/dev/null 2>&1; then
        say "Docker is available (used by 'wizard evolve' for local harness evolution)"
    else
        warn "Docker not found or not running — chat works without it, but 'wizard evolve' needs a local Docker daemon."
    fi
}

# --- source -------------------------------------------------------------

fetch_source() {
    if [ -d "$WIZARD_HOME/.git" ]; then
        say "Updating existing checkout at ${WIZARD_HOME} (${WIZARD_REF}) ..."
        git -C "$WIZARD_HOME" fetch --depth 1 origin "$WIZARD_REF" \
            || die "git fetch failed in ${WIZARD_HOME}"
        git -C "$WIZARD_HOME" checkout -q FETCH_HEAD \
            || die "git checkout failed in ${WIZARD_HOME}"
    else
        [ -e "$WIZARD_HOME" ] && die "${WIZARD_HOME} exists but is not a git checkout — move it aside or set WIZARD_HOME"
        say "Cloning ${WIZARD_REPO}@${WIZARD_REF} into ${WIZARD_HOME} ..."
        mkdir -p "$(dirname "$WIZARD_HOME")"
        git clone --depth 1 --branch "$WIZARD_REF" \
            "https://github.com/${WIZARD_REPO}" "$WIZARD_HOME" \
            || die "git clone failed — check WIZARD_REPO (${WIZARD_REPO}) and WIZARD_REF (${WIZARD_REF})"
    fi
}

# --- build --------------------------------------------------------------

build_backend() {
    say "Building the Python backend (uv sync → NexAU; first run downloads deps) ..."
    ( cd "$WIZARD_HOME" && uv sync ) \
        || die "uv sync failed in ${WIZARD_HOME} — see output above"
}

build_binary() {
    say "Building and installing the wizard binary (cargo install; this can take a few minutes) ..."
    # --path bakes ${WIZARD_HOME} as the manifest dir, so the installed binary
    # resolves the bridge/venv/agent from the clone. Installs to ~/.cargo/bin.
    ( cd "$WIZARD_HOME" && cargo install --path . --locked --force ) \
        || die "cargo install failed — see output above"
    INSTALLED="$HOME/.cargo/bin/wizard"
    [ -x "$INSTALLED" ] || die "build succeeded but ${INSTALLED} is missing"
    "$INSTALLED" --version >/dev/null 2>&1 || die "the built binary does not run ('wizard --version' failed)"
    case ":$PATH:" in
        *":$HOME/.cargo/bin:"*) ;;
        *) warn "~/.cargo/bin is not on your PATH — add it to your shell profile so 'wizard' is found" ;;
    esac
}

# --- main ---------------------------------------------------------------

main() {
    say "Wizard installer (build from source)"
    detect_platform
    ensure_git
    ensure_rust
    ensure_uv
    check_docker
    fetch_source
    build_backend
    build_binary

    printf '\n'
    say "Done. Run 'wizard' to start — the first run opens onboarding."
    say "Installed: ~/.cargo/bin/wizard  ·  source kept at: ${WIZARD_HOME} (do not delete it)"
}

main "$@"
