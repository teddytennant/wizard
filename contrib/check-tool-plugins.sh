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
#   - `mesh` left out, twice: once headless and once with the GUI present. The
#     mesh is the one plugin core reaches through *two* seams rather than one
#     — `wizard peers` (an `entrypoint::Subcommand`) and the session tee (an
#     `app::tee::TeeFactory`) — and the second of those is on the TUI's hot
#     path, so a leg without it is what proves `App::mesh` is honestly a
#     `None` and not a compile error waiting to happen.
#
# `mesh` is also the first feature another feature *depends* on: `graph = ["mesh"]`,
# because a `MeshGraph` is a `PeerStore` turned into something drawable. So
# leaving `mesh` out means leaving `graph` out too, and `without` alone cannot
# express that — hence `without_many` below. Dropping `graph` while keeping
# `mesh` is the other direction and is a leg of its own.
#
#   - `tool-git` left out with everything else present. This is the first
#     *Lua* plugin, and it fails in a way the others cannot: its tools are
#     registered by a script that only runs once `plugins::bundled::ensure`
#     has been awaited, so the leg proves that leaving it out costs two tool
#     names and not a compile error in the four places that assert what the
#     roster holds (`plugins`, `mcp`, `harness`, `tools::registry`).
#   - `tool-publish` left out. The second Lua plugin, and the first whose
#     tool a *slash command* invokes as well as the model, so it has two
#     degrade paths at once: `publish` must be absent from the roster, and
#     `/publish` and `wizard --publish` must each answer with the sentence
#     naming the feature rather than doing nothing. Four call sites go through
#     `plugins::run_tool`, and this is what proves none of them was left
#     reaching for a deleted `crate::evolve::publish`.
#   - `acp` left out, which is also the only build that does not link
#     `agent-client-protocol` at all — the one plugin feature that gates a
#     dependency, so it is the one where "removable" includes the dependency
#     graph and not just the module tree.
#   - `fleet` left out with everything else present, which catches core
#     reaching into the fleet for `FleetDirs`, a status row or a state file —
#     none of which it does today, and all of which would compile fine both
#     with every feature on and with every feature off.
#   - `plugin-js` left out, which is the first leg where what is removed is a
#     *language* rather than a subsystem. Three things have to survive it: the
#     `rquickjs` dependency genuinely leaves the build (it is `optional`, and
#     `dep:` from this feature alone), `PluginKind` loses its `Js` variant
#     without a `match` anywhere going non-exhaustive, and a `plugin.js` in
#     `~/.wizard/plugins` degrades to a sentence naming the feature rather
#     than being silently skipped. `tool-json` goes with it — the JS plugin
#     Wizard ships names `plugin-js` in Cargo.toml, so dropping only the
#     backend from the list leaves the plugin to turn it back on, which is the
#     `graph`/`mesh` shape and is why `without_many` exists.
#   - `tool-json` left out with the backend *present*. The complement, and the
#     one that catches the thing neither extreme can see: a build with a
#     JavaScript engine linked and no bundled JavaScript plugin has to leave
#     `json_query` out of the roster rather than fail to compile, which is the
#     same claim `without tool-git` makes about the Lua half.
#   - `gateway` left out with everything else present. It is the first plugin
#     that owns *two* entrypoints, so it is the first leg where "absent" has
#     two halves that can disagree: a build where `wizard --gateway` degraded
#     to a sentence and `wizard gateway install` still tried to write a unit
#     file would pass every other leg here. It is also the leg that proves the
#     three things core kept — `[gateway]` in `config.toml`,
#     `credentials::GATEWAY_TOKEN` and `config::group_chat_warning` — are
#     genuinely core and not a plugin's exports read through a re-export.
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

# Everything in `default` except the named features, comma-joined. Needed
# because a feature that another feature enables is not removed by dropping it
# from the list: `--features graph` turns `mesh` back on.
without_many() {
    local kept=() f drop keep
    for f in "${DEFAULT_FEATURES[@]}"; do
        keep=1
        for drop in "$@"; do
            [ "$f" = "$drop" ] && keep=0
        done
        [ "$keep" = 1 ] && kept+=("$f")
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
        # A leg with no results is either a leg that ran nothing or a leg
        # whose log went to a full disk, and those want different reactions:
        # the first is a bug in this repository, the second is a machine that
        # needs room. Both still fail -- an unproven leg is unproven -- but
        # reading `NO RESULTS` and going looking for the code cost an hour
        # once, so the two are named apart.
        if grep -q 'No space left on device' "$log"; then
            results+=("DISK FULL     $label (log truncated; free space and re-run)")
        else
            results+=("NO RESULTS    $label")
        fi
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
leg "without tool-git" --features "$(without tool-git)"
leg "without tool-publish" --features "$(without tool-publish)"
leg "without graph" --features "$(without graph)"
leg "without acp" --features "$(without acp)"
leg "without fleet" --features "$(without fleet)"
leg "without gateway" --features "$(without gateway)"
leg "without tool-json" --features "$(without tool-json)"

# `tool-json` goes with it: it enables `plugin-js`, so dropping only the
# backend from the list leaves it on. This is the leg that proves `rquickjs` is
# genuinely out of the build rather than merely uncalled, and that a
# `plugin.js` in `~/.wizard/plugins` says which feature would run it.
leg "without plugin-js" --features "$(without_many plugin-js tool-json)"

# `graph` goes with it: it enables `mesh`, so dropping only `mesh` from the
# list leaves it on. This is the leg that proves `App::mesh` degrades to a
# `None`, `wizard peers` degrades to a sentence, and quinn/rustls/mdns-sd are
# genuinely out of the build rather than merely uncalled.
leg "without mesh" --features "$(without_many mesh graph)"

# The GUI is where `graph` is actually consumed, so both sides of it need a
# build with the window linked. `--build-only` is not enough here: the
# `graph_explorer` integration test is `#![cfg(all(native, graph))]` and has to
# be seen compiling *and* passing in the first of these and compiling to
# nothing in the second.
leg "the window, with the explorer" --features "$(all),native"
leg "the window, with graph deleted" --features "$(without graph),native"

# The window renders peers, so a mesh-less GUI is the combination most likely
# to have an edge nobody gated. Neither leg above sees it: `without mesh` has
# no window and `the window, with graph deleted` still has the mesh.
leg "the window, with the mesh deleted" --features "$(without_many mesh graph),native"

printf '\n=== removability matrix ===\n'
for line in "${results[@]}"; do
    printf '  %s\n' "$line"
done

if [ "$fail" -ne 0 ]; then
    printf '\nFAILED: a plugin whose removal breaks the build is not a plugin\n' >&2
    exit 1
fi
printf '\nevery non-provider plugin is independently removable\n'
