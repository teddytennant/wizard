#!/bin/sh
# Compare wizard-ui-claude with Claude Code, cell by cell, in a private tmux server.
#
#   crates/claudew/tools/compare.sh                  the look against tests/fixtures/ref
#   REF=live crates/claudew/tools/compare.sh         capture the real `claude` first
#   REF=live SAVE=1 crates/claudew/tools/compare.sh  ... and store it as the new fixtures
#
#   WIZARD_BIN=path/to/wizard crates/claudew/tools/compare.sh
#                                                    ... and the live-Wizard scenes (live.py)
#
# Three parts, each of which can fail the run:
#   Claude data: the replay hands the look what the reference showed (its commands, models,
#     sessions) so the layout can be compared cell by cell. Prints a mismatch count per scene
#     and size and writes a side by side per scene to $WORK/report.
#   Wizard data: the look with a recorded wizard's models, modes and commands, compared by
#     the layout of each row (wz-* scenes).
#   Live: real `wizard` (WIZARD_BIN) behind the look, answering from tools/fake_openai.py,
#     checked for what only shows with Wizard's own data. Skipped without WIZARD_BIN.
# `--version` is checked too. The light theme is compared. A live reference capture sends a
# few tiny prompts and a /compact per size through `claude` (CLAUDE_BIN), on whatever account
# it is logged in to; it never touches ~/.claude.
# Needs tmux and python3. Every tmux call uses the socket `lookclaude`.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
crate=$(dirname "$here")
root=$(cd "$crate/../.." && pwd)
work=${WORK:-$(mktemp -d "${TMPDIR:-/tmp}/claudew-compare.XXXXXX")}
sizes=${SIZES:-80x24,120x40,200x50}
# The directory the fixtures were captured in, user name scrubbed. The header shows it,
# and its length decides wraps, so both programs run in exactly this path.
cwd=/tmp/claude-1000/-home-owner/00000000-0000-0000-0000-000000000000/scratchpad/look-claude/proj
reset() {
    # a project wizard has run in has .wizard/, and the @ menu lists it; claude's own
    # .claude/ would not be there in real use
    rm -rf "$cwd/.claude"
    mkdir -p "$cwd/.wizard" "$cwd/src"
    printf 'fn main() {\n    println!("hello");\n}\n' > "$cwd/main.rs"
    printf '# notes\n\nalpha\nbeta\n' > "$cwd/notes.md"
    echo 'pub fn add(a: i32, b: i32) -> i32 { a + b }' > "$cwd/src/lib.rs"
}
reset
# the scenes compared with live wizard run in a project of their own, so the sessions /resume
# lists are only theirs
xcwd=$(dirname "$cwd")/proj-x
mkdir -p "$xcwd/.wizard" "$xcwd/src"
printf 'fn main() {\n    println!("hello");\n}\n' > "$xcwd/main.rs"
(cd "$root" && cargo build -q -p claudew)
bin="${CARGO_TARGET_DIR:-$root/target}/debug/claudew"

if [ "${REF:-}" = live ]; then
    ref="$work/ref"
    claude=${CLAUDE_BIN:-claude}
    cmd="$claude --setting-sources project --strict-mcp-config --permission-mode default --model haiku"
    python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group static --sizes "$sizes"
    python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\",\"theme\":\"light\"}'" --cwd "$cwd" --out "$ref" --group light --sizes "$sizes"
    python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group turn --sizes "$sizes" \
        --before "printf 'fn main() {\n    println!(\"hello\");\n}\n' > main.rs"
    python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group turn2 --sizes 80x24,120x40 \
        --before "rm -rf w.txt .git"
    python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group turn3 --sizes 80x24,120x40 \
        --before "printf 'fn main() {\n    println!(\"hello\");\n}\n' > main.rs"
    # the scenes live wizard is compared with cell by cell, and a session resuming them
    python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$xcwd" --out "$ref" --group shared --sizes 120x40 \
        --before "printf 'fn main() {\n    println!(\"hello\");\n}\n' > main.rs"
    python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$xcwd" --out "$ref" --group shared-resume --sizes 120x40
    # scrub what is personal: the account's address and organisation, the home directory
    python3 - "$ref" <<'PY'
import glob, re, sys
for f in glob.glob(sys.argv[1] + '/*.ansi'):
    t = open(f, encoding='utf-8').read()
    t = re.sub(r'[\w.+-]+@[\w-]+(\.[\w-]+)+', 'owner@example.com', t)
    t = re.sub(r"(Organization:(\x1b\[[0-9;]*m)?\s+).*", r"\1Example Organization", t)
    t = re.sub(r'-home-[a-z0-9_]+/', '-home-owner/', t)
    t = re.sub(r'(?<=\()(America|Europe|Asia|Africa|Australia|Pacific)/[A-Za-z_]+', 'Region/City', t)
    open(f, 'w', encoding='utf-8').write(t)
PY
    if [ "${SAVE:-}" = 1 ]; then
        rm -f "$crate/tests/fixtures/ref"/*
        cp "$ref"/* "$crate/tests/fixtures/ref/"
        python3 "$here/mkreplay.py" "$ref" "$cwd" "$crate/tests/fixtures/replay"
    fi
else
    ref="$crate/tests/fixtures/ref"
fi

python3 "$here/mkreplay.py" "$ref" "$cwd" "$work/replay"
# Claude data includes the reference's agents as ~/.wizard/subagents, for the @ menu; the
# Wizard-data scenes get a home with nothing in it.
mkdir -p "$work/wizard-home" "$work/claude-home"
cp -r "$crate/tests/fixtures/claude-data-home/." "$work/claude-home/"
mkdir -p "$work/claude-home/.wizard/sessions"
while IFS="$(printf '\t')" read -r id bytes; do
    head -c "$bytes" /dev/zero | tr '\0' ' ' > "$work/claude-home/.wizard/sessions/$id.jsonl"
done < "$work/replay/session-files.tsv"
for size in $(echo "$sizes" | tr , ' '); do
    for group in static turn light turn2 turn3 wizard; do
        script="$work/replay/$group-$size.jsonl" theme=dark home="$work/claude-home"
        [ "$group" = wizard ] && home="$work/wizard-home"
        [ "$group" = light ] && script="$work/replay/static-$size.jsonl" theme=light
        [ -f "$ref/md-$size.ansi" ] || [ "$group" != turn2 ] || continue
        [ -f "$ref/plan-$size.ansi" ] || [ "$group" != turn3 ] || continue
        reset
        rm -rf "$cwd/w.txt" "$cwd/.git"
        python3 "$here/drive.py" --target look --cwd "$cwd" --out "$work/look" --group "$group" --sizes "$size" \
            --cmd "env HOME=$home CLAUDEW_BACKEND=replay CLAUDEW_REPLAY=$script $bin --theme $theme --cwd $cwd" > /dev/null
    done
done
echo "captures and report in $work"
status=0
python3 "$here/compare.py" "$ref" "$work/look" --report "$work/report" --sizes "$sizes" \
    --version "$(cat "$work/replay/version.txt")" --cwd "$cwd" || status=1

# --version names this crate's version and the reference's
crate_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
# the installed claude when capturing live, the fixtures' otherwise
if [ "${REF:-}" = live ]; then
    claude_version=$("${CLAUDE_BIN:-claude}" --version | sed -n 's/^\([0-9][0-9.]*\).*/\1/p')
else
    claude_version=$(sed 's/\x1b\[[0-9;]*m//g' "$ref/home-80x24.ansi" | sed -n 's/.*Claude Code v\([0-9.]*\).*/\1/p' | head -1)
fi
want="wizard-ui-claude $crate_version (claude look $claude_version)"
got=$("$bin" --version)
if [ "$got" = "$want" ]; then
    echo "--version: ok ($got)"
else
    echo "--version: '$got', want '$want'"
    status=1
fi

if [ -n "${WIZARD_BIN:-}" ]; then
    reset
    python3 "$here/live.py" --wizard "$WIZARD_BIN" --look "$bin" --cwd "$cwd" --work "$work" \
        --sizes "${LIVE_SIZES:-80x24,120x40}" --ref "$ref" --xcwd "$xcwd" || status=1
    # live wizard against claude, cell by cell, on the scenes both were put through
    echo "live wizard against the reference:"
    python3 "$here/compare.py" "$ref" "$work/live" --only x- --sizes 120x40 --report "$work/report-live" \
        --version "$("$WIZARD_BIN" --version | awk '{print $NF}')" --cwd "$xcwd" || status=1
else
    echo "live: skipped (set WIZARD_BIN to a wizard binary to drive real wizard)"
fi
exit $status
