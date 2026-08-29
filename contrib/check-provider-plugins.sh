#!/usr/bin/env bash
# "Deleting any one plugin must leave a tree that compiles and passes tests."
#
# `contrib/check-plugin-work.sh` proves the two extremes: everything on, and
# everything off. Neither catches the interesting case, which is one plugin
# missing while the rest are present — a core module that reached into
# `provider-ollama` still compiles with `--no-default-features` because the
# module it reached into is gone too, and still compiles by default because it
# is there. It only fails in between.
#
# So this builds and tests each leave-one-out set, plus the all-off floor. It
# is the slow gate: N+1 feature sets is N+1 near-full rebuilds of the crate
# (dependency artifacts are shared). Run it when the plugin set or the boundary
# changes, not on every edit.
#
# Two legs here are load-bearing beyond their own plugin, and both are about the
# local backends:
#
#   - `without provider-llamacpp` is the only build with no `/server`
#     implementation in it. `src/server.rs` is a trait and a lookup, and the
#     three surfaces that dispatch `/server` have to degrade to a sentence
#     rather than to a compile error — which neither extreme can show, since
#     with everything off there is no `/server` caller worth compiling and with
#     everything on the lookup always answers. It is also the leg that proves
#     the GGUF downloader left core cleanly: it and the process manager moved
#     into this plugin together, and anything in core still reaching for either
#     fails only here.
#   - `without provider-ollama` proves the other half of that move. Ollama
#     reports a model pull through `crate::progress::Progress` and asks
#     `platform::host::local_port` whether a `base_url` is this machine's, and
#     both of those used to live in `src/server.rs`. If either had gone into the
#     llama.cpp plugin instead of into core, this leg would still pass and
#     `without provider-llamacpp` would be the one that broke — so the pair is
#     what pins the split, not either one alone.
#
# Usage: contrib/check-provider-plugins.sh [--build-only]
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

BUILD_ONLY=${1:-}

PLUGINS=(
    provider-anthropic
    provider-chatgpt
    provider-cloudflare
    provider-llamacpp
    provider-ollama
    provider-openai
    provider-xai
)

fail=0
results=()

# Disk on the machines this runs on is the real constraint: seven feature sets
# of `target/` is tens of gigabytes, and a build that dies on ENOSPC reports a
# test count of zero, which reads exactly like a passing-but-empty run. So the
# floor is checked before each leg and the crate's own artifacts (not its
# dependencies') are dropped when it gets close.
free_gb() { df -BG --output=avail / | tail -1 | tr -dc '0-9'; }

leg() {
    local label=$1
    shift
    printf '\n=== %s ===\n' "$label"
    if [ "$(free_gb)" -lt 8 ]; then
        printf 'only %sG free; cargo clean -p wizard\n' "$(free_gb)"
        cargo clean -p wizard
    fi
    if ! cargo build --locked --no-default-features "$@"; then
        results+=("BUILD FAILED  $label")
        fail=1
        return
    fi
    if [ "$BUILD_ONLY" = "--build-only" ]; then
        results+=("built         $label")
        return
    fi
    local log
    log=$(mktemp)
    cargo test --locked --no-default-features --no-fail-fast "$@" 2>&1 | tee "$log" \
        | grep -E '^test result' >/dev/null
    local passed failed
    passed=$(grep -oE '[0-9]+ passed' "$log" | grep -oE '^[0-9]+' | awk '{n += $1} END {print n + 0}')
    failed=$(grep -oE '[0-9]+ failed' "$log" | grep -oE '^[0-9]+' | awk '{n += $1} END {print n + 0}')
    # A leg that reported nothing is not a pass; it is a truncated log.
    if [ "${passed:-0}" -lt 1 ]; then
        results+=("NO RESULTS    $label")
        fail=1
    elif [ "${failed:-1}" -ne 0 ]; then
        if [ "$failed" -eq 1 ] \
           && grep -qE '^[[:space:]]+platform::lockfile::tests::a_second_holder_waits_and_gets_the_lock_once_the_first_drops_it$' "$log"; then
            results+=("$(printf '%-5s passed, known lockfile flake  %s' "$passed" "$label")")
        else
            results+=("$(printf '%-5s passed, %s FAILED  %s' "$passed" "$failed" "$label")")
            fail=1
        fi
    else
        results+=("$(printf '%-5s passed, 0 failed  %s' "$passed" "$label")")
    fi
    rm -f "$log"
}

for missing in "${PLUGINS[@]}"; do
    set=()
    for p in "${PLUGINS[@]}"; do
        [ "$p" = "$missing" ] || set+=("$p")
    done
    joined=$(IFS=,; echo "${set[*]}")
    leg "without $missing" --features "$joined"
done

leg "no providers at all"

printf '\n=== removability matrix ===\n'
for line in "${results[@]}"; do
    printf '  %s\n' "$line"
done

if [ "$fail" -ne 0 ]; then
    printf '\nFAILED: a plugin whose removal breaks the build is not a plugin\n' >&2
    exit 1
fi
printf '\nevery provider plugin is independently removable\n'
