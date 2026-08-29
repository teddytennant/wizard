#!/usr/bin/env bash
# What the plugin architecture cost, measured rather than asserted.
#
# Three questions a reader of `docs/plugins.md` is entitled to ask, and the
# answers this script produces:
#
#   1. Does a build you can actually strip get smaller, or is the feature list
#      decorative? -- `size`, which builds every named profile and weighs it.
#   2. Does starting Wizard cost more now that a kernel loads plugins before
#      the first prompt? -- `start`, cold-cache process wall clock.
#   3. Does a tool written in Lua answer as fast as the Rust it replaced?
#      -- `call`, the one number that decides how much more can move.
#
# Deliberately not a criterion benchmark: these are end-to-end, on the real
# binary, in the shape a user meets them. A microbenchmark of the Lua bridge
# would flatter it by leaving out process start, plugin load and JSON.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

RUNS=${RUNS:-10}
OUT=${OUT:-bench-results.txt}
: > "$OUT"

say() { printf '%s\n' "$*" | tee -a "$OUT"; }

# Median is the honest statistic here: a cold page-cache outlier on the first
# run is real but not representative, and a mean lets one 3-second stall
# rewrite a 40ms number.
median() { sort -n | awk '{v[NR]=$1} END {print (NR%2) ? v[(NR+1)/2] : (v[NR/2]+v[NR/2+1])/2}'; }

time_ms() {
    local start end
    start=$(date +%s%N)
    "$@" >/dev/null 2>&1
    end=$(date +%s%N)
    echo $(( (end - start) / 1000000 ))
}

# Portable `stat`: GNU takes -c, BSD and macOS take -f.
file_size() { stat -c%s "$1" 2>/dev/null || stat -f%z "$1"; }

# Binary size per named profile, with the delta against the stock build.
#
# The four sets this used to build were invented here and matched nothing a user
# could ask for: "no-mesh" was a hand-picked feature list with the mesh in its
# name and `graph` left in, which turns the mesh back on. The profiles are real
# now (`src/plugins/profile.rs`), so the list is read off a *binary* rather than
# restated in this file — build the default one, ask it what the others are,
# then build each. A profile added to that table is measured the next time this
# runs, and there is no copy here to go stale.
#
# python3 rather than jq because contrib already needs it
# (`contrib/check-registry.py`) and jq is not on every box this runs on.
#
# Each row is verified end to end rather than on trust: after building, the
# binary is asked which profile it thinks it is, and a name that comes back
# wrong means the flags this script passed and the feature set that came out are
# not the same thing. Without that check every row still prints a plausible
# number, which is the one failure a size table cannot survive.
bench_size() {
    say "=== binary size by profile (release; [profile.release] sets strip = true) ==="

    if ! cargo build --release --locked >/dev/null 2>&1; then
        say "the stock build failed, so there is nothing to enumerate profiles with"
        return 1
    fi
    local table
    table=$(./target/release/wizard plugin profiles --json | python3 -c 'import json,sys
for p in json.load(sys.stdin)["profiles"]: print(p["name"]+"\t"+" ".join(p["cargo_flags"]))')
    if [ -z "$table" ]; then
        say "could not read the profile table out of the binary"
        return 1
    fi

    local baseline=0 name flags bytes reported delta mb
    while IFS=$'\t' read -r name flags; do
        [ -n "$name" ] || continue
        # shellcheck disable=SC2086
        if ! cargo build --release --locked $flags >/dev/null 2>&1; then
            say "$(printf '%-9s %s' "$name" 'BUILD FAILED')"
            continue
        fi
        bytes=$(file_size target/release/wizard)
        reported=$(./target/release/wizard plugin profiles --json \
            | python3 -c 'import json,sys; print(json.load(sys.stdin)["active"] or "custom")')
        if [ "$reported" != "$name" ]; then
            say "$(printf '%-9s %12s  built as `%s` -- MISMATCH' "$name" "$bytes" "$reported")"
            continue
        fi
        [ "$name" = "default" ] && baseline=$bytes
        if [ "$baseline" -gt 0 ] && [ "$name" != "default" ]; then
            delta=$(awk -v b="$bytes" -v d="$baseline" \
                'BEGIN {printf "%+.1f%%", (b - d) * 100 / d}')
        else
            delta="--"
        fi
        mb=$(awk -v b="$bytes" 'BEGIN {printf "%.2f", b / 1048576}')
        say "$(printf '%-9s %12s bytes  %8s MB  %8s' "$name" "$bytes" "$mb" "$delta")"
    done <<<"$table"

    # Put the stock build back. `size` run on its own would otherwise leave
    # whichever profile sorted last sitting in target/release/wizard, which is
    # not the binary the caller had before.
    cargo build --release --locked >/dev/null 2>&1 || true
}

bench_start() {
    say ""
    say "=== cold start, $RUNS runs, median ms ==="
    cargo build --release --locked >/dev/null 2>&1 || { say "build failed"; return; }
    local b=target/release/wizard
    for cmd in "--version" "--help"; do
        local samples=()
        for _ in $(seq "$RUNS"); do samples+=("$(time_ms "$b" $cmd)"); done
        say "$(printf '%-12s %s ms' "$cmd" "$(printf '%s\n' "${samples[@]}" | median)")"
    done
}

bench_call() {
    say ""
    say "=== tool call latency, $RUNS runs, median ms ==="
    say "(a Lua-implemented tool against a Rust-implemented one, same binary,"
    say " same harness, so the difference is the bridge and nothing else)"
    cargo build --release --locked >/dev/null 2>&1 || { say "build failed"; return; }
    local b=target/release/wizard
    # `harness call` runs one tool and exits, which is the only way to time a
    # single call without a model in the loop.
    for tool in git_status read_file; do
        local samples=()
        for _ in $(seq "$RUNS"); do
            samples+=("$(time_ms "$b" harness call "$tool" '{}')")
        done
        say "$(printf '%-12s %s ms' "$tool" "$(printf '%s\n' "${samples[@]}" | median)")"
    done
}

case "${1:-all}" in
    size)  bench_size ;;
    start) bench_start ;;
    call)  bench_call ;;
    all)   bench_size; bench_start; bench_call ;;
    *)     echo "usage: $0 [size|start|call|all]" >&2; exit 2 ;;
esac

say ""
say "results in $OUT"
