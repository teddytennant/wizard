# Wizard's first run, recorded with vhs:
#   nix run nixpkgs#vhs -- demo/first-run.tape
# from the repo root, with a release binary at target/release/wizard
# (WIZARD_DEMO_BIN overrides) and python3 on PATH.
#
# What the recording is: a fresh HOME, the one first-run screen, "Paste an API
# key", OpenAI, a pasted key, the TUI with its card, and the first starter
# prompt answered. The answer comes from demo/mock-server.py, an
# OpenAI-compatible server that streams one canned reply; no model is involved.
# The shell runs in its own network namespace (unshare -rn), so the pasted key
# reaches nothing and the card says so. Once the card has been on screen, the
# recording pauses while the live provider is pointed at the mock server; a
# slash command dismisses the card, so the starter prompt is typed out after
# that instead of picked with the arrow keys. Everything on screen is the real
# TUI.

Output demo/first-run.gif

Set Shell "bash"
Set FontSize 15
Set Width 1200
Set Height 680
Set Padding 14
Set Framerate 20
Set TypingSpeed 40ms
Set Theme { "name": "wizard-warm", "background": "#1a1a17", "foreground": "#e9e6df", "cursor": "#e9e6df", "black": "#11110f", "red": "#e06a55", "green": "#a3b285", "yellow": "#d6b36a", "blue": "#95a3b3", "magenta": "#b59a8e", "cyan": "#9cb3a8", "white": "#94908a", "brightBlack": "#55524b", "brightRed": "#f0836b", "brightGreen": "#b8c79a", "brightYellow": "#e8c98a", "brightBlue": "#aebccb", "brightMagenta": "#c9ada0", "brightCyan": "#b0c7bc", "brightWhite": "#f1eee6" }

# --- hidden setup: fresh HOME, a throwaway copy of the sample repo, the mock provider
Hide
Type "export WIZARD_DEMO_ROOT=$PWD WIZARD_DEMO_BIN=${WIZARD_DEMO_BIN:-$PWD/target/release/wizard}"
Enter
Type "unshare -rn bash --norc"
Enter
Type "ip link set lo up; export HOME=$(mktemp -d); export PATH=$HOME/bin:$PATH; mkdir -p $HOME/bin; ln -s $WIZARD_DEMO_BIN $HOME/bin/wizard; test -x $HOME/bin/wizard || echo NO BINARY AT $WIZARD_DEMO_BIN"
Enter
Type "python3 $WIZARD_DEMO_ROOT/demo/mock-server.py 8089 & disown"
Enter
Type "rm -rf /tmp/wizard-demo && cp -r $WIZARD_DEMO_ROOT/demo/sample-project /tmp/wizard-demo && cd /tmp/wizard-demo && printf '# calc\\n' > README.md && git init -q && git add -A && git -c user.name=Demo -c user.email=demo@example.com commit -qm initial"
Enter
Type "export PS1='\[\e[92m\]user\[\e[0m\]@wizard \[\e[32m\]/t/wizard-demo\[\e[0m\]> '; clear"
Enter
Sleep 1s
Show

# --- launch: the one screen
Type "wizard"
Sleep 600ms
Enter
Sleep 2s

# --- Paste an API key (third row), then OpenAI (third row)
Down
Sleep 400ms
Down
Sleep 600ms
Enter
Sleep 1.5s
Down
Sleep 400ms
Down
Sleep 600ms
Enter
Sleep 1.2s

# --- the key, masked as it is typed
Type@60ms "sk-demo-not-a-real-key"
Sleep 800ms
Enter

# The TUI opens with the card and the starter prompts. The check against
# api.openai.com cannot reach it from the namespace, so the key is saved
# unchecked and the card says so.
Sleep 3s

# Paused: point the live provider at the mock server on 127.0.0.1.
Hide
Type "/provider add demo openai http://127.0.0.1:8089/v1 demo-model"
Enter
Sleep 800ms
Type "/provider use demo"
Enter
Sleep 800ms
Show
Sleep 1s

# --- the first starter prompt, typed
Type@45ms "Explain how this project is put together"
Sleep 800ms
Enter

# the canned answer streams in; its last words are the marker
Wait+Screen@60s /add a test/
Sleep 3s

Type@60ms "/quit"
Sleep 500ms
Enter
Sleep 1s
