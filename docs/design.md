# TUI design brief

What the screen is for: reading what the model said, watching what a tool did,
and typing the next thing. Everything else is chrome, and chrome is paid for in
the user's attention. This file is the standard a screen is held to. The house
skin (`wizard`) follows it; the `codex` and `grok` skins reproduce someone
else's chrome and are held to theirs.

## Rules

- **One accent.** The prompt glyph, the agent's `·` marker, a tool name. Nothing
  else takes it. Emphasis elsewhere is bold or brightness, never a second hue.
  The only conventional hues are green `+` and red `-` in a diff.
- **Meaning never rests on color.** Every state also reads through a glyph or a
  word (`✓`, `✗`, `+`, `-`, `exit 2`), so `NO_COLOR`, 16 and 256 colors show the
  same facts.
- **No backgrounds.** The terminal's own background shows through everywhere.
- **Chrome only where it carries information.** A rule separates the transcript
  from the composer because they scroll differently; there is no rule under the
  composer. No boxes in the transcript. Floating layers (pickers) get one
  border, because a picker has to read as being on top.
- **Spacing.** One blank row between turns. Tool cards in a run stay tight so
  they read as one group. Nothing is centered.
- **Nothing on screen the user did not ask for.** No splash art, no tagline, no
  tips, no idle key hints. The empty state is the name and version, then how to
  start; a startup problem sits between them. When the directory suggests
  any, up to three starter prompts hang under the hint as muted `❯` rows: the
  composer's own glyph, so they need no key hint, and ↓ moves onto them. On the
  first run one dim line after them says where the config went.
- **Fast.** No animation delays paint. Nothing waits on a spinner frame. A
  spinner never replaces output: while text streams there is no spinner at all.

## What each thing looks like

- **Status line** (bottom row): `model · branch · ctx · cost`. The model you
  are talking to, the git branch of the project root, the tokens the next call
  will load, and the session cost when a rate is known. Sovereign mode,
  `PLAN`, `OMAKASE`, `ULTRA ×N`, vim `NORMAL`, background tasks and a failed
  provider probe appear only while they are true. Right side: the elapsed time
  of a running turn, or the keys a modal state needs. Idle shows no hints.
- **Streaming text** renders as markdown as it arrives, with a dim `▍` at the
  tail. No spinner next to it.
- **Waiting on the model** (nothing streaming yet): one row, the spinner alone,
  replaced by the text the moment it starts. After the first round trip it
  carries the count, `⠋ step 2`, and the budget when there is one, `step 2/8`.
  The clock is the status line's; the screen has one.
- **Tool call**: one header row, `✓ execute  ls -la  0.4s`. The glyph is the
  state (spinner running, `✓` done, `✗` failed), the name is accent, the
  argument is dim, the elapsed time is dim and left off under 100 ms. A
  non-zero exit puts `exit 2` on the header. The output hangs below, indented
  two columns, dim. A running command shows its tail; a finished one its head,
  then `… +N lines`; a failed one stays open, because the lines under `✗` are
  the reason.
- **Diff**: the file name once, on the tool header (`✓ edit_file  src/x.rs:12`),
  relative to the project root when it is under it. The
  body is the change as `-` and `+` lines in the diff colors. The `/diff`
  sidebar shows each file once, then `@@` hunk headers dim, then the lines.
  `diff --git`, `index`, `---` and `+++` rows are not shown.
- **Errors**: `✗` then the message, bold, in the transcript where it happened.
  No red panel, no box. A startup problem is the first thing on the empty state.
- **Composer**: a dim rule, then `❯ ` and the draft. Grows to ten rows, then
  scrolls.

## Widths

- **80 columns**: everything above holds. Tool arguments truncate with `…` at
  the header's right edge; the status line drops chips from the right.
- **40 columns**: the status line keeps the model and drops the rest in order;
  every empty-state row that does not fit ends in `…`; tool headers keep the
  glyph and name. Nothing wraps a header onto a second row.

## Deliberately absent

Splash art. Taglines. Spinner verbs. Idle key hints. The working directory (the branch says which repo; `/status`
has the path). Emoji. Boxes around messages. Backgrounds. A second accent.
Progress bars for things with no known total, except compaction, which is one
opaque call and needs to show it is alive.
