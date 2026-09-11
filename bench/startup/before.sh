#!/usr/bin/env bash
# The first five minutes as a new user sees them: a fresh container, the
# main-branch install.sh timed end to end, then wizard started with no config
# and every onboarding screen dumped as plain text into before/. The path
# walked is "OpenAI / OpenAI-compatible" with a pasted key and the default
# answer everywhere else.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${OUT:-$here/before}
mkdir -p "$out"
docker image inspect wizard-bench-base >/dev/null 2>&1 || docker build -q -t wizard-bench-base -f "$here/Dockerfile.base" "$here" >/dev/null
docker run --rm --network bridge -e HOST_UID="$(id -u)" -v "$here:/bench:ro" -v "$out:/out" wizard-bench-base bash -c '
    apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq python3-pyte >/dev/null 2>&1
    cd /work
    s=$(date +%s%N)
    curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash > /out/install.log 2>&1
    e=$(date +%s%N)
    printf "install.sh: %d ms\nwizard --version: %s\n" $(( (e - s) / 1000000 )) "$(wizard --version)" | tee /out/install-time.txt
    rm -rf /out/screens
    python3 /bench/before/capture.py /out/screens wizard -- \
        dump:provider key:down key:down key:down key:down key:down wait:0.3 key:enter \
        dump:model wait:0.3 key:enter \
        dump:api-key text:sk-bench-0000 wait:0.3 key:enter \
        dump:key-env wait:0.3 key:enter \
        dump:gateway wait:0.3 key:enter \
        dump:mode wait:0.3 key:enter \
        dump:interface wait:0.3 key:enter \
        dump:web-search wait:0.3 key:enter \
        wait:3 dump:tui wait:3 dump:tui-settled
    chown -R "$HOST_UID" /out
'
echo "screens in $out/screens"
