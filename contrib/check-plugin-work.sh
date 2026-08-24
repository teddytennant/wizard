#!/bin/sh
# The full gate for work on the core/plugin split, in one command.
#
# Same four checks CI runs, in the order that fails cheapest first: format,
# then lint, then the file-size ratchet, then the tests. Run from anywhere
# inside the repo.
#
# Known flake, not a regression:
# `platform::lockfile::tests::a_second_holder_waits_and_gets_the_lock_once_the_first_drops_it`
# can lose its race under parallel load. Re-run it alone before believing it.
set -eu

cd "$(git rev-parse --show-toplevel)"

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --all-targets --locked -- -D warnings"
cargo clippy --all-targets --locked -- -D warnings

echo "==> contrib/check-file-size.sh"
sh contrib/check-file-size.sh

echo "==> cargo test --no-fail-fast"
cargo test --no-fail-fast
