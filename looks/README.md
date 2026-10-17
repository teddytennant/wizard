# looks

The six full Wizard terminal looks: wizard-ui-opencode, wizard-ui-pi, wizard-ui-codex, wizard-ui-grok, wizard-ui-jcode, wizard-ui-claude. Each is its own binary that `/ui` restarts into, talking to `wizard acp`.

This is its own cargo workspace. `cargo build --release` here builds `openw` (opencode), `piw`, `codexw`, `grokw`, `jcodew` and `claudew`; the release copies them into the tarball beside wizard as `wizard-ui-<look>`, and that is also where a local build has to go for `/ui` to find it.

`crates/claudew/tools/compare.sh` puts wizard-ui-claude and the claude CLI through the same keys in a private tmux server and diffs the two screens cell by cell. With `WIZARD_BIN=path/to/wizard` it also drives real wizard behind the look, against a fake OpenAI-compatible server, and checks what only shows with Wizard's own data.
