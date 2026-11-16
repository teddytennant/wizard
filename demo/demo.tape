# One task in the house TUI, then the same conversation in every look.
#
#   nix shell nixpkgs#vhs nixpkgs#ttyd nixpkgs#ffmpeg -c vhs demo/demo.tape
#
# Run from the repo root with `wizard` and the `wizard-ui-*` looks on PATH and
# a provider that answers. The take copies sample-project to ~/tally, so point
# HOME at a throwaway directory with its own .wizard/config.toml.

Output demo/wizard.mp4

Set Shell "bash"
Set FontFamily "JetBrains Mono, DejaVu Sans Mono"
Set FontSize 16
Set Width 1280
Set Height 720
Set Padding 16
Set Framerate 30
Set TypingSpeed 45ms
Set Theme { "name": "wizard-warm", "background": "#1a1a17", "foreground": "#e9e6df", "cursor": "#e9e6df", "black": "#11110f", "red": "#e06a55", "green": "#a3b285", "yellow": "#d6b36a", "blue": "#95a3b3", "magenta": "#b59a8e", "cyan": "#9cb3a8", "white": "#94908a", "brightBlack": "#55524b", "brightRed": "#f0836b", "brightGreen": "#b8c79a", "brightYellow": "#e8c98a", "brightBlue": "#aebccb", "brightMagenta": "#c9ada0", "brightCyan": "#b0c7bc", "brightWhite": "#f1eee6" }

Hide
Type "export PS1='\[\e[92m\]demo\[\e[0m\]@workstation \[\e[32m\]~/tally\[\e[0m\]> '"
Enter
Type "rm -rf ~/tally && cp -r demo/sample-project ~/tally && cd ~/tally && git init -q -b main && git add -A && git -c user.name=Demo -c user.email=demo@example.com commit -qm initial && clear"
Enter
Sleep 1s
Show

Type "wizard"
Sleep 300ms
Enter
Sleep 2.5s

Type "fix the failing test"
Sleep 500ms
Enter
Sleep 5s

# The idle status line ends in the cost, a busy one in the elapsed time. Each
# look is waited on by its own chrome, since a look can take a moment to start.
Hide
Wait+Screen@240s /(?m)· <?\$\d+\.\d+\s*$/
Show
Sleep 3.5s

Type@30ms "/ui codex"
Enter
Wait+Screen@20s /default · ~.tally/
Sleep 2.5s
Type@30ms "/ui grok"
Enter
Wait+Screen@20s /Shift\+Tab:mode/
Sleep 2.5s
Type@30ms "/ui opencode"
Enter
Wait+Screen@20s /ctrl\+p commands/
Sleep 2.5s
Type@30ms "/ui pi"
Enter
Wait+Screen@20s /grok-4\.7 • default/
Sleep 2.5s
Type@30ms "/ui jcode"
Enter
Wait+Screen@20s /Grok 4\.7/
Sleep 2.5s
Type@30ms "/ui claude"
Enter
Wait+Screen@20s /for shortcuts/
Sleep 4s
