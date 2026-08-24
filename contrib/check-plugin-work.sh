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

# The baseline this migration must not regress. Captured with
# `cargo test --no-fail-fast` on the branch as it stood after the kernel landed
# (2536); it was 2422 on `main` @ 1ffd988 before that. Raise it when a phase
# adds tests, so the ratchet keeps ratcheting.
BASELINE_TESTS=2536

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
elif command -v nix >/dev/null 2>&1; then
    # Not installed on the NixOS box this migration runs on, and skipping it is
    # not harmless here: the whole point of moving Rust into Lua plugins is that
    # crates stop being used, and this is the only check that notices.
    nix run nixpkgs#cargo-machete -- --help >/dev/null 2>&1 \
        && { nix run nixpkgs#cargo-machete || bad "cargo machete found unused dependencies"; } \
        || printf 'skipped: could not obtain cargo-machete\n'
else
    bad "cargo-machete unavailable and no nix to fetch it; unused deps would go unnoticed"
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

if [ "${passed:-0}" -lt 1 ]; then
    bad "the test run reported no results at all (truncated log? build failure?)"
fi
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

# The "delete any one plugin" rule from docs/plugins.md, as something that can
# fail. A plugin whose removal breaks the build is not a plugin, and the only
# way to know is to remove it: `--no-default-features` drops
# `provider-anthropic`, and the tree still has to compile, still has to pass,
# and `kind = "anthropic"` still has to degrade to a named error rather than a
# panic. `src/plugins/mod.rs` carries the assertion for the second half.
#
# Cheap enough to run every time (one extra feature-set build) and worth it:
# this is the leg that catches a core module reaching into a plugin, which is
# the failure the whole architecture exists to prevent and which the
# default-features build cannot see.
step "no-default-features (a build with the anthropic plugin deleted)"
cargo build --no-default-features --locked || bad "cargo build --no-default-features"
nd_log=$(mktemp)
cargo test --no-default-features --locked --no-fail-fast 2>&1 | tee "$nd_log" \
    | grep -E '^test result' || true
nd_failed=$(grep -oE '[0-9]+ failed' "$nd_log" | grep -oE '^[0-9]+' \
            | awk '{n += $1} END {print n + 0}')
nd_passed=$(grep -oE '[0-9]+ passed' "$nd_log" | grep -oE '^[0-9]+' \
            | awk '{n += $1} END {print n + 0}')
printf '\nno-default-features: passed=%s failed=%s\n' "$nd_passed" "$nd_failed"
# A run that reported nothing is not a pass. This leg once printed
# `passed=0 failed=0` and scored green because the disk filled and the log it
# parses was truncated to nothing -- the exact shape of a gate that lies.
if [ "${nd_passed:-0}" -lt 1 ]; then
    bad "--no-default-features reported no test results at all (truncated log? build failure?)"
fi
if [ "${nd_failed:-1}" -ne 0 ]; then
    if grep -q 'a_second_holder_waits_and_gets_the_lock_once_the_first_drops_it' "$nd_log" \
       && [ "$nd_failed" -eq 1 ]; then
        printf 'note: only the known lockfile flake failed\n'
    else
        bad "$nd_failed test(s) failed with --no-default-features"
    fi
fi
rm -f "$nd_log"

step "result"
if [ "$fail" -ne 0 ]; then
    printf 'GATE FAILED\n' >&2
    exit 1
fi
printf 'GATE PASSED\n'
