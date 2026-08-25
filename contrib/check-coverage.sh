#!/bin/sh
# Coverage ratchet: measures line coverage with cargo-llvm-cov and fails if it
# has fallen below the floor, or if the floor has never been seeded. The mirror
# image of contrib/check-file-size.sh: that number only ever moves down, this
# one only ever moves up. Runs from any directory inside the repo.
#
# Extra arguments are forwarded to cargo-llvm-cov, so a local run can narrow
# the measurement:  contrib/check-coverage.sh --lib
#
# Three environment variables, all documented where they are read below:
# MIN_LINE_PERCENT can only RAISE the floor this file carries, COVERAGE_REPORT
# reuses a summary that has already been measured instead of paying for another
# instrumented build, and COVERAGE_SUMMARY_OUT keeps a copy of the summary this
# run measured so CI can publish it.
set -eu

# ---------------------------------------------------------------------------
# THE ONE LINE TO EDIT. The floor, as a whole percent.
DEFAULT_MIN_LINE_PERCENT=79
# ---------------------------------------------------------------------------
#
# Seeded 2026-08-02 from a `coverage` run on the v2 branch, which measured
# 79.60% (56658/71176 lines). The floor is set a point and a half DOWN from
# that on purpose: instrumented line counts move a little between runs, and a
# floor set flush against the measurement flaps red on commits that changed
# nothing, which is how ratchets get deleted.
#
# Raised to 79 on 2026-08-07, measured twice on v2 by this script under
# `nix shell nixpkgs#cargo-llvm-cov`: 81.25% (65163/80205 lines), then 81.28%
# (65318/80362) after the seller-price work landed. Roughly 42,000 lines
# landed since the seed and the ratio rose, so the floor follows them up.
# Those two runs differ by 0.03 points, so the flap this comment worried
# about is real but small; the gap is still left at two and a quarter points
# rather than the point and a half above, because two local runs a few
# minutes apart measure the same machine and not the CI matrix. Tighten it
# once CI has measured this branch a few times.
#
# Measured with DEFAULT features only, which is the whole point of the number:
# `src/plugins/native/` sits behind an off-by-default flag and is absent from both
# sides of the ratio, so an unmeasured GUI can neither prop the figure up nor
# drag it down.
#
# A zero here means UNSEEDED and makes this script exit 1 on every run. That
# path is still live, and `coverage-selfcheck` in CI is what proves it: a
# coverage check that cannot fail is a report wearing a gate's name, and the
# only thing worse than no gate is a green one that never had a chance of
# going red.
#
# From here: only ever RAISE this number, after adding the tests that earn it;
# never lower it to let a coverage regression through. It is a floor, not a
# target.
#
# The floor comes from that line and from nowhere else. MIN_LINE_PERCENT in the
# environment may only RAISE it: against a floor of 60, `MIN_LINE_PERCENT=80`
# is a stricter local run and is honoured, while `MIN_LINE_PERCENT=1` is
# refused outright rather than quietly lowering the gate. The unseeded test at
# the bottom reads the file's value and never the raised one, so no environment
# can talk this script out of failing while the line above says 0 either.
#
# A ratchet a caller can lower from the environment is not a ratchet. The shape
# that has to stay impossible is one `env:` block added to the CI step in a
# hurry, turning a gate that was failing for a reason green while the run
# summary still reads like a seeded floor. Nobody ever comes back to it.
#
# The self-check job in .github/workflows/ci.yml exercises every branch of this
# against copies of this file with the floor seeded to a known number, so it
# tests the logic rather than whatever the line above happens to say today.

die() {
    echo "error: $*" >&2
    exit 1
}

# A whole or fractional percent, checked as text rather than through awk,
# because awk turns any word into 0 and 0 is the one value that means
# "unseeded". A typo'd floor must not read as an honest open floor. A leading
# `-` fails the same test: the `-` is not in the character class.
is_percentage() {
    case "$1" in
        '' | *[!0-9.]* | *.*.*) return 1 ;;
    esac
    return 0
}

is_percentage "$DEFAULT_MIN_LINE_PERCENT" \
    || die "DEFAULT_MIN_LINE_PERCENT=$DEFAULT_MIN_LINE_PERCENT in this script is not a percentage"

# Checked before anything expensive runs: an override that cannot be honoured
# is a caller mistake, and the reader deserves to hear about it before a
# ten-minute instrumented build rather than after one.
if [ -n "${MIN_LINE_PERCENT:-}" ]; then
    is_percentage "$MIN_LINE_PERCENT" \
        || die "MIN_LINE_PERCENT=$MIN_LINE_PERCENT is not a percentage"
    if awk -v m="$MIN_LINE_PERCENT" -v d="$DEFAULT_MIN_LINE_PERCENT" \
        'BEGIN { exit !(m + 0 < d + 0) }'; then
        die "MIN_LINE_PERCENT=$MIN_LINE_PERCENT is below this file's floor of ${DEFAULT_MIN_LINE_PERCENT}%; the override can only raise it"
    fi
else
    MIN_LINE_PERCENT="$DEFAULT_MIN_LINE_PERCENT"
fi

# Report a line to the operator, and to the run summary when there is one.
# GITHUB_STEP_SUMMARY is set only inside a GitHub Actions step; locally stderr
# is the whole report. Anything this script wants a human to actually see goes
# through here rather than to stderr alone, where a green job buries it.
note() {
    echo "$*" >&2
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        echo "$*" >>"$GITHUB_STEP_SUMMARY"
    fi
}

command -v jq >/dev/null 2>&1 \
    || die "jq is not installed (needed to read the coverage summary)"

# An already-measured summary, in cargo-llvm-cov's `--json --summary-only`
# shape. Resolved to an absolute path before the `cd` below, so a relative
# path means what the caller typed it against. Two uses: re-checking a
# measurement CI already paid for, and exercising the comparison at the bottom
# of this file (which is otherwise reachable only after a full instrumented
# build of the whole crate).
report="${COVERAGE_REPORT:-}"
if [ -n "$report" ]; then
    case "$report" in
        /*) ;;
        *) report="$PWD/$report" ;;
    esac
    [ -f "$report" ] || die "COVERAGE_REPORT=$report is not a file"
fi

cd "$(git rev-parse --show-toplevel)"

if [ -z "$report" ]; then
    command -v cargo-llvm-cov >/dev/null 2>&1 \
        || die "cargo-llvm-cov is not installed (cargo install cargo-llvm-cov, or use taiki-e/install-action in CI)"

    report="$(mktemp)"
    trap 'rm -f "$report"' EXIT

    # --summary-only keeps the per-file table out of the JSON: the ratchet needs
    # one number and the file is otherwise tens of megabytes. --locked matches the
    # rest of CI so a Cargo.lock drift fails here too instead of silently
    # re-resolving and measuring a different dependency tree.
    cargo llvm-cov --locked --summary-only --json --output-path "$report" "$@"

    # Keep the measurement, when the caller asked for it. The report is
    # otherwise a mktemp that this script deletes on the way out, so the only
    # trace a CI run leaves of a ten-minute instrumented build is one line of
    # prose — and "coverage is measured, published and ratcheted" wants the
    # numbers themselves to survive the run that produced them. CI uploads
    # this file as a build artifact; see the `coverage` job.
    #
    # Deliberately a different variable from COVERAGE_REPORT, which means the
    # opposite (read a summary somebody else measured). One name doing both
    # would make "reuse this" and "overwrite this" the same spelling.
    if [ -n "${COVERAGE_SUMMARY_OUT:-}" ]; then
        cp "$report" "$COVERAGE_SUMMARY_OUT" \
            || die "could not write the coverage summary to $COVERAGE_SUMMARY_OUT"
    fi
else
    echo "note: reusing the coverage summary at $report; nothing was measured." >&2
fi

percent="$(jq -r '.data[0].totals.lines.percent' "$report")"
covered="$(jq -r '.data[0].totals.lines.covered' "$report")"
total="$(jq -r '.data[0].totals.lines.count' "$report")"

case "$percent" in
    '' | null) die "could not read line coverage out of the cargo-llvm-cov summary" ;;
esac

# Both numbers are fractional, and POSIX sh only compares integers.
summary="$(
    awk -v p="$percent" -v c="$covered" -v t="$total" -v m="$MIN_LINE_PERCENT" \
        'BEGIN { printf "line coverage: %.2f%% (%d/%d lines), floor %s%%", p, c, t, m }'
)"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    echo "### Coverage" >>"$GITHUB_STEP_SUMMARY"
fi
note "$summary"

# Publish the number where it can be read without opening the job log.
#
# The step summary above is one page deep inside one run; an annotation shows
# on the run's own header and in the Checks tab, and the step outputs let a
# later step (a badge, a comment, a release note) use the figure rather than
# re-measure it. Same measurement, three places, no extra build.
#
# Gated on GITHUB_STEP_SUMMARY because that is this file's existing "we are
# publishing" signal, and the ratchet's self-check job blanks it precisely so
# its fixture's invented numbers cannot be mistaken for a measurement. Without
# the gate the self-check would publish 62.3% six times per run.
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    echo "::notice title=Coverage::$summary"
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
        {
            echo "line_percent=$percent"
            echo "lines_covered=$covered"
            echo "lines_total=$total"
            echo "line_floor=$MIN_LINE_PERCENT"
        } >>"$GITHUB_OUTPUT"
    fi
fi

# An unseeded floor is a failure, not a note. This is the last thing that runs
# rather than the first, because the measurement above is the whole point: it
# is what the failure hands the reader to close the gap with. Compared through
# awk so both "0" and "0.0" count as unseeded; only a positive floor can gate
# anything. (A negative never reaches here: `is_percentage` rejected the `-`
# before the measurement, with its own message.)
#
# It reads DEFAULT_MIN_LINE_PERCENT, the value in this file, and not the
# possibly-raised MIN_LINE_PERCENT: an unseeded floor means nobody has measured
# this tree yet, which is a fact about the file, and an environment variable
# must not be able to answer it. Otherwise `MIN_LINE_PERCENT=1` is a legal
# raise from 0 that walks straight past this failure and gates nothing, which
# is the whole escape hatch this check exists to not have.
if awk -v m="$DEFAULT_MIN_LINE_PERCENT" 'BEGIN { exit !(m + 0 <= 0) }'; then
    seed="$(awk -v p="$percent" 'BEGIN { s = int(p) - 1; if (s < 0) s = 0; printf "%d", s }')"
    note "error: the coverage floor in contrib/check-coverage.sh is ${DEFAULT_MIN_LINE_PERCENT}%, so this check gates nothing."
    note "       Set DEFAULT_MIN_LINE_PERCENT=${seed} in contrib/check-coverage.sh"
    note "       (the measurement above, a point down for run-to-run drift) and the"
    note "       ratchet starts biting. It fails until then on purpose: a coverage"
    note "       check that cannot go red is a report wearing a gate's name."
    exit 1
fi

if awk -v p="$percent" -v m="$MIN_LINE_PERCENT" 'BEGIN { exit !(p + 0 < m + 0) }'; then
    echo "error: line coverage fell below the $MIN_LINE_PERCENT% ratchet." >&2
    echo "Add tests for what you changed. The floor is DEFAULT_MIN_LINE_PERCENT" >&2
    echo "in this file and it only moves up; MIN_LINE_PERCENT cannot lower it." >&2
    exit 1
fi
