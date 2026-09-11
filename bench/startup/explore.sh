#!/usr/bin/env bash
# One run of an agent with the stripped output dumped, for finding markers.
set -u
a=$1; runs=${2:-1}; out=${3:-/tmp/bench-out}; mkdir -p "$out"
here=$(cd "$(dirname "$0")" && pwd)
. "$here/agents/$a/bench.env"
# shellcheck disable=SC2086
docker run --rm -e MEASURE_TRACE --network "${NET:-none}" ${ENV:+$(printf -- '-e %s ' $ENV)} -e OPENAI_API_KEY=sk-bench -e ANTHROPIC_API_KEY=sk-ant-bench-00000000000000000000 \
  -v "$here:/bench:ro" -v "$out:/out" "wizard-bench-$a" bash -c \
  "[ -f /bench/agents/$a/setup.sh ] && sh /bench/agents/$a/setup.sh; python3 /bench/measure.py --name $a --cmd '$CMD' --marker '$MARKER' --runs $runs --settle 3 --timeout ${TIMEOUT:-30} --dump /out/$a.dump --out /out/$a.json"
