# TUI design brief

What the screen is for: reading what the model said, watching what a tool did,
and typing the next thing. Everything else is chrome, and chrome is paid for in
the user's attention. This file is the standard a screen is held to. The house
skin (`wizard`) follows it; the `codex` and `grok` skins reproduce someone
else's chrome and are held to theirs.

The house look is a transcript, not a landing page. Claude Code's restraint
and Pi's minimalism are the reference: one accent, status colors, whitespace,
and glyphs that say something.

## Rules

- **One accent, plus status colors.** The prompt glyph, the agent's `◆`, a
  focused name. Diffs and pass/fail use a sage and a dusty rose; yellow is a
  warning. Nothing else takes a hue. Emphasis elsewhere is bold or brightness.
  A single foreground cannot clear 4.5:1 on both a dark slate and a light
  cream ground (the equal-ratio ridge is about 2.69:1). The house tones sit
  on that ridge, still distinct, and the sign (`+`, `−`, `●`, `✕`) carries
  the fact when the hue does not.
- **Meaning never rests on color.** Every state also reads through a glyph or a
  word (`●`, `✕`, `+`, `-`, `exit 2`), so `NO_COLOR`, 16 and 256 colors show
  the same facts.
- **No backgrounds.** Never paint a background color. Every cell stays
  `Color::Reset`, so a transparent, blurred, light, or dark terminal shows
  through. The prompt band, the rail, and user rows get no fill. Selection is
  reverse video. Focus is accent text or bold, not a slab. There is no page,
  raised, or sunken tone.
- **Separate with space, or one dim rule.** The composer is the exception:
  the draft sits between two faint `─` rules of the same weight, and the
  status line is the row under the lower one. A wide terminal may put a faint
  `│` down the side rail. No fading divider, no boxes in the transcript.
  Floating layers (pickers) get one border, because a picker has to read as
  being on top.
- **Spacing does the work.** One blank row between turns. Tool cards in a run
  stay tight. Nothing is centered. Fewer `·` separators and fewer dim labels;
  alignment is the label.
- **Nothing on screen the user did not ask for.** No splash, no pixel
  wordmark, no tagline, no tip row, no idle key hints. The empty state is
  `◆ wizard {version}`, then a small aligned block (model, mode, directory
  and branch, context), then up to three recent sessions — or, when there
  are none, up to three starter `❯` rows — then one dim hint
  (`type a message`). A startup problem sits under the title. On the first
  run one dim line after the hint says where the config went.
- **Say what is happening.** A waiting turn is an indicator and a plain word:
  `thinking`, `writing`, or `running cargo` — the thing actually in flight.
  Cute verbs (`Conjuring…`) are not the house voice. A custom
  `[ui] spinner_verbs` list is still honored, because that is a setting the
  user wrote down. While text is streaming, the tail is the same dim braille
  the thinking row uses.
- **Fast.** No animation delays paint. Nothing waits on an indicator frame.
  `WIZARD_REDUCED_MOTION=1` freezes each indicator on its first frame (the
  waiting ellipsis becomes a single `…`, a subagent dot becomes `●`). The
  screen does not probe the terminal at startup (an OSC query hangs the TUI),
  so light versus dark is the user's background showing through, not a
  detected theme.

## What each thing looks like

- **Opening.** `◆ wizard 3.8.3` on one line (the version is dim), then a
  small left-aligned block: `model`, `mode`, `dir` (leaf and branch), and
  `context` as a meter. Up to three recent sessions under that, then one dim
  hint. No wordmark, no divider, no fill. Plan and omakase replace the mode
  word while they are on.
- **Status line** (bottom row): model, mode, branch, a context meter
  (`━━━━────── 32%`), and the session cost when a rate is known. `PLAN`,
  `OMAKASE`, `ULTRA ×N`, vim `NORMAL`, background tasks and a failed provider
  probe appear only while they are true. Right side: the elapsed time of a
  running turn, or the keys a modal state needs. Idle shows no hints. The
  working directory stays off this line; the branch names the repo and
  `/status` has the path.
- **Streaming text** renders as markdown as it arrives, with the dim thinking
  braille at the tail.
- **Tool call**: one header row, `● execute  ls -la  0.4s`. The glyph is the
  state (a thin line spinner while running, `●` done, `✕` failed), the name
  is accent, the argument is dim, the elapsed time is dim and left off under
  100 ms. A non-zero exit puts `exit 2` on the header. The output hangs below
  on a `╰`, dim. A running command shows its tail, or one progress line when
  the command is a build; a finished one its head, then `… +N lines`; a
  failed one stays open.
- **Compact view** (on by default): a turn's tools are one line,
  `▸ ran 3 commands  edited 2 files`. Enter or a click opens it (`▾`). The
  full view is Ctrl-O or `/view`, and the choice is saved.
- **Diff**: the file name once, on the tool header, with `+n −m` in the
  status tones (the minus is U+2212). The body is a line diff: a dim gutter,
  a `-` or `+` only on a changed line, and dim unchanged lines as context.
  Context and added lines show the new file's line number. A removed line
  shows the old file's number, which can sit beside a higher new number
  when the line was replaced. A run of unchanged lines folds to
  `┄ n unchanged lines`, and the `┄` starts in the code column of the
  shallowest line it hides. Only the words that changed are bold and underlined,
  and only when they are a minority of the line. A line that is mostly new
  stays plain colored text. Nothing is filled.
- **Errors**: `✗` then the message, bold, in the transcript where it happened.
  No red panel, no box.
- **Composer**: a dim rule, then `❯ ` and the draft, then the same dim rule.
  The status line is below that. Grows to ten rows, then scrolls. A window
  shorter than three rows keeps the prompt on the last row and drops the
  rules. No fill behind the draft.
- **Side rail** (120 columns and wider, and only when there is something to
  put there): a right column, about a quarter of the width, split off by one
  faint vertical rule. Subagents (name, elapsed, what they are doing, steps
  as `n/budget`), background commands (command, elapsed, the last output
  line or a progress bar), and todos. Text wraps inside the rule, indented,
  and never back onto it. A subagent's note is at most three lines; the last
  ends in `…` when there is more. Opening the run shows the rest. No fill.
  Under 120 columns this collapses to the one-row summary above the status
  line, and todos stay in their band. The bottom summary stays one line.
- **Background output** (Ctrl-B): the newest task's tail, in the transcript
  area, on the terminal's own background. Esc closes it. `x` stops a task
  that is still running.

## Glyphs

The core set is what a stock terminal can draw: `◆` `╰` `●` `✕` `❯` `▸`.
Kitty, Ghostty, WezTerm, and iTerm2 (`TERM_PROGRAM`, or `TERM` containing
`ghostty`) get the richer set (`✦`, `⎿`). `WIZARD_GLYPHS=ascii` uses
`*` `` ` `` `o` `x` `>`. `WIZARD_GLYPHS=core` forces the core set.

## Keys

- **Esc** closes whatever is open (search, history, task output, keys, diff,
  todos, suggestions, a subagent, the rail). At the bottom, with nothing open
  and an empty draft, Esc interrupts a running turn. Ctrl-C always interrupts,
  and a second Ctrl-C quits.
- **Shift+Tab** cycles genie → plan → omakase, and back. Sovereign and chat
  stay where `/mode` put them; Shift+Tab does not leave those. Inside a
  picker, Shift+Tab still moves the selection.
- **Ctrl-F** searches the transcript. **Ctrl-R** searches input history.
  **Ctrl-B** shows background output. **?** on an empty composer lists keys.
  Inside tmux or screen, a binding that is the prefix is that prefix pressed
  twice (`ctrl+b ctrl+b output` when the prefix is C-b). The hint says the
  chord the terminal will actually deliver. A custom prefix such as C-a
  doubles Ctrl-A and leaves Ctrl-B alone. `[ui] mux = "off"` turns the
  doubling off. `[ui] mux_prefix = "ctrl-a"` names the prefix instead of
  asking the multiplexer.
- **Ctrl-O** switches compact and full, and saves it. **Ctrl-T** opens or
  closes every collapsed turn (in the full view it toggles the last tool
  card). **Tab** then **Enter** opens the focused summary. A click does the
  same; a drag selects. Ctrl-Y copies the last reply.

## Widths

- **120 columns and up**: the side rail, when it has something to show.
- **80 columns**: everything else holds. Tool arguments truncate with `…`;
  the status line drops chips from the right. The rail is one summary row.
- **40 columns**: the status line keeps the model and drops the rest in order;
  every empty-state row that does not fit ends in `…`; tool headers keep the
  glyph and name. Nothing wraps a header onto a second row.

## Deliberately absent

Splash art. Pixel wordmarks. Taglines. A row of tips. Fading dividers.
Spinner verbs in the house voice. Idle key hints. The working directory on
the status line. Emoji. Boxes around messages. Background fills, including
on diffs, the prompt, the rail, and selection. A second accent. A startup
query to the terminal to ask what color its background is. A percent that
was not in the output.

## Indicators

Each wait has its own glyph, in the dim accent, one cell or three. The tick
is about 100ms.

- **Thinking, and the streaming tail.** Braille `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`.
- **A tool or a shell command.** A thin line, `│╱─╲`.
- **A subagent on the rail.** A pulsing dot, `● ◉ ○ ◉`, half speed.
- **Waiting on a connect or a rebuild.** A dim ellipsis, `.  ` `.. ` `...`,
  every four ticks. Reduced motion is `…`.
- **A background command.** A tiny orbit, `◜◠◝◞◡◟`, beside the task count.
- **Compaction with no chunk count.** A one-cell `━` walking a dim `─` track.

`NO_COLOR` drops the hue and keeps the glyph. Reduced motion freezes the
first frame of each family.

## Progress

Where a number exists, the bar is determinate: `━` filled in the dim accent
over a `─` track, one line, no fill.

- **Compaction.** One chunk of the summary is one step. The status line is
  `compacting 182k  2/5` plus the bar. When the chunk count is not known yet,
  the line is the sweep, not a made-up percent. When the pass finishes and
  the size before it was known, the transcript says `compacted 182k → 41k`.
- **A build or a long command** (`cargo`, `npm`, `pytest`, and the same
  family). `Compiling n/m`, a percent on a build line, and `running N tests`
  plus the `ok` / `FAILED` / `ignored` lines become `done/total` beside the
  bar and the elapsed time. A command that looks like a build but has not
  printed a fraction yet gets the sweep. Ordinary prose (`1/2`) is not a bar.
  The line replaces the running command's tail, and the last line of a
  background task on the rail.

## Multiplexers

tmux (`$TMUX`), screen (`$STY`), and zellij (`$ZELLIJ`) are detected when
the TUI starts. tmux is asked for `prefix` and `prefix2` (`tmux show -gv`).
Screen is asked for its escape; the default is Ctrl-A. Zellij has no prefix
unless `[ui] mux_prefix` sets one. `$TMUX` wins over `$STY`, because tmux's
`TERM` is often `screen`.

The app still handles the single key the multiplexer forwards. The hint is
what changes: the binding is the prefix twice, which is what `send-prefix`
delivers. Detection does not run in tests; only the real TUI installs it.

Inside tmux or screen, `COLORTERM=truecolor` is often the outer shell's.
Truecolor is used only when tmux advertises `RGB` or `Tc` (terminal-features,
terminal-overrides, or default-terminal), or when `TERM` itself says
truecolor. Otherwise the palette is 256, or 16 when `TERM` is not a 256-color
name. Zellij and a bare terminal trust `COLORTERM`. `NO_COLOR` still wins,
and `mux = "off"` does not turn color back on.

The frame is one synchronized update (`CSI ? 2026`). Bracketed paste and
focus events are enabled so a paste is one string and a focus sequence is
not typed into the composer. Mouse capture is on, which is what click-to-expand
uses; Wizard does not change tmux's `mouse` option, so clicks need `mouse on`
in tmux. OSC 52 clipboard writes are wrapped in tmux's DCS passthrough (and
screen's), so a copy still reaches the outer terminal. Other OSC sequences
the terminal does not know are left for it to ignore.
