#!/usr/bin/env bash
# Build one image per agent, measure each in its own container, write
# results.md and results.json next to this script.
#
#   bench/startup/run.sh                 # every agent -> results.md
#   bench/startup/run.sh wizard codex    # a subset  -> results-wizard-codex.md
#
#   RUNS=10           pty starts per agent: run 1 is the cold start, runs 2..N
#                     give the warm median and p90
#   NET=bridge        container network. Every agent has its phone-home
#                     switches off and its model endpoint on a dead local
#                     port, so what is left on the wire is whatever an agent
#                     does with no switch. NET=none is not comparable: some
#                     agents retry for a long time with no route at all.
#   OUT=dir           per-agent JSON and screen dumps (default bench/startup/out)
#   SKIP_BUILD=1      reuse images that are already built
#   LOAD_OK=1         measure on a busy host anyway (the numbers are then
#                     about the host, not the agents)
#   WIZARD_VERSION=v3.0.1   release tag install.sh downloads for the wizard image
#   WIZARD_BINARY=/path     measure this binary instead of a release: it goes
#                           into the same base image with the repo's loadout/
#                           laid down the way install.sh does
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
RUNS=${RUNS:-10}
NET=${NET:-bridge}
OUT=${OUT:-$here/out}
all=(wizard claude codex opencode crush goose aider)
agents=("$@")
[ ${#agents[@]} -eq 0 ] && agents=("${all[@]}")
mkdir -p "$OUT"

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

# --- host guard ----------------------------------------------------------
# A loaded host inflates every wall-clock number, and not evenly: the
# CPU-heavy agents suffer most. Refuse unless the box is quiet.
ncpu=$(nproc)
load1=$(cut -d' ' -f1 /proc/loadavg)
avail_kb=$(awk '/MemAvailable/ {print $2}' /proc/meminfo)
load_max=$(python3 -c "print($ncpu / 4)")
if [ -z "${LOAD_OK:-}" ]; then
    if python3 -c "import sys; sys.exit(0 if $load1 > $load_max else 1)"; then
        log "1-minute load is $load1, above $load_max (nproc/4). Wait, or LOAD_OK=1 to measure anyway."
        exit 2
    fi
    if [ "$avail_kb" -lt $((8 * 1024 * 1024)) ]; then
        log "only $((avail_kb / 1024)) MB available memory, under 8 GB. Wait, or LOAD_OK=1 to measure anyway."
        exit 2
    fi
fi
python3 "$here/hostinfo.py" > "$OUT/host.json"

layer_bytes() {
    docker history --human=false --format '{{.Size}}' "$1" | awk '{s+=$1} END {print s+0}'
}

build_wizard() {
    if [ -n "${WIZARD_BINARY:-}" ]; then
        [ -x "$WIZARD_BINARY" ] || { log "WIZARD_BINARY=$WIZARD_BINARY is not an executable file"; exit 1; }
        ctx="$here/agents/wizard/ctx"
        rm -rf "$ctx" && mkdir -p "$ctx"
        cp "$WIZARD_BINARY" "$ctx/wizard"
        cp -r "$here/../../loadout" "$ctx/loadout"
        log "building wizard from $WIZARD_BINARY"
        docker build -q -t wizard-bench-wizard -f "$here/agents/wizard/Dockerfile.local" "$ctx"
    else
        log "building wizard from release ${WIZARD_VERSION:-v3.0.1}"
        docker build -q -t wizard-bench-wizard --build-arg "WIZARD_VERSION=${WIZARD_VERSION:-v3.0.1}" "$here/agents/wizard"
    fi
}

if [ -z "${SKIP_BUILD:-}" ]; then
    log "building base image"
    docker build -q -t wizard-bench-base -f "$here/Dockerfile.base" "$here" >/dev/null
    for a in "${agents[@]}"; do
        if [ "$a" = wizard ]; then
            build_wizard > "$OUT/$a.build.log" 2>&1 || { log "build failed for wizard, see $OUT/wizard.build.log"; exit 1; }
            continue
        fi
        log "building $a"
        if ! docker build -q -t "wizard-bench-$a" "$here/agents/$a" > "$OUT/$a.build.log" 2>&1; then
            log "build failed for $a, see $OUT/$a.build.log"
            echo "{\"name\": \"$a\", \"install_failed\": true}" > "$OUT/$a.json"
        fi
    done
elif ! docker image inspect wizard-bench-base >/dev/null 2>&1; then
    log "SKIP_BUILD=1 but there is no wizard-bench-base image; install sizes would be wrong. Build first."
    exit 1
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
    set +e
    # shellcheck disable=SC2086
    docker run --rm --network "$NET" \
        -e OPENAI_API_KEY=sk-bench \
        -e ANTHROPIC_API_KEY=sk-ant-bench-00000000000000000000 \
        -e HOST_UID="$(id -u)" \
        ${ENV:+$(printf -- '-e %s ' $ENV)} \
        -v "$here:/bench:ro" -v "$OUT:/out" \
        "wizard-bench-$a" bash -c '
            a=$1; runs=$2; cmd=$3; marker=$4; vcmd=$5; bundle=$6
            [ -f /bench/agents/$a/setup.sh ] && sh /bench/agents/$a/setup.sh
            python3 /bench/measure.py --name "$a" --cmd "$cmd" --marker "$marker" \
                --version-cmd "$vcmd" --runs "$runs" --settle 3 --timeout 120 \
                --dump /out/$a.screen.txt --out /out/$a.json || exit 1
            bytes=$(sh -c "$bundle" 2>/dev/null || echo 0)
            python3 - "$a" "$bytes" <<EOF
import json, sys
p = "/out/%s.json" % sys.argv[1]
d = json.load(open(p))
d["bundle_bytes"] = int(sys.argv[2] or 0)
json.dump(d, open(p, "w"), indent=2)
EOF
            chown -R "$HOST_UID" /out
        ' _ "$a" "$RUNS" "$CMD" "$MARKER" "$VERSION_CMD" "$BUNDLE" 2>&1 | grep -v '^ ' | sed "s/^/  /" >&2
    status=${PIPESTATUS[0]}
    set -e
    if [ "$status" -ne 0 ]; then
        log "measurement failed for $a (exit $status)"
        echo "{\"name\": \"$a\", \"measure_failed\": true}" > "$OUT/$a.json"
    fi
    python3 - "$OUT/$a.json" "$install_bytes" "$NET" "${NOTE:-}" "${ENV:-}" <<'EOF'
import json, sys
p, install, net, note, env = sys.argv[1:]
d = json.load(open(p))
d["install_bytes"] = int(install)
d["network"] = net
d["note"] = note
d["env"] = env
d["rule"] = ("installed as its README says, default config, phone-home switched off where the agent "
             "offers a switch, model endpoint pointed at a dead local port (127.0.0.1:9).")
json.dump(d, open(p, "w"), indent=2)
EOF
    unset CMD MARKER VERSION_CMD BUNDLE NOTE ENV
done

if [ "${agents[*]}" = "${all[*]}" ]; then
    stem=results
else
    stem=results-$(IFS=-; echo "${agents[*]}")
fi
python3 "$here/report.py" "$OUT" "$here/$stem.md" "$here/$stem.json" "${agents[@]}"
log "wrote $here/$stem.md"
