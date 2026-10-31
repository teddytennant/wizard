# UI skins

Wizard's TUI can wear another coding agent's terminal chrome. Seven ship:
`wizard` (the house look), `codex`, `grok`, `opencode`, `pi`, `jcode` and
`claude`. `wizard`, `codex` and `grok` draw inside this process; `codex` and
`grok` also have a full look, and the other four exist only as one. A full look
is its own binary (`wizard-ui-<name>`, built from the cargo workspace in
`looks/`) that `/ui` restarts into, talking to `wizard acp`. Pick one at
onboarding, change it live with `/ui`, or cycle the **Interface** row in
`/settings`. See [usage.md](usage.md#the-interface) for the user-facing half.

This document is the implementer's half: how the layer is built, and where the
borrowed parts came from.

## What a skin controls

A skin is a static table (`Chrome` in `src/skin/mod.rs`), not a trait object —
switching one is a pointer swap and nothing allocates per frame. It owns:

| | |
|---|---|
| **Blocks** | Per transcript-entry kind (`user`, `assistant`, `thinking`, `tool`, `notice`): the accent column, the left/right pads, the vertical padding, the background tint, and the marker leading the content. `src/skin/layout.rs`. |
| **Tool cards** | The status glyphs, the label grammar (`bash …` or `Ran …`), and the arm the output hangs off (`└`). |
| **Composer** | Framed with rules, a box, or nothing; the prompt glyph. |
| **Welcome** | Which of three home screens. |
| **Status line** | The separator, the busy phrasing, the idle hint. |
| **Spinner** | The frame sequence and its length. |

It does **not** control what Wizard is. The commands, the model, onboarding,
and everything the status line reports (mode, the `ULTRA ×N` multiplier,
background subagents, the context meter) are Wizard's under every skin. The
skin layer is deliberately given no way to add, hide, or rename a command.

## Slash commands in the full looks

Every full look has the same `/` menu. It is what `wizard acp` advertises
(the built-in commands, your own commands and the skills), plus eight the look
answers itself because they need its pickers, its process or its screen:
`/clear`, `/exit`, `/quit`, `/help`, `/model [tag]` and `/resume [id]` with the
house TUI's meaning, `/export [file]`, which writes the conversation as
markdown (to `wizard-<session>.md` in the project when no file is named, and
never over a file that is already there), and `/settings`, which opens the
look's settings (grok's settings window, pi's settings menu) or says the look
has none. Their table is `agent_core::commands` in `looks/`. Anything else
typed as `/name` goes to wizard as typed, which runs it or answers
`Unknown slash command`, as the house TUI does. A look may
draw wizard's answer its own way, and with no argument `/effort`, `/mode` and
`/rewind` open a picker in the looks that have one (opencode: effort and mode;
codex: rewind; grok and pi: effort; claude: all three); elsewhere wizard
answers with the choices.

On top of that, each look keeps a few commands that only change how it draws
or takes input:

| Look | Its own commands |
|------|------------------|
| `opencode` | `/theme`, `/timestamps`, `/thinking` (show or collapse thinking), `/copy` |
| `codex` | `/theme`, `/raw`, `/vim`, `/statusline`, `/title`, `/keymap`, `/copy` |
| `grok` | `/theme`, `/timestamps`, `/compact-mode`, `/scroll-keys` (vim keys in the scrollback), `/multiline`, `/copy` |
| `pi` | `/scoped-models`, `/copy` |
| `jcode` | `/cls` (clear the view, keep the conversation) |
| `claude` | `/vim` (editor mode, kept in `~/.claude/settings.json`) |

`/copy` copies the last response in every look that has it. A test in
`src/commands/mod.rs` fails when a look's text names a `/name` that is none of
these.

Colors come from the skin's own palette (`Skin::companion_theme`, see
[usage.md](usage.md#color)). There is no separate theme setting: picking the
skin picks the palette.

## The block model, and why the order matters

The transcript used to be a flat list of lines with a two-column marker glued
to the front of each *logical* line, wrapped afterwards. A paragraph that
soft-wrapped therefore lost its marker on every row but the first. That is
invisible when the marker is two spaces, and fatal when it is a colored bar
down the side of a block.

So the order is now the one both open upstreams use: **wrap the content to the
content width first, then decorate every row that came out.**

```text
│A│PL│         content          │ PR │
│1│ 2│           flex           │  2 │
```

`decorate` in `src/skin/layout.rs` puts the accent column on every row *including
the vertical padding*, the marker on the first content row and its continuation
on the rest, and carries a tint to the block's full width so it is a rectangle
rather than the ragged shape of its text.

## Background tints

`codex` and `grok` sit blocks on slabs. A slab is only meaningful relative to
the background under it, so `src/skin/blend.rs` computes it as an **alpha over
the terminal's own background** rather than a fixed color: `Tint::Raised` lifts
off a dark terminal and settles onto a light one.

The terminal background comes from `WIZARD_BG` (an `#rrggbb` you set) or from
`COLORFGBG`. **When it cannot be known, nothing is tinted** — that is the
correct answer rather than a fallback, and it is what Codex does too. Wizard's
own `wizard`/`minimal` pairing never asks for a slab in the first place, so the
house look is unaffected either way.

## Attribution

Both borrowed looks come from open source, and code ported from them is
marked at each site. Both are Apache-2.0; Wizard is MIT, and Apache-2.0 permits
this with attribution, which is what this section is.

The attribution lives here and in `NOTICE`, not on the screen. A borrowed skin
is meant to be indistinguishable from the product it borrows from — the only
things on it that are Wizard's are its name, its commands, and the state its
own backend reports — so a credit line rendered into the welcome screen would
be one more row upstream does not have.

### OpenAI Codex — <https://github.com/openai/codex> (Apache-2.0)

Ported from `codex-rs/tui/src/`:

| Upstream | Here |
|---|---|
| `color.rs` — `blend`, `is_light` | `src/skin/blend.rs` |
| `style.rs` — `user_message_bg_rgb`'s alphas (white at 0.12 over dark, black at 0.04 over light) | `src/skin/blend.rs::Tint` |
| `shimmer.rs` — the sweeping band: raised cosine, half-width 5, 10 columns of lead-in/out, and the DIM/normal/BOLD fallback | `src/skin/motion.rs::shimmer` |
| `render/line_utils.rs` — `prefix_lines`, and the wrap-then-prefix order | `src/ui/mod.rs::prefix_rows` |
| `exec_cell/render.rs` — the `  └ ` / `    ` output arm and the head/tail elision | `src/ui/mod.rs::tool_card_lines` |
| `bottom_pane/chat_composer.rs` — the unframed composer, `LIVE_PREFIX_COLS = 2`, the `›` prompt (bold *and* dim) | `ComposerFrame::Bare` |
| `status_indicator_widget.rs` — `Working (… • esc to interrupt)` and its punctuation | `BusyStyle::Working` |

### xAI Grok Build — <https://github.com/xai-org/grok-build> (Apache-2.0)

Ported from `crates/codegen/`:

| Upstream | Here |
|---|---|
| `xai-grok-pager/src/scrollback/layout.rs` — the accent/pad/content/pad column structure and its widths (left 2, right 2 — the doc comment there says 1 and is stale) | `src/skin/layout.rs` |
| `xai-grok-pager/src/scrollback/scrollback_pane.rs` — the `┃` accent column painted down a block's full height | `src/skin/layout.rs::decorate` |
| `xai-grok-pager-render/src/theme/tokyonight.rs` — `wave_brightness`, `pulse_brightness` | `src/skin/motion.rs::wave`, `::pulse` |
| `theme/groknight.rs` — the neutral-gray base with violet/steel accents | `assets/themes/grok.toml` (approximated in xterm-256) |

Two deliberate divergences from Grok Build, both for the same reason — the
house rule that meaning never rests on hue alone, which is what keeps the TUI
readable at 16 colors and under `NO_COLOR`:

- Upstream carries tool status in color alone (the rail and a `◆` bullet).
  Wizard keeps a distinct glyph for **failure** and only for failure.
- Upstream tints tool-output panels per line. Wizard leaves them untinted,
  since the terminal background is often unknown and a half-applied panel is
  worse than none.

### A note on closed-source agents

No code from a closed-source TUI is copied here or in `looks/`. The `claude` look
is built from its observable screen alone: `looks/crates/claudew` drives the
real program and the look through the same keys in a private tmux server and
diffs the two cell by cell (`crates/claudew/tools/compare.sh`). Nothing from
that program's install is read, copied or redistributed.

The look reads `editorMode` and `keybindings.json` the way Claude Code does, and its permission prompt
offers "switch to auto mode" (wizard's `sovereign`) with the tip above it, as 2.1.314 does; see
[claude-code-parity.md](claude-code-parity.md#keys-and-the-prompt). `looks/crates/claudew/tools/pix.py`
takes a real pixel screenshot of a tmux session (a headless Hyprland output, foot, grim), and `drive.py`
takes one of every scene named in `PIX_SCENES` into `PIX_DIR`, so the look and `claude` can be compared as a
person sees them. `vimdiff.py` is the vim-mode counterpart of `compare.sh`.

The look draws Claude Code's mascot (three rows of block characters, nine columns)
next to the name Wizard; the header text is Wizard's own.

To capture the reference without an account, run `REF=fake crates/claudew/tools/compare.sh`.
It starts `tools/fake_anthropic.py`, an Anthropic Messages endpoint that answers the
scenes' fixed prompts with scripted tool calls, and runs `claude` against it in a `HOME`
of its own, so nothing reaches a subscription or `~/.claude`. `REF_GROUPS="turn3 shared"`
captures only those scene groups. What it cannot give is the subscription's own screens
(plan name, weekly-limit notice, account-specific commands and models), so its captures are
for finding differences, not for replacing `tests/fixtures/ref`. The installed `claude` updates itself (it moved to
2.1.316 while the fixtures were being retaken), so a capture meant for `ref-2.1.314` sets
`CLAUDE_BIN=~/.local/share/claude/versions/2.1.314`.

The `claude` look's scene groups are in `tools/drive.py`. Besides the ones for the home screen, turns,
permissions, pickers and vim, `statusline` and `statusline-pad` run a test `statusLine` command
(`tools/statusline-a.sh`, `statusline-pad.sh`; `compare.sh` writes the project settings for them), `todo` and
`todo-many` drive the task list, `plan2` approves a plan and `plan3` turns one down with typed feedback.
`compare.sh` compares against
`tests/fixtures/ref-2.1.314`, a capture of the installed 2.1.314 through the fake server; `REFDIR=tests/fixtures/ref`
compares against the older 2.1.300 subscription capture instead, which `cargo test` still uses for the scenes both
have. `REF=fake REF_BASE=tests/fixtures/ref-2.1.314 REF_GROUPS="todo" LOOK_GROUPS="todo"` redoes one group over the
stored capture, `REF=fake SAVE=1` regenerates the whole of `tests/fixtures/ref-2.1.314` (it leaves `ref` and the tests' replay alone), and `COMPARE_CWD` moves the project directory (it must keep the length) when two runs share the box.

### Timing in the captures

Several things on the `claude` screen depend on the clock, and a capture taken at the wrong moment differs
for no reason the look could fix. What 2.1.314 does, found by leaving it alone with the fake server and
taking a capture every second:

- The `◐ medium · /effort` notice sits right-aligned above the prompt for ten seconds after the session opens, and
  for ten seconds after a panel closes (`/model`, `/resume`, `/help`) or a permission prompt is
  answered. A turn does not bring it back, typing and `ctrl+c` do not, and neither does a resize.
- `← for agents` leaves the mode line ten seconds after the session opens, and `/clear` does not bring it back.
- The first time shift+tab reaches auto mode it prints a long explanation of it in the transcript. The look has no
  such text, because Wizard's modes have no classifier to describe (`modes-3`, `modes-4`).
- The `backslash (\) + return (⏎) for newline` entry of the shortcuts table becomes `\⏎ for newline` once a
  newline has been typed that way. The look keeps that in a flag file in its state directory.

`drive.py` waits for these before the scenes that need them to be absent (`('since', N)` counts from the process
start, a `wait` from the scene), so the look, which starts faster than `claude`, and the reference are in the same
state when they are captured. `turn-spinner` relies on `fake_anthropic.py` holding its answer after the shell
command for three seconds (`FAKE_BASH_PAUSE`).

### Where the look stands against 2.1.314

`compare.sh` counts the cells that differ after `explained.tsv` has taken out what is a real difference of data
(the plan name, the model list, the usage numbers, Wizard's own commands and keys). Every row there has a reason.
Unexplained cells in the last full run, `static light turn turn2 turn3 plan2 plan3 todo todo-many think statusline
statusline-pad wizard` at 80x24, 120x40 and 200x50:

| Scene group | Before | After |
|---|---:|---:|
| Home, shortcuts, prompt and menus (32 scenes) | 1,860 | 0 |
| Model picker (5) | 4,115 | 0 |
| Resume picker (2) | 806 | 0 |
| Modes (6) | 167 | 0 |
| Permissions: bash, edit, write (5) | 1,271 | 0 |
| Turns and transcript (16) | 1,200 | 0 |
| Plan: dialog, approval, feedback (6) | 399 | 0 |
| Task list (6) | 0 | 0 |
| Status line (6) | 0 | 0 |
| **Unexplained cells** | **9,818** | **0** |
| Rows laid out differently with Wizard's own data | 75 | 12 |

The 12 rows are `wz-modes-3` and `wz-modes-4` at 80x24, where the reference's auto mode explanation scrolls its
transcript up a row; the layout check has no way to say that.

With a real `wizard` behind the look (`WIZARD_BIN=... compare.sh`, `live.py` against `fake_openai.py`) 0 checks
fail, among them the three-answer plan dialog and the typed feedback reaching the model as
`plan rejected: use tabs`, and the shared scenes compared with the reference have 0 unexplained cells.

Before this work the same run counted 9,818 unexplained cells: the model picker 4,115, bash and edit permissions
1,271, the home screen and menus 1,860 (the shortcuts table at 80x24, the help panel at 200x50), turns 1,200,
the resume picker 806, the plan dialog 399, modes 167. What changed was the look in most cases: the 2.1.314
model picker (descriptions wrapped under their names, a window of `(height - 17) / 2` models, the default
model's name in the brand colour), the three-answer plan dialog, bold phrases in the permission answers, a tip that
wraps, the shortcuts table's newline entry, the notice and hint timing above, a prompt scrolled half off the top
of the screen keeping its tail, and the task spinner naming the task's `activeForm`. The rest was captures taken
at a moment that happened to differ, which are retaken with the waits above (the reference groups `static`,
`light`, `turn`, `turn2`, `turn3`, `think`, `plan2` and `todo` were captured again, so Before and After are not
cells of the same files). In `explained.tsv` ten rows that described the old two-answer plan dialog and a stale
scroll were removed, and rows were added for the auto mode explanation, the command search ranking, the clock
bound spinner colours and bullet, the plan file path and Wizard's own model panel; each says why.

Product names and trademarks belong to their owners. The skins are an homage,
and each one says whose look it wears on its own home screen.
