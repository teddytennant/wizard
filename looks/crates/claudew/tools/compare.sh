#!/bin/sh
# Compare wizard-ui-claude with Claude Code, cell by cell, in a private tmux server.
#
#   crates/claudew/tools/compare.sh                  the look against tests/fixtures/ref-2.1.314
#   REFDIR=crates/claudew/tests/fixtures/ref crates/claudew/tools/compare.sh
#                                                    ... against the older 2.1.300 capture
#   REF=live crates/claudew/tools/compare.sh         capture the real `claude` first
#   REF=live SAVE=1 crates/claudew/tools/compare.sh  ... and store it as the new fixtures
#   REF=fake crates/claudew/tools/compare.sh         the same against a fake Anthropic server, in a
#                                                    HOME of its own: no account, no ~/.claude
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
cwd=${COMPARE_CWD:-/tmp/claude-1000/-home-owner/00000000-0000-0000-0000-000000000000/scratchpad/look-claude/proj}
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
# statusline_setup GROUP: the project settings the status line scenes run under
statusline_setup() {
    case "$1" in
    statusline) line="{\"statusLine\":{\"type\":\"command\",\"command\":\"$here/statusline-a.sh\"}}" ;;
    statusline-pad) line="{\"statusLine\":{\"type\":\"command\",\"command\":\"$here/statusline-pad.sh\",\"padding\":2}}" ;;
    *) return 0 ;;
    esac
    mkdir -p "$cwd/.claude"
    printf '%s\n' "$line" > "$cwd/.claude/settings.json"
}
# the scenes compared with live wizard run in a project of their own, so the sessions /resume
# lists are only theirs
xcwd=$(dirname "$cwd")/proj-x
mkdir -p "$xcwd/.wizard" "$xcwd/src"
printf 'fn main() {\n    println!("hello");\n}\n' > "$xcwd/main.rs"
(cd "$root" && cargo build -q -p claudew)
bin="${CARGO_TARGET_DIR:-$root/target}/debug/claudew"

# refrun runs a reference capture. REF=fake gives it a HOME of its own and an Anthropic
# endpoint that is fake_anthropic.py, so nothing reaches an account or ~/.claude.
refrun() { "$@"; }
# group NAME: whether to capture it; REF_GROUPS="turn3 shared" captures only those
group() {
    [ -z "${REF_GROUPS:-}" ] && return 0
    case " $REF_GROUPS " in *" $1 "*) return 0 ;; esac
    return 1
}
if [ "${REF:-}" = fake ]; then
    port=${FAKE_PORT:-18997}
    fakehome="$work/claude-ref-home"
    mkdir -p "$fakehome"
    key=sk-ant-api03-fake00000000000000000000000000000000000000
    python3 "$here/fake_anthropic.py" "$port" "$work/fake-anthropic.log" &
    fakepid=$!
    trap 'kill $fakepid' EXIT
    cat > "$fakehome/.claude.json" <<EOF
{"hasCompletedOnboarding": true, "lspRecommendationDisabled": true, "theme": "dark", "customApiKeyResponses": {"approved": ["$(printf %s "$key" | tail -c 20)"], "rejected": []},
 "projects": {"$cwd": {"hasTrustDialogAccepted": true}, "$xcwd": {"hasTrustDialogAccepted": true}}}
EOF
    fakeenv="ANTHROPIC_BASE_URL=http://127.0.0.1:$port ANTHROPIC_API_KEY=$key DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1"
    # the task tools are on for the models 2.1.314 knows; the fake key's haiku needs this
    refrun() { env HOME="$fakehome" LOOK_ENV="$fakeenv ${EXTRA_REF_ENV:-}" "$@"; }
fi

if [ "${REF:-}" = live ] || [ "${REF:-}" = fake ]; then
    ref="$work/ref"
    # REF_BASE=dir starts from an earlier capture, so REF_GROUPS can add or redo a few scenes
    if [ -n "${REF_BASE:-}" ]; then mkdir -p "$ref" && cp -r "$REF_BASE/." "$ref/"; fi
    claude=${CLAUDE_BIN:-claude}
    cmd="$claude --setting-sources project --strict-mcp-config --permission-mode default --model haiku"
    group static && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group static --sizes "$sizes"
    # 2.1.314 remembers a backslash newline in its config, which the static scenes type; the light
    # scenes are captured as if none had been
    python3 - "$fakehome/.claude.json" <<'PY' 2>/dev/null || true
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d.pop('hasUsedBackslashReturn', None)
json.dump(d, open(p, 'w'))
PY
    group light && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\",\"theme\":\"light\"}'" --cwd "$cwd" --out "$ref" --group light --sizes "$sizes"
    group turn && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group turn --sizes "$sizes" \
        --before "printf 'fn main() {\n    println!(\"hello\");\n}\n' > main.rs"
    group turn2 && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group turn2 --sizes 80x24,120x40 \
        --before "rm -rf w.txt .git"
    group turn3 && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group turn3 --sizes "$sizes" \
        --before "printf 'fn main() {\n    println!(\"hello\");\n}\n' > main.rs"
    group think && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group think --sizes 80x24,120x40
    # the scenes live wizard is compared with cell by cell, and a session resuming them
    group shared && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$xcwd" --out "$ref" --group shared --sizes 120x40 \
        --before "printf 'fn main() {\n    println!(\"hello\");\n}\n' > main.rs"
    group shared-resume && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$xcwd" --out "$ref" --group shared-resume --sizes 120x40
    for g in statusline statusline-pad; do
        if group $g; then
            statusline_setup $g
            refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group $g --sizes "$sizes"
            rm -rf "$cwd/.claude"
        fi
    done
    group plan2 && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group plan2 --sizes "$sizes"
    group plan3 && refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group plan3 --sizes "$sizes"
    for g in todo todo-many; do
        group $g && EXTRA_REF_ENV=CLAUDE_CODE_ENABLE_TODO_TOOLS=1 refrun python3 "$here/drive.py" --target ref --cmd "$cmd --settings '{\"tui\":\"fullscreen\"}'" --cwd "$cwd" --out "$ref" --group $g --sizes "$sizes"
    done
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
        if [ "${REF:-}" = fake ]; then
            # the fake-key capture goes to its own directory and leaves the tests' replay alone
            mkdir -p "$crate/tests/fixtures/ref-2.1.314"
            cp "$ref"/* "$crate/tests/fixtures/ref-2.1.314/"
            run_id=$(printf '%s\n' "$cwd" | sed -n 's#.*/\(00000000-0000-0000-0000-[0-9a-f]*\)/scratchpad/.*#\1#p')
            sed -i "s#$run_id#00000000-0000-0000-0000-000000000000#" "$crate"/tests/fixtures/ref-2.1.314/*.ansi
        else
            rm -f "$crate/tests/fixtures/ref"/*
            cp "$ref"/* "$crate/tests/fixtures/ref/"
            python3 "$here/mkreplay.py" "$ref" "$cwd" "$crate/tests/fixtures/replay"
        fi
    fi
else
    # REFDIR=captures keeps an earlier capture as the reference (no new one is taken). The
    # default is the stored capture of the installed 2.1.314; REFDIR=$crate/tests/fixtures/ref
    # is the older 2.1.300 one from a subscription.
    ref="${REFDIR:-$crate/tests/fixtures/ref-2.1.314}"
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
cp -r "$work/claude-home" "$work/claude-home-light"
for size in $(echo "$sizes" | tr , ' '); do
    for group in static turn light turn2 turn3 think wizard statusline statusline-pad todo todo-many plan2 plan3; do
        # LOOK_GROUPS="todo todo-many" runs only those
        [ -z "${LOOK_GROUPS:-}" ] || case " $LOOK_GROUPS " in *" $group "*) ;; *) continue ;; esac
        script="$work/replay/$group-$size.jsonl" theme=dark home="$work/claude-home"
        [ "$group" = statusline-pad ] && script="$work/replay/static-$size.jsonl"
        [ "$group" = wizard ] && home="$work/wizard-home"
        [ "$group" = light ] && script="$work/replay/static-$size.jsonl" theme=light
        # the light reference never typed a backslash newline, so its shortcuts table still explains it:
        # the light scenes run in a home the static scenes have not touched
        [ "$group" = light ] && home="$work/claude-home-light"
        [ -f "$ref/md-$size.ansi" ] || [ "$group" != turn2 ] || continue
        [ -f "$ref/plan-$size.ansi" ] || [ "$group" != turn3 ] || continue
        [ -f "$ref/think-done-$size.ansi" ] || [ "$group" != think ] || continue
        [ -f "$ref/sl-home-$size.ansi" ] || [ "$group" != statusline ] || continue
        [ -f "$ref/slp-home-$size.ansi" ] || [ "$group" != statusline-pad ] || continue
        [ -f "$ref/plan-approve-$size.ansi" ] || [ "$group" != plan2 ] || continue
        [ -f "$ref/plan-feedback-$size.ansi" ] || [ "$group" != plan3 ] || continue
        [ -f "$ref/todo-$size.ansi" ] || [ "$group" != todo ] || continue
        [ -f "$ref/todo-many-$size.ansi" ] || [ "$group" != todo-many ] || continue
        reset
        statusline_setup "$group"
        rm -rf "$cwd/w.txt" "$cwd/.git"
        python3 "$here/drive.py" --target look --cwd "$cwd" --out "$work/look" --group "$group" --sizes "$size" \
            --cmd "env HOME=$home CLAUDEW_BACKEND=replay CLAUDEW_REPLAY=$script $bin --theme $theme --cwd $cwd" > /dev/null
    done
done
echo "captures and report in $work"
status=0
python3 "$here/compare.py" "$ref" "$work/look" --report "$work/report" --sizes "$sizes" ${COMPARE_ARGS:-} \
    --version "$(cat "$work/replay/version.txt")" --cwd "$cwd" || status=1

# --version names this crate's version and the reference's
crate_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
# the installed claude when capturing live, the fixtures' otherwise
if [ "${REF:-}" = live ] || [ "${REF:-}" = fake ]; then
    claude_version=$("${CLAUDE_BIN:-claude}" --version | sed -n 's/^\([0-9][0-9.]*\).*/\1/p')
else
    # what the stored capture says it is; the older tests/fixtures/ref is 2.1.300 and the look
    # follows 2.1.314, so against that one the look's own number stands in
    claude_version=$(sed 's/\x1b\[[0-9;]*m//g' "$ref/home-80x24.ansi" | sed -n 's/.*Claude Code v\([0-9.]*\).*/\1/p' | head -1)
    [ "${ref%/ref}" = "$ref" ] || claude_version=$(sed -n 's/.*CLAUDE_VERSION: &str = "\(.*\)";.*/\1/p' "$crate/src/app.rs")
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
