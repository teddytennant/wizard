#!/bin/sh
# Execute install.sh inside a throwaway container and prove that what it
# installed actually runs.
#
# install.sh is 1600+ lines of branching (NixOS static-musl preference,
# sudo versus ~/.local/bin, gnu-then-musl fallback, checksum verification,
# loadout) that shellcheck reads and nothing executes. This is the executor:
# CI mounts the repo read-only at /src and runs this script as the container
# command, once per image in the matrix (see the `install-script` job in
# .github/workflows/ci.yml).
#
# Usage: install-in-container.sh [flavor]
#   flavor  "default" (binary + loadout; the curl|bash one-liner) or
#           "minimal" (binary only). Defaults to "default".
#
# Environment:
#   SIMULATE_NIXOS=1  create /etc/NIXOS before running, so install.sh takes
#                     its NixOS branch. See the note where it is handled.
#   VERIFIER=python   leave python3 as the only signature checker in the
#                     container. See the note where it is handled.
set -eu

FLAVOR="${1:-default}"
VERIFIER="${VERIFIER:-auto}"
INSTALLER="${INSTALLER:-/src/install.sh}"

log() { printf '\033[1;35m[container]\033[0m %s\n' "$*"; }
die() {
    printf '\033[1;31m[container] error:\033[0m %s\n' "$*" >&2
    exit 1
}

[ -r "$INSTALLER" ] || die "no installer at ${INSTALLER} (mount the repo at /src)"

# --- prerequisites ------------------------------------------------------
#
# Deliberately the minimum install.sh needs, not a build environment. It is a
# bash script with `set -euo pipefail` that shells out to curl, tar, find,
# awk, sed and sha256sum, and none of the four base images ship all of that:
# alpine has no bash and no curl, the debian/ubuntu images have no
# ca-certificates (every https download would fail on TLS, not on the branch
# under test), and nixos/nix has no awk or sed. Installing them here keeps a
# failure below attributable to install.sh rather than to the image.
#
# A signature checker is on the list for the same reason. install.sh verifies
# checksums.txt against its minisign signature before it unpacks anything and
# refuses when it can find nothing to verify with, so an image carrying neither
# minisign nor an openssl that does ed25519 and blake2b cannot reach a single
# branch this harness exists to exercise: it dies at the signature, one line
# into the download. That is install.sh being right, and it tests nothing.
#
# Which of the two goes in is per-manager on purpose, so the matrix covers more
# than one of install.sh's paths four times: minisign where it packages cleanly
# (alpine, nix), and openssl on debian/ubuntu, where it drives the fallback that
# hosts without minisign take. The third path, python3, is what VERIFIER=python
# below covers. openssl is named there even
# though it is already present — ca-certificates depends on it, so those legs
# were verifying through a package nothing asked for, and the day that
# dependency goes they fail on a missing signature checker with nothing in the
# diff to say why.
#
# The manager is discovered rather than passed in, so adding an image to the
# CI matrix needs no edit here as long as it uses one of these three.
if command -v apk >/dev/null 2>&1; then
    log "alpine: installing prerequisites with apk"
    apk add --no-cache bash curl ca-certificates tar findutils grep minisign >/dev/null
elif command -v apt-get >/dev/null 2>&1; then
    log "debian/ubuntu: installing prerequisites with apt-get"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null
    apt-get install -y -qq --no-install-recommends \
        bash curl ca-certificates tar findutils openssl >/dev/null
elif command -v nix-env >/dev/null 2>&1; then
    # curl is listed even though the base image has one: `nix-env -i` rebuilds
    # the user environment from the packages it can enumerate, and the curl the
    # image ships comes in through nix's own closure rather than as a profile
    # element, so installing anything at all drops it off PATH.
    log "nix: installing prerequisites with nix-env"
    nix-env -iA nixpkgs.gawk nixpkgs.gnused nixpkgs.curl nixpkgs.cacert \
        nixpkgs.minisign >/dev/null 2>&1 \
        || die "nix-env could not install the prerequisites"
else
    die "no supported package manager (apk / apt-get / nix-env) in this image"
fi

# --- python-only host (VERIFIER=python) ---------------------------------
#
# install.sh looks for three signature checkers in turn: minisign, an openssl
# that does ed25519 and blake2b, then python3. The third exists for macOS, and
# there is no macOS runner in this matrix, so this leg builds the same shape in
# a container: no minisign, an `openssl` earlier on PATH that answers the way
# Apple's LibreSSL does, and a python3. Nothing else about the install changes,
# which is the point — the release it verifies and the binary it lands are the
# ones every other leg gets.
if [ "$VERIFIER" = "python" ]; then
    log "leaving python3 as the only signature checker in this container"
    if command -v apk >/dev/null 2>&1; then
        apk add --no-cache python3 >/dev/null
    elif command -v apt-get >/dev/null 2>&1; then
        apt-get install -y -qq --no-install-recommends python3 >/dev/null
    elif command -v nix-env >/dev/null 2>&1; then
        nix-env -iA nixpkgs.python3 >/dev/null 2>&1 || die "nix-env could not install python3"
    fi

    found="$(command -v minisign 2>/dev/null || true)"
    [ -n "$found" ] && mv "$found" "${found}.hidden"

    # /usr/local/bin comes before /usr/bin on every image here, and install.sh
    # searches it as one of the Homebrew/MacPorts-shaped prefixes too, so this
    # is the openssl it finds by both routes.
    mkdir -p /usr/local/bin
    cat >/usr/local/bin/openssl <<'STUB'
#!/bin/sh
# Stands in for Apple's /usr/bin/openssl: LibreSSL, which has neither ed25519
# over raw bytes nor blake2b-512, and says so the way LibreSSL says it.
case "$1" in
    pkeyutl) echo "pkeyutl: Usage: pkeyutl [-in file] [-out file] [-verify]" >&2 ;;
    dgst) echo "dgst: Unknown message digest" >&2 ;;
esac
exit 1
STUB
    chmod +x /usr/local/bin/openssl

    # Assert the shape rather than assume it: an image that still has a working
    # minisign or openssl would pass this leg without ever running the code it
    # exists to cover.
    command -v minisign >/dev/null 2>&1 && die "minisign is still on PATH; this leg would test nothing"
    openssl dgst -blake2b512 </dev/null >/dev/null 2>&1 \
        && die "the openssl on PATH can still hash blake2b; this leg would test nothing"
    command -v python3 >/dev/null 2>&1 || die "no python3 in this image, so nothing here can verify a signature"
fi

# --- NixOS simulation ---------------------------------------------------
#
# There is no NixOS container image. `nixos/nix` is the Nix *package manager*
# on a minimal rootfs: it has neither /etc/NIXOS nor an /etc/os-release
# saying `ID=nixos`, so install.sh's is_nixos() answers no and the image
# tests the ordinary FHS path a second time instead of the NixOS one.
#
# /etc/NIXOS is what a real NixOS system's activation script writes, and it is
# the first thing is_nixos() looks for, so creating it makes the container an
# honest stand-in: everything downstream (the static-musl asset preference,
# the ~/.local/bin install dir, the nix-run banner) then runs exactly as it
# would on the real distro. This is the one thing the harness pretends about;
# the install itself is real.
if [ "${SIMULATE_NIXOS:-0}" = "1" ]; then
    log "marking this container as NixOS (/etc/NIXOS)"
    : >/etc/NIXOS
fi

# --- run the installer --------------------------------------------------

case "$FLAVOR" in
    default) log "running install.sh (default flavor: binary + loadout)" ;;
    minimal)
        log "running install.sh (WIZARD_MINIMAL=1: binary only)"
        WIZARD_MINIMAL=1
        export WIZARD_MINIMAL
        ;;
    *) die "unknown flavor '${FLAVOR}' (want: default | minimal)" ;;
esac

# WIZARD_WITH_TOOLCHAIN stays 0: pulling rustup into every container would
# add minutes to each matrix leg to test a branch that has nothing to do with
# the distro under test.
bash "$INSTALLER"

# --- verify -------------------------------------------------------------
#
# The installer's own sanity check runs the binary out of its temp dir before
# placing it. This checks the copy that survived: the one on disk at the path
# the installer chose, which on the NixOS branch is ~/.local/bin (not on PATH
# in a bare container, hence the fallback lookup).
bin="$(command -v wizard 2>/dev/null || true)"
if [ -z "$bin" ]; then
    for candidate in "$HOME/.local/bin/wizard" /usr/local/bin/wizard; do
        [ -x "$candidate" ] && bin="$candidate" && break
    done
fi
[ -n "$bin" ] || die "install.sh reported success but left no wizard binary on disk"

log "installed at ${bin}"
version="$("$bin" --version)" || die "${bin} does not run in this container"
log "wizard --version -> ${version}"

case "$version" in
    *wizard*) ;;
    *) die "unexpected --version output: ${version}" ;;
esac

# The default flavor promises a loadout; the minimal flavor promises the
# absence of one. Asserting both directions is what keeps this from being a
# test that only proves the tarball unpacked.
if [ "$FLAVOR" = "default" ]; then
    for f in "$HOME/.wizard/mcp.toml" \
        "$HOME/.wizard/subagents/reviewer.toml" \
        "$HOME/.wizard/subagents/researcher.toml" \
        "$HOME/.wizard/subagents/tester.toml" \
        "$HOME/.wizard/subagents/documenter.toml"; do
        [ -f "$f" ] || die "default install left no ${f}"
    done
    log "loadout present (mcp.toml + 4 subagents)"
else
    [ -e "$HOME/.wizard/mcp.toml" ] \
        && die "WIZARD_MINIMAL=1 still wrote the loadout to ~/.wizard/mcp.toml"
    log "minimal install wrote no loadout, as documented"
fi

log "OK"
