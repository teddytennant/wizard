#!/usr/bin/env bash
# Build one image per agent, measure each in its own container, write
# results.md and results.json next to this script.
#
#   bench/startup/run.sh                 # every agent
#   bench/startup/run.sh wizard codex    # a subset
#
#   RUNS=10        pty starts per agent (median and p90 come from these)
#   NET=bridge     container network for the measurement runs. Most agents
#                  phone home before drawing the prompt and sit in retry
#                  loops without a network, so the default is a real one.
#                  NET=none measures them offline.
#   OUT=dir        where the per-agent JSON lands (default: bench/startup/out)
#   SKIP_BUILD=1   reuse the images that are already built
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
RUNS=${RUNS:-10}
NET=${NET:-bridge}
OUT=${OUT:-$here/out}
agents=("$@")
[ ${#agents[@]} -eq 0 ] && agents=(wizard claude codex opencode crush goose aider)
mkdir -p "$OUT"

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

layer_bytes() {
    docker history --human=false --format '{{.Size}}' "$1" | awk '{s+=$1} END {print s+0}'
}

if [ -z "${SKIP_BUILD:-}" ]; then
    log "building base image"
    docker build -q -t wizard-bench-base -f "$here/Dockerfile.base" "$here" >/dev/null
    for a in "${agents[@]}"; do
        log "building $a"
        if ! docker build -q -t "wizard-bench-$a" "$here/agents/$a" > "$OUT/$a.build.log" 2>&1; then
            log "build failed for $a, see $OUT/$a.build.log"
            echo "{\"name\": \"$a\", \"install_failed\": true}" > "$OUT/$a.json"
        fi
    done
fi

base_bytes=$(layer_bytes wizard-bench-base)

for a in "${agents[@]}"; do
    if ! docker image inspect "wizard-bench-$a" >/dev/null 2>&1; then
        log "no image for $a, skipping"
        continue
    fi
    # shellcheck disable=SC1090
    . "$here/agents/$a/bench.env"
    install_bytes=$(( $(layer_bytes "wizard-bench-$a") - base_bytes ))
    log "measuring $a ($RUNS runs, network=$NET, install $((install_bytes / 1048576)) MB)"
    docker run --rm --network "$NET" \
        -e OPENAI_API_KEY=sk-bench \
        -e ANTHROPIC_API_KEY=sk-ant-bench-00000000000000000000 \
        -e HOST_UID="$(id -u)" \
        -v "$here:/bench:ro" -v "$OUT:/out" \
        "wizard-bench-$a" bash -c '
            a=$1; runs=$2; cmd=$3; marker=$4; vcmd=$5; bundle=$6
            [ -f /bench/agents/$a/setup.sh ] && sh /bench/agents/$a/setup.sh
            python3 /bench/measure.py --name "$a" --cmd "$cmd" --marker "$marker" \
                --version-cmd "$vcmd" --runs "$runs" --settle 3 --timeout 120 \
                --dump /out/$a.screen.txt --out /out/$a.json
            bytes=$(sh -c "$bundle" 2>/dev/null || echo 0)
            python3 - "$a" "$bytes" <<EOF
import json, sys
p = "/out/%s.json" % sys.argv[1]
d = json.load(open(p))
d["bundle_bytes"] = int(sys.argv[2] or 0)
json.dump(d, open(p, "w"), indent=2)
EOF
            chown -R "$HOST_UID" /out
        ' _ "$a" "$RUNS" "$CMD" "$MARKER" "$VERSION_CMD" "$BUNDLE" 2>&1 | grep -v '^ ' | sed "s/^/  /" >&2 || true
    python3 - "$OUT/$a.json" "$install_bytes" "$NET" "${NOTE:-}" <<'EOF'
import json, sys
p, install, net, note = sys.argv[1:]
d = json.load(open(p))
d["install_bytes"] = int(install)
d["network"] = net
d["note"] = note
json.dump(d, open(p, "w"), indent=2)
EOF
    unset CMD MARKER VERSION_CMD BUNDLE NOTE
done

python3 "$here/report.py" "$OUT" "$here/results.md" "$here/results.json"
log "wrote $here/results.md"
