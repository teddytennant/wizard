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
  focused name. Green and red are for diffs and pass/fail; yellow is a
  warning. Nothing else takes a hue. Emphasis elsewhere is bold or brightness.
- **Meaning never rests on color.** Every state also reads through a glyph or a
  word (`●`, `✕`, `+`, `-`, `exit 2`), so `NO_COLOR`, 16 and 256 colors show
  the same facts.
- **No backgrounds.** Never paint a background color. Every cell stays
  `Color::Reset`, so a transparent, blurred, light, or dark terminal shows
  through. The prompt band, the rail, and user rows get no fill. Selection is
  reverse video. Focus is accent text or bold, not a slab. There is no page,
  raised, or sunken tone.
- **Separate with space, or one dim rule.** A single faint `─` sits above the
  composer, because the transcript and the draft scroll differently. A wide
  terminal may put a faint `│` down the side rail. No second rule, no fading
  divider, no boxes in the transcript. Floating layers (pickers) get one
  border, because a picker has to read as being on top.
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
- **Say what is happening.** A waiting turn is a spinner and a plain word:
  `thinking`, `writing`, or `running cargo` — the thing actually in flight.
  Cute verbs (`Conjuring…`) are not the house voice. A custom
  `[ui] spinner_verbs` list is still honored, because that is a setting the
  user wrote down. While text is streaming there is no spinner at all, only
  a dim `▍`.
- **Fast.** No animation delays paint. Nothing waits on a spinner frame.
  `WIZARD_REDUCED_MOTION=1` freezes the spinner on `◆`. The screen does not
  probe the terminal at startup (an OSC query hangs the TUI), so light versus
  dark is the user's background showing through, not a detected theme.

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
- **Streaming text** renders as markdown as it arrives, with a dim `▍` at the
  tail.
- **Tool call**: one header row, `● execute  ls -la  0.4s`. The glyph is the
  state (spinner running, `●` done, `✕` failed), the name is accent, the
  argument is dim, the elapsed time is dim and left off under 100 ms. A
  non-zero exit puts `exit 2` on the header. The output hangs below on a `╰`,
  dim. A running command shows its tail; a finished one its head, then
  `… +N lines`; a failed one stays open.
- **Compact view** (on by default): a turn's tools are one line,
  `▸ ran 3 commands  edited 2 files`. Enter or a click opens it (`▾`). The
  full view is Ctrl-O or `/view`, and the choice is saved.
- **Diff**: the file name once, on the tool header. The body is `-` and `+`
  in the diff colors — the sign and the text, not a filled row. Only the
  words that changed are bold and underlined, and only when they are a
  minority of the line. A line that is mostly new stays plain colored text.
- **Errors**: `✗` then the message, bold, in the transcript where it happened.
  No red panel, no box.
- **Composer**: a dim rule, then `❯ ` and the draft. Grows to ten rows, then
  scrolls. No fill behind the draft.
- **Side rail** (120 columns and wider, and only when there is something to
  put there): a right column, about a quarter of the width, split off by one
  faint vertical rule. Subagents (name, elapsed, what they are doing, steps
  as `n/budget`), background commands (command, elapsed, the last output
  line), and todos. Text wraps inside the rule, indented, and never back
  onto it. No fill. Under 120 columns this collapses to the
  one-row summary above the status line, and todos stay in their band.
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
query to the terminal to ask what color its background is. Progress bars for
things with no known total, except a subagent's step count toward its
budget, and compaction, which is one opaque call and needs to show it is
alive.
