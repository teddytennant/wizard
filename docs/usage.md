# Usage

Day-to-day reference: the TUI's slash commands, the `wizard agents`
dashboard, the subagent rail, and the core mechanics (token usage, todos,
project instructions) that work identically in every mode (genie TUI,
sovereign headless, perpetual, gateway).

## Slash commands

Everything typed as `/command` in the TUI. Tab-completion lists these with
inline hints.

| Command | What it does |
|---------|--------------|
| `/help` | List the available commands |
| `/clear` | Clear the conversation |
| `/model [tag]` | Show the current model, or switch to `tag` |
| `/mode [genie\|sovereign]` | Show or switch personality mode (`/genie` and `/sovereign` are shortcuts) |
| `/effort [low\|medium\|high\|default]` | Set reasoning effort for models that support it (xAI Grok 4.x, OpenAI o-series/gpt-5); no argument opens the picker, `default` clears to the provider default |
| `/plan` | Toggle plan mode (also Shift+Tab): read-only investigation until a plan is approved |
| `/omakase` | Toggle omakase: chef's-choice plan mode, the agent decides and auto-approves its own plan ([modes.md](modes.md)) |
| `/evolve [--deep] <desc>` | Self-extend: add a skill, MCP server, scripted tool, or subagent; `--deep` rebuilds the binary ([evolve.md](evolve.md)) |
| `/reload` | Reload skills, scripted tools, and MCP servers without a restart |
| `/rewind [turn]` | Restore file checkpoints and truncate history; no argument opens the turn picker ([checkpoints.md](checkpoints.md)) |
| `/resume [id]` | Reopen a past session and continue it; no argument opens the session picker |
| `/resume-claude [id]` | Continue a conversation from Claude Code's own history; no argument opens the picker. Imports a copy — `~/.claude` is only ever read |
| `/compact` | Summarize older history into a progress note now, instead of waiting for the automatic threshold |
| `/btw <question>` | Ask a quick side question against the current conversation without adding the exchange to history or the session file (token-cheap asides mid-task; works while a turn is running) |
| `/fork <task>` | Spawn a background side quest that inherits the full conversation context (history, tools, system prompt). Runs in parallel without interrupting the main session; its report is injected into history when finished (works while a turn is running) |
| `/agents` | Browse the subagent roster; Enter pre-fills a delegation request |
| `/dashboard` | Toggle the machine-wide session manager, same view as `wizard agents` (below) |
| `/bashes` | List background tasks (`execute` with `run_in_background`), running and finished ([tasks.md](tasks.md)) |
| `/rail [dismiss [name-or-id]]` | List the subagent rail, or take finished rows off it. Running rows stay |
| `/goal [text]` | Show or set the standing mission goal (drives sovereign/continuous mode; persists to `.wizard/mission.toml`) |
| `/diff` | Toggle the git diff sidebar |
| `/todos` | Toggle the todo list above the input |
| `/cost` | Session token usage, with cost estimates when per-provider rates are configured |
| `/memory [read\|forget <name>]` | List the saved project memories, show one, or forget one ([memory.md](memory.md)) |
| `/status` | Session status: model, provider, mode, effort, session id, usage, todo progress, background tasks, plan/omakase, ultra (GUI also prints current context tokens) |
| `/doctor` | Environment diagnostics, same checks as `wizard doctor` ([doctor.md](doctor.md)) |
| `/provider …` | Add, remove, or switch LLM providers; no arguments opens the interactive menu |
| `/fusion [config]` | Toggle model fusion, or configure the panel ([fusion.md](fusion.md)) |
| `/ultra [config]` | Toggle mixture of agents, or configure the roster ([ultra.md](ultra.md)) |
| `/server [status\|start\|stop]` | Manage the local llama-server |
| `/login <provider>` | OAuth sign-in; the TUI command accepts `xai` only. ChatGPT OAuth is `wizard --login chatgpt` from the shell, or the GUI Settings flow |
| `/publish [branch]` | Fork Wizard to your GitHub and get a one-line installer ([market.md](market.md)) |
| `/settings` | Open the in-app settings menu |
| `/ui [name]` | List the available interfaces, or wear one: `wizard`, `codex`, `grok` ([The interface](#the-interface)) |
| `/vim` | Toggle modal (vim-style) editing of the input composer |
| `/quit` | Exit Wizard |
| `/exit` | The same thing, under the other name people type. `/q` is accepted too, though only `/quit` and `/exit` are offered by tab-completion |

Your own commands (markdown files that expand into prompts) sit alongside
these; see [commands.md](commands.md), which also covers `@path` file
references.

### Agent-run slash commands

The agent can run these same commands itself with the native `run_command`
tool, it passes a command line exactly as you would type it (e.g.
`/effort high`, `/model claude-sonnet-5`, `/reload`). So the agent can raise
its own reasoning effort for a hard task, switch models, or reload skills
without you stepping in. For context pressure, prefer the native **`compact`
tool** over `run_command` → `/compact`: `compact` runs mid-turn on every
surface (including headless and the gateway), while a queued `/compact` only
runs after the current turn finishes and only on interactive surfaces. See
[Agent-managed context](#agent-managed-context).

Because a turn already in flight can't be reconfigured, a queued command runs
the moment that turn finishes, effort, model, and mode changes therefore take
effect on the **next** turn.

The agent's set is an **allowlist**, not "everything minus a few": it may run
`/model <tag>`, `/mode <mode>`, `/effort <level>`, `/goal`, `/diff`, `/todos`,
`/dashboard`, `/cost`, `/memory`, `/doctor`, `/status`, `/bashes`, `/compact`,
`/reload`, `/plan`, `/omakase`, `/settings`, `/vim`, `/help`, and `/fusion` and
`/ultra` as bare toggles. Everything else is refused with a note the agent
sees, and the refusals are broader than the obvious ones: anything that would
park the agent at an interactive picker (`/model`, `/mode` or `/effort` with no
argument, `/fusion config`, `/ultra config`, `/agents`), that ends or rewinds
the session (`/quit`, `/clear`, `/rewind`, `/resume`), that sets up providers
or rewrites the binary (`/provider`, `/login`, `/publish`, `/evolve`), that
manages the local server (`/server`, all subcommands including `status`), that
repaints your terminal (`/ui`), or that peels off another thread the agent
has native tools for (`/btw`, `/fork` — `spawn_subagent` is the agent's route).

Only the interactive surfaces (TUI and GUI) apply these commands; the tool is
refused outright in headless `-p` runs, the gateway, ACP, and subagents
(including `/ultra` lenses), nothing is silently dropped. The GUI narrows the
set further, since `/vim`, `/ui`, `/quit` and `/exit` are terminal-only
there.

### Queued user messages

While a turn is running you can keep typing and press **Enter**. The message
lands in the transcript immediately, is announced with a "queued (will send
after this turn)" notice, and runs automatically once the current turn finishes
(after any slash commands the agent itself queued via `run_command`). Multiple
messages stack FIFO; the status bar shows `queued N` while any are waiting.
The queue is capped (32); overflow keeps the composer text so nothing is lost.
`/clear` and Ctrl-C interrupt both drop the queue, a cleared or interrupted
conversation shouldn't auto-fire prompts that no longer apply.

### `/fork` vs `/btw` vs `spawn_subagent`

Three ways to peel work off the main thread, each with a different contract:

| | Context | Tools | Lands in history? | Who starts it |
|---|---|---|---|---|
| `/btw <question>` | Full conversation (snapshot) | None (one-shot answer) | No (token-cheap aside) | You, mid-turn OK |
| `/fork <task>` | Full conversation (snapshot) | Parent's tools minus five: `spawn_subagent`, `run_command`, `exit_plan`, `interview`, `compact` | Yes, report injected when the fork finishes | You, mid-turn OK |
| `spawn_subagent` | Only the `task` string you write | Chosen subagent's scope | Yes, same background drain as `/fork` | The agent |

`/fork` is the "true parallel agent" path: you stay in the main thread, the
fork inherits everything it needs with zero re-explanation, and its findings
come back as a system note the main model sees on its next turn (or on the
idle drain if you're between turns). Watch it on the subagent rail below the
composer (↓ focuses it); kill it from there the same way as any other
background subagent.

## The interface

The TUI can wear another coding agent's chrome. `/ui` lists the three that ship
and `/ui <name>` switches immediately:

| Interface | Look |
|-----------|------|
| `wizard` (default) | The house look: the braille wand-and-spark mark on the home screen, dim rules around the composer, `❯` for you and `·` for the agent, a chip-separated status line |
| `codex` | OpenAI Codex's: a `>_` banner, `›` for you and `•` for the agent, `Ran <cmd>` headers with a `└` output arm, no composer frame at all, and `Working (step 3 • 12s • esc to interrupt)` |
| `grok` | Grok Build's: a `┃` bar down the left of every block, colored by whose block it is, a boxed composer, and `Thinking… step 3 · 12s` |

**A skin is a look, and only a look.** The commands stay Wizard's (`/model`,
`/fusion`, `/ultra`, `/publish`), onboarding is Wizard's, the provider
and the keys are yours, and the status line still reports Wizard's own state —
mode, the `ULTRA ×N` multiplier, background subagents, the context meter —
under both. Wearing Codex's chrome does not give you Codex. The home
screen says whose look it is for that reason.

`/ui` **persists**: it writes `[ui] skin` to `~/.wizard/config.toml`, so the
choice survives a restart. Onboarding asks the same question on a first run.
Resolution order at startup is **`[ui] skin` in `config.toml`, then
`WIZARD_SKIN`, then `wizard`**; a blank value at either level counts as unset.

A skin also brings its **palette** — `codex` wants cyan, `grok` wants violet
on gray — and switching always brings it along. There was once a separate
`[ui] theme` key, above a `WIZARD_THEME` variable, so a palette could be set
independently of the chrome around it; in practice its main effect was that
`/ui codex` drew Codex's frame in the previous skin's colors for exactly the
people who had bothered to set one. Chrome and palette now travel together.

Two house rules survive every skin, because they are what keeps the TUI
legible when the terminal is not: **no background colors** (everything paints
on the terminal's own background, so Grok Build's tinted blocks become the bar
alone) and **no meaning carried by hue only** (glyphs and brightness say it
first, so the skins still read under `NO_COLOR` and on a 16-color terminal).

Implementation notes and the full attribution live in
[ui-skins.md](ui-skins.md).

### Where the looks come from

Both borrowed looks come from Apache-2.0 sources —
[openai/codex](https://github.com/openai/codex) (`codex-rs/tui`) and
[xai-org/grok-build](https://github.com/xai-org/grok-build)
(`crates/codegen/xai-grok-pager*`) — and the algorithms ported from them are
credited at each site. [ui-skins.md](ui-skins.md) has the file-by-file table,
and `NOTICE` carries the licence attribution. Trademarks belong to their
owners. A skin reproduces the chrome and nothing else: the name on the screen,
the commands, the model and every fact reported on it are Wizard's.

## Copying text out

Two ways to copy, one machinery behind both:

- **Drag** across the transcript. The covered text is copied when you release.
  Wizard captures the mouse so the wheel scrolls, which pre-empts your
  terminal's own click-drag selection, so this is the replacement for it.
  Holding **Shift** while you drag falls back to the terminal's selection.
- **Ctrl-Y** copies the last reply, whole. This is the one to use for an
  answer: a drag only copies what is on screen, so anything scrolled off the
  top is not in it, and there is nothing to drag with over a serial console or
  a terminal that eats mouse events.

Every copy is attempted on **every route the terminal stack offers**, because
which one you will actually paste from is not knowable from inside Wizard:

| Route | What it sets | When |
|---|---|---|
| Native tool | The clipboard of the machine Wizard runs on, via `wl-copy` / `xclip` / `xsel` / `pbcopy` / `clip.exe` | Always locally. Over SSH only when there is a display; first in a local session, last in a remote one |
| tmux paste buffer | What `prefix ]` pastes, via `tmux load-buffer -w` | Inside tmux |
| OSC 52 | The clipboard of the terminal you are *sitting at*, wrapped in tmux's or screen's passthrough when one is in the way | Always, up to 74994 bytes of text |

They are not fallbacks for each other. Over SSH the native tool is the wrong
one: `xclip` on the server sets the server's clipboard, which nobody can see,
and exits successfully doing it. OSC 52 is the only route that can reach your
own machine. Inside tmux, the paste buffer is the route that is certain to
work.

**Inside tmux**, the load-bearing route is `tmux load-buffer -w`, which fills
the paste buffer and asks tmux to push the text out to the real terminal with
its own escape. A bare OSC 52 from an application does not get through: tmux's
default `set-clipboard external` ignores it, and the DCS passthrough Wizard
also sends has been gated behind `set -g allow-passthrough on` since tmux
3.3a. Turn that option on if you want the passthrough route too; nothing
breaks without it.

**Over 74994 bytes** the escape is skipped rather than sent, because terminals
drop an oversized OSC 52 in silence and a copy that reports success and pastes
nothing is the worst outcome available. Wizard says so on screen and the other
routes still run, so in tmux the text is in the paste buffer even when the
outer terminal got nothing.

## Color

The TUI paints from a table of **semantic tokens** (`accent`, `muted`, `error`,
`tool.running`, `diff.add`, …). Nothing in the renderer names a color; a
palette file says what each token looks like, so a skin changing the colors is
data, not code.

One palette ships per skin, and there is nothing to choose between them
independently — picking the skin picks the palette:

| Palette | Skin | Look |
|---------|------|------|
| `minimal` | `wizard` (default) | Monochrome base, one accent, rounded edges on floating layers, no background colors. Every value is already an ANSI-16 name, so it renders identically over SSH and on a serial console |
| `codex` | `codex` | Near-monochrome with one cyan, all ANSI-16 names, plain borders |
| `grok` | `grok` | Violet and steel on neutral gray |

A palette that does not load leaves you on `minimal` with a one-line warning
rather than costing you the TUI.

Color *depth* is still yours to set, and is a separate question from which
palette is in force: see `NO_COLOR` and `WIZARD_COLOR` below.

In a palette file a token value is a color name, an `#rrggbb` string, or a
palette index 0-255. An **unknown token key is an error**, not a silent skip.
`assets/themes/minimal.toml` in the repo is the complete list of keys.

### Color depth

Themes are authored at full depth and degraded on load to what the terminal can
actually render, so a 16-color terminal never receives an escape sequence it
would print as garbage. Detection, highest priority first:

| Signal | Result |
|--------|--------|
| `NO_COLOR` set and non-empty | mono |
| `WIZARD_COLOR` | Forced: `mono` (also `none`, `off`, `0`), `16` (`ansi`, `ansi16`, `basic`, `on`, `1`), `256` (`ansi256`, `8bit`), `truecolor` (`24bit`, `16m`, `rgb`). `auto` or anything unrecognised means "keep detecting". Parsing trims surrounding space and ignores case |
| `TERM=dumb` | mono |
| `COLORTERM` contains `truecolor` / `24bit` | truecolor |
| `TERM` contains `256color` / `direct` | 256 colors |
| anything else, including no `TERM` at all | 16 colors |

The numeric spellings follow the neighbouring conventions rather than inverting
them: `WIZARD_COLOR=0` is off and `WIZARD_COLOR=1` is on (16 colors), the same
way `CLICOLOR` and `FORCE_COLOR` read those values. To capture TUI output
without escape sequences use `WIZARD_COLOR=mono` (or `0`, or `off`).

`NO_COLOR` is on top and **nothing overrides it**, `WIZARD_COLOR` included.
It is a cross-tool contract: you set it once, in a profile, to mean "no program
on this machine paints my terminal", and a program that lets its own variable
win over it is the one program that ignores the setting. So `WIZARD_COLOR=1`
alongside `NO_COLOR=1` is mono, not 16 colors. The escape hatch for a
`NO_COLOR` shell is to unset it for the one command, which the same convention
already provides, since an empty value counts as unset:

```bash
NO_COLOR= WIZARD_COLOR=truecolor wizard
```

`WIZARD_COLOR` is the escape hatch for the other problem, a terminal that lies
about itself in either direction, and it outranks everything below it: a
recognised value ends the decision there, before `TERM` and `COLORTERM` are
looked at. A value the table does not list (including `auto`) is not a forced
depth at all, so the sniffing below it gets its turn.

At 256 colors a truecolor value snaps to the nearest cube entry; at 16 it snaps
to the nearest ANSI color; at mono every token becomes the terminal's default
foreground. That last one is survivable because the UI never encodes meaning in
color alone: state also reads through bold, glyphs (`✓` / `✗`), and layout. The
"16 colors" fallback is the conservative guess for an unknown terminal, not a
degraded mode you should have to opt out of. `wizard doctor` reports the depth
it picked, which is worth checking first when the colors look wrong.

## Resuming a Claude Code conversation

`wizard resume` reopens the most recent Wizard session recorded against this
project, the subcommand spelling of `wizard --resume`. With `--claude` it takes
the conversation from Claude Code instead:

```bash
wizard resume --claude                 # list what Claude Code has here, pick one, continue it
wizard resume --claude --list          # just the listing, converts nothing
wizard resume --claude --session 0f2a  # take that session id (a unique prefix is enough)
wizard resume --claude --leaf <uuid>   # take a branch other than the one it left off on
```

Picking a session converts its history into a new Wizard session under
`~/.wizard/sessions/` and continues it in the TUI. Tool calls keep their real
provider ids, so the results stay bound to the calls they answer.

**`~/.claude` is read and never written.** It is another program's live state,
and a half-written line there would cost a conversation that cannot be got
back, so the reader is structurally read-only: a test scans its own source and
fails the build if a write API ever appears in it. The only file the import
creates is the Wizard session it is producing.

### It is a DAG, not a list

One Claude Code session is one `.jsonl` file, but reading that file top to
bottom does not give you the conversation. Every message line carries a `uuid`
and a `parentUuid`, and editing a prompt or rewinding does not rewrite the
file: it appends a *second* child under the same parent. A flat read therefore
interleaves branches that were never in the same conversation at all.

The conversation is the parent chain walked back from a chosen leaf. By default
that leaf is the tip Claude Code itself would resume from (`last-prompt`),
which is why `--list` reports how many times a session forked: on a branched
session the message count is smaller than the file, because the other branches
are not being offered. `--leaf <uuid>` takes one of them on purpose.

A chain can also stop before the root, which is normal after a `/clear` or a
compaction, and the import says so rather than pretending it has everything.

### What does not survive

- **Reasoning.** A thinking block carries a provider signature that is only
  accepted when it comes back untouched, and it was issued for another client's
  request on another account. Replaying it would fail the first turn of the
  resumed session with an error about a block you never knew was there.
- **Images inside tool results.** Wizard's tool-result block carries text. An
  image attached to a prompt does survive; one returned by a tool does not.
- **Turn boundaries.** Imported history has no checkpoint markers, so `/rewind`
  cannot cut into it. Everything after the import is normal.

A failing tool result keeps its "error:" prefix in the text, because Wizard's
result block has no separate flag to carry it and a failure that reads as a
success is how a resumed model concludes that a broken command worked.

### From the window

The window lists these sessions in the same picker as Wizard's own, and marks
which is which.

In the window (`wizard gui`) the chat list grows a **claude code** section under
the workspace groups, folded shut. It only appears when
Claude Code has actually recorded something for the directory a new chat would
open in, so a machine without it sees nothing. Its rows carry a hollow diamond
in the gutter where a Wizard chat carries a dot, and the word `claude` where a
Wizard chat carries its age — a shape and a word, not a colour.

Opening one is not the same act as opening a Wizard chat, and the fold says so
before the click (*opens as a copy · file untouched*). It imports: the chain is
walked back from that session's leaf, written as a new Wizard session, and the
new chat opens with a note saying how much came across and that Claude Code's
own file was read and not modified.

The section reads on open rather than on the sidebar's refresh timer, because
listing means parsing every transcript in the project — that is tens of
megabytes for a repository worked in for months. Moving the window to another
workspace drops the rows rather than relabelling them.

`/resume-claude` is the same thing in the TUI: the picker `/resume` opens, over
the conversations Claude Code recorded for the working directory, with the same
keys and the same import behind Enter. It is a separate command rather than a
row in `/resume` because opening one is a different act — a `/resume` row
reopens a file Wizard owns, and this one copies a conversation out of another
program. In the window, `/resume-claude` unfolds the sidebar's Claude Code
section, which is where those rows already live.

`wizard resume --claude` remains the shell spelling, and is still what a script
wants: `--list` prints the ids, `--session <id>` skips the picker, and `--leaf
<uuid>` picks which fork of a branched session to walk back from.

## `wizard agents` and background subagents

`wizard agents` opens the agent dashboard from the shell, the same view as
`/dashboard` inside a session. Every running Wizard session heartbeats a
record to `~/.wizard/running/`, so the dashboard lists every live session on
the machine, grouped under four headers: **Needs input**, **Working**, **Idle**
and **Completed**. A failed session is a fifth state but has no header of its
own — it sorts under **Completed** — so read the row, not the group, to tell a
clean finish from a crash. From it you can:

- **Dispatch** a new background session: type a prompt into the input at the
  bottom and it spawns a detached headless sovereign run (`wizard --bg`) that
  registers in the same dashboard and survives your session exiting.
- **Peek** at the selected session's recent transcript.
- **Stop** the selected background session (Ctrl-X).

Within a session, the agent delegates long-horizon work to subagents via
`spawn_subagent`. Its `background` parameter defaults to `false` — the tool
description is what pushes the model toward `background: true`, not the schema
— so whether a given run detaches is the model's call rather than a guarantee.
When it does detach, the turn returns immediately, you keep chatting, and the
subagent's report lands in context when it finishes. The status bar shows a
`⏵ N bg subagent(s)` marker while detached subagents run, and a
`⏵ N bg task(s)` marker while background `execute` tasks run (`/bashes` lists
those).

## The subagent rail

Every subagent run, foreground or background, gets a row on the rail, which
sits between the composer and the status bar. It costs no screen space until
something has been delegated.

```text
 ❯ ◉ researcher   web_fetch                            0:12
   ● reviewer     auditing the diff                    0:04 +3
   ✔ tester       all 214 tests pass                   1:31
```

A row is a status dot (pulsing while running, still `✔` or `✗` once it
finishes), the subagent's name, what it is doing right now (the tool in
flight by name, else its latest message), the elapsed clock, and `+N`: how
much it has done since you last looked at it. Five rows show at most, then a
`+N more` marker. Finished rows sit still. They do not blink and they do not
leave until you dismiss them.

Enter opens the selected run: that subagent's own conversation replaces the
main chat, its messages and its collapsible tool cards, drawn by the same
renderer, under a header naming the run.

```text
 ▌ researcher · running · 0:42 · 6 steps
   find the latest Tokio release notes  esc back · ↑↓ next agent
```

↑/↓ keep walking the runs once you are inside one, each takes over the screen
in turn, wrapping around, so browsing does not end when you open something.
Esc is only for leaving.

A foreground run is marked `· foreground` there: the parent turn is blocked
until it reports. The composer stays live while a pane is open, so you can
keep talking to the main agent while you watch one work.

| Key | What it does |
|-----|--------------|
| ↓ (in the composer) | Focus the rail, on the first row still going (the last row if nothing is) |
| ↑ / ↓ (on the rail) | Move between rows; ↑ off the top row returns focus to the composer |
| Enter | Open the selected row |
| Esc (in a pane) | Back to the main chat, focus in the composer |
| ↑ / ↓ (in a pane) | Open the previous / next run, wrapping around; with only one run, scroll it |
| Shift+↑ / Shift+↓ (in a pane) | Scroll the pane |
| PageUp / PageDown (in a pane) | Scroll the pane by ten lines |
| Ctrl-X | Kill the selected run (background runs only) or stop the selected command. On an already-finished pane, dismiss it |
| Backspace / Delete | Dismiss a finished pane. A running row is left alone |
| Any other key | Focus returns to the composer and the key is typed there |

## Background commands on the rail

Background commands share the rail with the subagents, below them and under the
same index, so `↓` walks out of the last agent and into the first command
without your having to know there are two lists. A command gets there two ways:
the model started it with `run_in_background`, or you pressed **Ctrl-B** while
it ran in the foreground (see `docs/interactive-commands.md`; under tmux, press
it twice).

Enter on a command opens its live output instead of a chat view — `↑`/`↓` and
PageUp/PageDown scroll it, following the tail until you scroll up, Ctrl-X stops
the command, and Esc puts the chat back. `/bashes` lists the same commands as
text.

A finished command rests on the rail for half a minute before retiring, longer
than a finished run does: a run's report lands in the main chat, so its row has
done its job once you see it go green, while a command's output lives only in
the registry and the row is the way back to it. The one you are watching does
not retire under you.

A finished run stays on the rail until you dismiss it (Backspace or Delete on
the row, Ctrl-X on a finished pane, or `/rail dismiss`). The agent can run
`/rail` and `/rail dismiss` the same way when you ask it to clear the rail.
Nothing is lost either way: a run's report is the output of the
`spawn_subagent` card in the main chat, which a background run writes back to
when it lands. Esc from an open pane returns you to the composer and leaves
the row where it is.

↓ only enters the rail when you are not part-way through input history, where
it keeps walking history. Any key the rail does not use returns focus to the
composer *and* is typed there, so a keystroke is never lost. Ctrl-X refuses a
foreground run, since the parent turn is blocked on it; Ctrl-C interrupts that
turn instead.

A subagent's own steps stay in its pane, so the main transcript holds only the
parent's `spawn_subagent` card and, for a detached run, the notice when it
reports back. Headless (`-p`) has no rail: there, subagent tool calls print
inline as `<name> ▸ <tool>` ([headless.md](headless.md)).

## Token usage and cost

Wizard accumulates the prompt/completion token counts every provider reports
on its final stream chunk.

- **TUI**: the status bar shows how many tokens the **next** model call will
  load into context (`12.3k tok`), the last reported prompt size, falling
  back to a char/4 estimate of the remaining history after `/clear` or
  `/compact`. It is *not* a session-lifetime sum (those double-count multi-step
  history and stay inflated after a clear). `/cost` still prints the full
  session prompt/completion breakdown.
- **Headless**: the final summary line includes the run's totals:
  `[run finished: Completed — 1234 prompt + 567 completion tokens]` (a run that
  reported no tokens at all prints just `[run finished: Completed]`).
- **Log**: every turn appends one JSON line to `~/.wizard/usage.jsonl`:

  ```json
  {"ts":1760000000,"project":"/home/u/proj","model":"claude-fable-5","provider":"claude","prompt_tokens":100000,"completion_tokens":1000,"cache_read_tokens":90000,"cache_write_tokens":0,"cost_usd":0.1263,"price_source":"table","mode":"genie"}
  ```

  `cache_read_tokens` and `cache_write_tokens` are **subsets** of
  `prompt_tokens`, never additions to it. Providers disagree about this on
  the wire — Anthropic reports its cache counts *outside* `input_tokens`,
  OpenAI reports them *inside* `prompt_tokens` — and each adapter reconciles
  to the subset form before the numbers reach the log.

- **Rollup**: `wizard usage` prints per-project and per-provider totals from
  that log (turns, prompt/completion/cached tokens, and cost); `--since 7d`
  limits the window. A cost followed by `*` was priced at the unknown-model
  fallback rather than a real rate — see below. The `cached` column is
  `cache_read_tokens` only: cache *writes* are billed and are in the cost
  figure, but no column shows them, so a cache-heavy turn's `cached` reads
  lower than the number of prompt tokens that got a non-standard rate.

`/cost` inside a session is a different, simpler path: it multiplies the
session's prompt and completion totals by the `usd_per_mtok_in` /
`usd_per_mtok_out` you configured on the active provider, and prints how to set
them when you have not. It does not consult the price table below and does not
know about cached tokens, so it says nothing at all on a provider with no
configured rates, and over-states a cached session on one that has them. The
log and `wizard usage` are where the priced numbers live.

### Where the cost figure comes from

Cost is settled once, when the turn's record is written, because that is the
only moment that holds the counts, the model that produced them and the
provider's configured rates together. `price_source` on each record says
which rate was used, in this order of precedence:

| `price_source` | Meaning |
|----------------|---------|
| `config` | `usd_per_mtok_in` / `usd_per_mtok_out` from your provider config. You typed these, so they win over everything else |
| `local` | A self-hosted backend (llama.cpp, Ollama). Tokens cost electricity, so `$0.0000` is the honest figure |
| `table` | Wizard's built-in list-price table matched the model id |
| `fallback` | A metered model the table does not know. Priced at the most expensive rate Wizard knows and flagged `*`, because an unknown model rendered as free would be the most misleading thing this column could say |

Cached tokens are priced at the vendor's published cached-input rate, which
is nowhere near uniform: Anthropic and OpenAI discount a cache read to 0.1x
input, xAI to 0.15x–0.2x depending on the model, and DeepSeek's disk cache to
about 0.008x. Where a vendor publishes no cached rate — or says caching is not
supported on that model at all, as Groq does for `llama-3.3-70b-versatile` — a
cache read is priced as fresh input rather than at a guessed discount, since
under-billing is the invisible error. Anthropic is also the only vendor here
that charges a *premium* (1.25x) to seed its cache; everywhere else the cache
fills automatically and a "write" token costs what an ordinary input token
costs. Billing a cached turn at the full input rate over-states it tenfold on
Anthropic and OpenAI and by more than a hundredfold on DeepSeek, which would
make prompt caching look worthless.

The table covers every model Wizard's own pickers offer from Anthropic,
OpenAI, xAI, Google Gemini, DeepSeek and Moonshot, and most of what the
Mistral and Groq presets offer: `devstral-2512` and `qwen/qwen3.6-27b` are the
two picker entries with no first-party rate published under that name, so they
fall back like anything else. On top of that, `gpt-oss-120b` is priced on each
of the four hosts that publish a rate for it (Groq, Together, Fireworks,
Cerebras — see below). Everything else, including OpenRouter's default
`openrouter/auto`, Cloudflare Workers AI, Z.AI and MiniMax, falls to
`fallback` on purpose: no per-token rate for them was found on a first-party
page, and a guessed rate flagged `table` would read as fact. Set the rates
yourself to override any of it:

  ```toml
  [[providers]]
  name = "claude"
  kind = "anthropic"
  # ...
  usd_per_mtok_in = 3.0
  usd_per_mtok_out = 15.0
  ```

**Open-weight models are priced per host.** A model id does not name a
seller: `gpt-oss-120b` is $0.15/$0.60 per million tokens on Groq, Together
and Fireworks and $0.35/$0.75 on Cerebras, and the three that agree on the
headline rate still disagree on cached input (Fireworks $0.015, Groq $0.075,
Together no published discount at all). So those ids are keyed by the API
host of the provider that served the turn, taken from its `base_url` — the
host is used because that is who invoiced the tokens, whereas the provider's
`name` is a label you can type. A host with no published rate for such a
model gets `fallback`, not the nearest host's number: a price that belongs to
someone else, flagged `table`, is the one error nothing downstream can spot.
Model ids that exactly one vendor sells (`claude-opus-5`, `grok-4.6`, …)
carry no host at all and price the same wherever you buy them.

One known limit remains: the lookup has no **per-request** prompt size, so
the xAI and Google models that bill a higher rate above a 200k-token
*request* carry their standard rate here. A genuinely long single request is
under-stated, because tiering on the turn total would over-charge every
ordinary multi-step turn instead.

Subagent runs and `/ultra` candidates are billed to the parent turn that paid
for them — tokens, cached tokens and all — so an ultra turn's cost line is
the whole fan-out, not just the main loop.

## Token-aware compaction

History compaction triggers on **either** the byte threshold
(`compact_threshold_bytes`, default 48 kB) **or** the last prompt exceeding
~80% of the model's context window, when the window is known:

- anthropic / openai / xai: static tables per model family
- llama.cpp: live `GET /props` probe for the loaded model's `n_ctx` (cached)
- ollama: the `num_ctx` its chat requests will actually carry — probe-derived
  and capped, defaulting to 16k — rather than the model's trained context
  length, because the server truncates the prompt at that value
- models none of the above recognises: byte threshold only

Compaction also runs *between steps inside a turn*, so a long tool loop
cannot overflow the window mid-turn. The newest messages that fit 40% of the
window stay verbatim (capped at ten), and the cut is allowed to land on an
assistant turn so an in-flight tool loop is actually folded instead of
walking back to the user prompt and summarizing one earlier note. The
summary is instructed to carry over the todo list state and the plan file
path (`.wizard/plan.md`).

## Agent-managed context

Wizard already persists every turn to `~/.wizard/sessions/<id>.jsonl` and
auto-compacts as above. Before each model step, when fill is elevated or
higher, an ephemeral `[context pressure]` line is injected so the agent sees
live headroom (not persisted to the session file). The agent is also taught,
via a block in its system prompt, to steward that window deliberately:

| Situation | What the agent should do |
|-----------|--------------------------|
| Long investigation, finished sub-goal, or older tool dumps drowning the current task | Call the native `compact` tool (mid-turn on every surface: TUI, GUI, headless, gateway). Summarizes older history into a progress note; recent tail stays verbatim. The note is also appended to the session JSONL so resume sees the breadcrumb |
| Pressure signal `elevated` / `high` / `critical` | Compact soon (`elevated`) or before more tool work (`high`/`critical`). Auto-compact still fires at the hard threshold |
| User pivots to an unrelated task | Save durable facts with `memory`, rewrite the todo list, then `compact`. Full prior transcript remains on disk as the session JSONL |
| New task must not see the old work at all | Ask the user for `/clear` (agent cannot run it). `/clear` rotates to a fresh session file; the previous JSONL is kept under `~/.wizard/sessions/` |
| Noisy multi-step work | `spawn_subagent` so intermediate steps never enter the parent context, only the final report does |

`run_command` → `/compact` still works on interactive surfaces but only
**after** the current turn ends; prefer the `compact` tool for in-turn
relief. Headless `-p`, the gateway, and continuous mode have the same
`compact` tool and pressure signal. Prefer compacting over asking the user
to clear whenever the prior thread is still useful as a summary.

## Todo list

The native `todo` tool lets the agent maintain a working todo list (action
`write` replaces the whole list of `{content, status}` items; `read` returns
it; statuses: `pending` / `in_progress` / `completed`). It is read-only for
the plan gate, so the agent can draft its list while planning.

- **TUI**: a compact band just above the input mirrors the list (`/todos`
  toggles it; it auto-shows on the first update). The band reserves layout
  space so it never covers chat text, the transcript shrinks above it.
- **Headless**: each update prints `≡ todo: 2/5 done — current: <item>`, or
  just `≡ todo: 2/5 done` when nothing is `in_progress`.
- **Subagents** get the tool too, with their own isolated list.

## Project instructions hierarchy

The system prompt's project-instructions section is assembled from every
directory between the filesystem root and the project root. In each
directory the first of `WIZARD.md` > `AGENTS.md` > `CLAUDE.md` wins, plus
the global `~/.wizard/WIZARD.md`. Files are concatenated outermost-first
(the project root's file has the last word), each prefixed with a comment
naming its path.

A line consisting of `@relative/path` inlines that file (one level deep,
~10 kB per include); the whole block is capped at ~40 kB.

A project's instruction file can only include files from its own directory
subtree. Instruction files are attacker-controlled the moment you clone a
repository, and their contents land in the *system* prompt, so an
`@../../.wizard/credentials.toml` line in someone else's `AGENTS.md` cannot
pull your keys in. Containment is checked after resolving symlinks and `..`, so
neither trick escapes it; a refused include is replaced by a comment naming it
rather than silently dropped. Your own global `~/.wizard/WIZARD.md` is exempt:
you wrote it, and confining it would only break your own includes.
