# Commands

Wizard's built-in `/commands`, the custom ones you define as markdown files, and the `@path`
tokens that inline file contents. All three live in `src/commands/`. The interactive surfaces
— TUI and the window — read the built-in table from there; headless `-p` shares
the custom-command and `@path` pipeline but does not parse built-ins (a `/word` typed there
goes to the model unless it names a custom command).

## Built-in commands

`COMMANDS` in `src/commands/mod.rs` is the single source of truth: what a command is called,
what it does, and how each surface runs it. The TUI's suggestion popup, the window's `/`
palette, and the allowlist the *agent* may invoke through its `run_command` tool are all
derived from it. Two hand-kept lists is how two surfaces drift into offering different
commands; there is one.

The TUI runs every command — it is the surface they were written against. The window is the
constrained one, and each command declares what it is there. Its column is still called
`gui`: it was the browser GUI's, and when that surface was deleted the window inherited the
column rather than growing a duplicate of it (`Surface::Gui`, `src/commands/surface.rs`).

| | command | the window |
|---|---|---|
| **Against the agent** | `/model`, `/mode`, `/genie`, `/sovereign`, `/effort`, `/plan`, `/omakase`, `/compact`, `/btw`, `/fork`, `/goal`, `/status`, `/cost`, `/memory`, `/doctor`, `/bashes`, `/agents`, `/reload`, `/rewind`, `/fusion`, `/ultra`, `/server`, `/evolve`, `/publish`, `/help` | `agent` — queued on the chat's worker; the reply is a notice in the chat |
| **The window's own** | `/clear`, `/diff`, `/todos`, `/dashboard`, `/rail`, `/resume`, `/resume-claude`, `/settings`, `/provider`, `/login` | `ui` — a pane, a sheet, a list |
| **Terminal only** | `/vim`, `/ui`, `/quit`, `/exit` | `unavailable` — refused, with what the command is and why a window is not where it runs |

Where the two surfaces differ, the reason is the same one: **a chat is its session file.**
`/clear` rotates that file, and `/resume` picks another — so in the window they are a new chat
and the chat list, not commands against the agent. `/rewind` truncates it, and answers with a
notice naming the turn and the files it restored; the transcript already drawn above that
point is left where it is.

### What the agent may run itself

The `run_command` tool lets the model invoke these commands. Two gates apply, in order:

1. `SlashCommand::agent_runnable` — the same on every surface. It refuses the interactive
   pickers without their argument (`/effort` alone; `/effort high` is fine, and the same for
   `/model` and `/mode`), the editors behind `/fusion config` and `/ultra config`, the roster
   picker `/agents`, the session-ending and destructive commands (`/quit`, `/clear`,
   `/rewind`, `/resume`, `/resume-claude`), the ones that reach outside the session to set the tool up
   (`/provider`, `/login`, `/publish`, `/evolve`, `/server`), `/ui` (it repaints the
   user's terminal), and `/btw` and `/fork` (they are the user's aside and the user's side
   quest; the agent has the conversation already, and `spawn_subagent` for its own background
   work).
2. The surface's dispatch set — every command on the TUI, the `server` ones the executor
   implements on the GUI, **none at all** headless, on the gateway, or inside a subagent
   (nothing there would drain the queue, so the tool refuses rather than report a success
   that never happens).

A command that fails either gate is refused **in the tool result**, which is the only thing
the model reads before the turn ends. It is never silently dropped.

## Custom slash commands

Two ways to put reusable text in front of the model: `/commands` you define as markdown files, and `@path` tokens that inline file contents. Both work identically in the TUI and in headless `-p` runs: one shared preprocessing pipeline (`commands::preprocess`) handles them.

A custom command is a markdown file whose body is a prompt template:

- `~/.wizard/commands/*.md`: global, available in every project
- `<project>/.wizard/commands/*.md`: per project, shadows a global command with the same name

The file stem is the command name: `review.md` defines `/review`. An optional frontmatter block (the same `---`-fenced convention as skills) carries a `description` shown in the TUI suggestion popup:

```markdown
---
description: review a file against the project conventions
---
Review $1 carefully. Check it against the conventions in @WIZARD.md.
Focus on: $ARGUMENTS
```

### Placeholders

| Placeholder | Expands to |
|-------------|------------|
| `$ARGUMENTS` | everything typed after the command name |
| `$1` … `$9` | the whitespace-split positional arguments (missing ones expand to the empty string) |

Expansion is a single pass: `$`-like text inside the arguments themselves is never re-expanded.

### Invocation

- **TUI:** type `/review src/app.rs`. Custom commands show up in the same suggestion popup as builtins (builtins win a name collision). The transcript shows what you typed; the model sees the expanded template.
- **Headless:** `wizard -p "/review src/app.rs"` expands exactly the same way.
- A `/word` that matches no builtin and no custom command is passed to the model as a normal prompt.
- `/reload` picks up new and edited command files without a restart.

## @file references

Any whitespace-delimited token of the form `@path` whose path resolves to an existing file expands to a fenced code block with the file's contents:

```
explain @src/main.rs and how it relates to @docs/architecture.md
```

- Paths resolve relative to the project root; absolute paths and `~/` work too.
- Contents are capped at 50KB per file, with a truncation note when cut.
- Image files (`.png .jpg .jpeg .gif .webp`) expand to a short `[image: name]` placeholder and are attached for vision-capable models (xAI Grok, OpenAI, Anthropic, OpenRouter, Ollama vision models). You can also paste an image path or a `data:image/...;base64,...` URL into the composer.
- **Paste an image from the clipboard** (a screenshot, a copied picture) and it attaches directly, shown in the composer as `[Image #1]`, `[Image #2]`, … like Claude Code. If your terminal doesn't forward the paste, press **Ctrl-V** to pull the image off the clipboard. Reading the clipboard uses `wl-paste`/`xclip` on Linux and `pngpaste` or AppleScript on macOS. There is a PowerShell arm in the source, but Windows is not a supported target in this release; under WSL2 you get the Linux path.
- A token that does not resolve to a file is left untouched, so email addresses (`user@host`) and decorators pass through. `@@path` escapes a literal `@path`.
- **TUI:** Tab completes the path under the cursor from its directory listing after you type `@`.

The expansion happens before the prompt reaches the agent, so the file contents land in the conversation history (and survive into the session file) like any other user text.
