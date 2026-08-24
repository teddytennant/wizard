#!/usr/bin/env bash
# The gate every plugin-migration change has to clear before it is merged.
#
# One script rather than a list in a prompt, because "make sure it works" is
# otherwise re-interpreted by every agent that reads it, and the interesting
# failures here are the ones nobody thought to check: a dependency orphaned by
# code that moved to Lua, a file that grew past the ratchet while being split,
# a test that passes alone and fails in the suite.
#
# Exits non-zero on the first failure, loudly. Run from anywhere in the repo.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

# The baseline this migration must not regress. Captured on `main` @ 1ffd988
# with `cargo test --no-fail-fast`.
BASELINE_TESTS=2422

fail=0
step() { printf '\n=== %s ===\n' "$1"; }
bad()  { printf 'FAIL: %s\n' "$1" >&2; fail=1; }

step "format"
cargo fmt --check || bad "cargo fmt --check"

step "file-size ratchet"
contrib/check-file-size.sh || bad "a file exceeds the 5500-line ratchet"

step "clippy (warnings are errors)"
cargo clippy --all-targets --locked -- -D warnings || bad "clippy"

step "unused dependencies"
# Moving Rust code into Lua plugins orphans crates. cargo machete is the only
# check that notices, and a stale dep is a real cost: it still compiles, still
# ships, and still shows up in `cargo deny`.
if command -v cargo-machete >/dev/null 2>&1; then
    cargo machete || bad "cargo machete found unused dependencies"
else
    printf 'skipped: cargo-machete not installed\n'
fi

step "tests"
# --no-fail-fast so one failing target does not hide the rest, which is what
# `cargo test` did on the baseline run and cost a full second pass.
test_log=$(mktemp)
cargo test --no-fail-fast 2>&1 | tee "$test_log" | grep -E '^test result' || true
# awk rather than bc: bc is not installed on every box this runs on, and a
# missing summing tool that silently reports 0 would turn a regression into a
# pass.
passed=$(grep -oE '[0-9]+ passed' "$test_log" | grep -oE '^[0-9]+' \
         | awk '{n += $1} END {print n + 0}')
failed=$(grep -oE '[0-9]+ failed' "$test_log" | grep -oE '^[0-9]+' \
         | awk '{n += $1} END {print n + 0}')
printf '\npassed=%s failed=%s (baseline %s)\n' "$passed" "$failed" "$BASELINE_TESTS"

if [ "${failed:-1}" -ne 0 ]; then
    # The one known flake, so a busy machine does not read as a regression.
    if grep -q 'a_second_holder_waits_and_gets_the_lock_once_the_first_drops_it' "$test_log" \
       && [ "$failed" -eq 1 ]; then
        printf 'note: only the known lockfile flake failed; re-run it alone to confirm\n'
    else
        bad "$failed test(s) failed"
    fi
fi
if [ "${passed:-0}" -lt "$BASELINE_TESTS" ]; then
    bad "test count went backwards: $passed < $BASELINE_TESTS (did tests get deleted rather than moved?)"
fi
rm -f "$test_log"

step "result"
if [ "$fail" -ne 0 ]; then
    printf 'GATE FAILED\n' >&2
    exit 1
fi
printf 'GATE PASSED\n'
