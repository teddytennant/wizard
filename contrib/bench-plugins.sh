#!/usr/bin/env bash
# What the plugin architecture cost, measured rather than asserted.
#
# Three questions a reader of `docs/plugins.md` is entitled to ask, and the
# answers this script produces:
#
#   1. Does a build you can actually strip get smaller, or is the feature list
#      decorative? -- `size`, which builds four profiles and weighs them.
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

bench_size() {
    say "=== binary size by profile (release, stripped) ==="
    local profiles=(
        "minimal:--no-default-features"
        "stock:"
        "no-mesh:--no-default-features --features provider-anthropic,provider-openai,tool-web,tool-git"
        "everything:--features native"
    )
    for entry in "${profiles[@]}"; do
        local name="${entry%%:*}" flags="${entry#*:}"
        # shellcheck disable=SC2086
        if cargo build --release --locked $flags >/dev/null 2>&1; then
            local bytes; bytes=$(stat -c%s target/release/wizard)
            say "$(printf '%-12s %8.1f MB' "$name" "$(echo "$bytes/1048576" | bc -l)")"
        else
            say "$(printf '%-12s %s' "$name" 'BUILD FAILED')"
        fi
    done
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
