# openw

The six Wizard terminal looks: wizard-ui-opencode, wizard-ui-pi, wizard-ui-codex, wizard-ui-grok, wizard-ui-jcode, wizard-ui-claude.

The Wizard release builds these and ships them in the same tarball as wizard. This repo is the source that build checks out.

`crates/claudew/tools/compare.sh` puts wizard-ui-claude and the claude CLI through the same keys in a private tmux server and diffs the two screens cell by cell. With `WIZARD_BIN=path/to/wizard` it also drives real wizard behind the look, against a fake OpenAI-compatible server, and checks what only shows with Wizard's own data.
