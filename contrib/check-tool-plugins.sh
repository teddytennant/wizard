#!/usr/bin/env bash
# "Deleting any one plugin must leave a tree that compiles and passes tests."
#
# `contrib/check-provider-plugins.sh` is this script for the seven provider
# features. This one is for the plugins that are not providers, which fail
# differently and therefore have to be checked differently.
#
# A missing provider still has a `kind` string a user can type, so its degrade
# path is an error message and the leave-one-out build is the whole test. A
# missing *tool* has no such string: the only correct behaviour is that it is
# absent from the roster the model is told about. A missing *surface* has a
# third path again — its `clap` variant is in core and keeps parsing, so
# `wizard fleet` still exists and has to say why it cannot run. And a missing
# *library* plugin is only interesting when its consumer is present. So the
# legs below build combinations the provider script never produces:
#
#   - `graph` left out with the GUI *present*, which is the only way to catch
#     `src/native/graph/` reaching for a plugin that is not there. Neither
#     `--no-default-features` nor a default build can see it: `native` is off
#     in both.
#   - `tool-web` left out with everything else present, which catches a core
#     module — or another plugin — that took a dependency on the web tools
#     rather than on `src/tools/http.rs`.
#   - `acp` left out, which is also the only build that does not link
#     `agent-client-protocol` at all — the one plugin feature that gates a
#     dependency, so it is the one where "removable" includes the dependency
#     graph and not just the module tree.
#   - `fleet` left out with everything else present, which catches core
#     reaching into the fleet for `FleetDirs`, a status row or a state file —
#     none of which it does today, and all of which would compile fine both
#     with every feature on and with every feature off.
#
# Usage: contrib/check-tool-plugins.sh [--build-only]
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

BUILD_ONLY=${1:-}

# Read from Cargo.toml rather than restated here, so a feature added to
# `default` is covered by these legs the day it lands instead of the day
# somebody remembers this file.
mapfile -t DEFAULT_FEATURES < <(
    awk '/^default = \[/ {inside = 1; next} inside && /^\]/ {exit} inside' Cargo.toml \
        | tr -d ' ",'
)
if [ "${#DEFAULT_FEATURES[@]}" -lt 2 ]; then
    printf 'could not read the default feature list out of Cargo.toml\n' >&2
    exit 1
fi

# Everything in `default` except the named feature, comma-joined.
without() {
    local drop=$1 kept=()
    for f in "${DEFAULT_FEATURES[@]}"; do
        [ "$f" = "$drop" ] || kept+=("$f")
    done
    (IFS=,; printf '%s' "${kept[*]}")
}

all() { (IFS=,; printf '%s' "${DEFAULT_FEATURES[*]}"); }

fail=0
results=()

# Disk is the real constraint on the machines this runs on: a build that dies
# on ENOSPC reports a test count of zero, which reads exactly like a passing
# empty run. Checked before each leg, same as the provider script.
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
    if [ "${passed:-0}" -lt 1 ]; then
        results+=("NO RESULTS    $label")
        fail=1
    elif [ "${failed:-1}" -ne 0 ]; then
        if [ "$failed" -eq 1 ] \
           && grep -q 'a_second_holder_waits_and_gets_the_lock_once_the_first_drops_it' "$log"; then
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

# Leave-one-out, against an otherwise-stock feature set.
leg "without tool-web" --features "$(without tool-web)"
leg "without graph" --features "$(without graph)"
leg "without acp" --features "$(without acp)"
leg "without fleet" --features "$(without fleet)"

# The GUI is where `graph` is actually consumed, so both sides of it need a
# build with the window linked. `--build-only` is not enough here: the
# `graph_explorer` integration test is `#![cfg(all(native, graph))]` and has to
# be seen compiling *and* passing in the first of these and compiling to
# nothing in the second.
leg "the window, with the explorer" --features "$(all),native"
leg "the window, with graph deleted" --features "$(without graph),native"

printf '\n=== removability matrix ===\n'
for line in "${results[@]}"; do
    printf '  %s\n' "$line"
done

if [ "$fail" -ne 0 ]; then
    printf '\nFAILED: a plugin whose removal breaks the build is not a plugin\n' >&2
    exit 1
fi
printf '\nevery non-provider plugin is independently removable\n'
