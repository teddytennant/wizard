# Commands

Wizard's built-in `/commands`, the custom ones you define as markdown files, and the `@path`
tokens that inline file contents. All three live in `src/commands/`. The TUI reads the
built-in table from there; headless `-p` shares
the custom-command and `@path` pipeline but does not parse built-ins (a `/word` typed there
goes to the model unless it names a custom command).

## Built-in commands

`COMMANDS` in `src/commands/mod.rs` is the single source of truth: what a command is called,
what it does, and how each surface runs it. The TUI's suggestion popup and the allowlist the
*agent* may invoke through its `run_command` tool are both
derived from it. Two hand-kept lists is how two surfaces drift into offering different
commands; there is one.

The TUI runs every command — it is the surface they were written against. The table also
carries a `gui` column (`Surface::Gui`, `src/commands/surface.rs`) that says how a graphical
front end over the agent core would run each one. Both surfaces that used it, the browser GUI
and then the iced window, are gone; Wizard GUI is a separate app that talks to Wizard over ACP.

### What the agent may run itself

The `run_command` tool lets the model invoke these commands. Two gates apply, in order:

1. `SlashCommand::agent_runnable` — the same on every surface. It refuses the interactive
   pickers without their argument (`/effort` alone; `/effort high` is fine, and the same for
   `/model` and `/mode`), the editors behind `/fusion config` and `/ultra config`, the roster
   picker `/agents`, the session-ending and destructive commands (`/quit`, `/clear`,
   `/rewind`, `/resume`, `/resume-claude`), the ones that reach outside the session to set the tool up
   (`/provider`, `/login`, `/publish`, `/evolve`, `/server`), `/ui` and `/view` (they
   repaint the user's terminal), and `/btw` and `/fork` (they are the user's aside and the user's side
   quest; the agent has the conversation already, and `spawn_subagent` for its own background
   work).
2. The surface's dispatch set — every command on the TUI, the `server` ones the executor
   implements on the GUI, **none at all** headless, on the gateway, over ACP, or inside a subagent
   (nothing there would drain the queue, so the tool refuses rather than report a success
   that never happens).

A command that fails either gate is refused **in the tool result**, which is the only thing
the model reads before the turn ends. It is never silently dropped.

### Reports that leave the machine

Most reports read the session. `/usage` asks the accounts instead: one block per signed-in
subscription (xAI, ChatGPT), whichever provider is active, plus the session's token rollup
when the active provider is an API key. It runs the same on the TUI and over ACP, so Wizard
GUI, Zed and the Android app get the same plain text. `wizard usage --subscriptions --json`
is the machine-readable form ([usage.md](usage.md#subscription-limits)).

## Claude Code names

Wizard answers to Claude Code's command names. Some are other names for commands it has (`/reset` is `/clear`, `/continue` is `/resume`, `/stats` is `/usage`, `/config` is `/settings`), some are small commands with a body in `src/commands/claude.rs` (`/permissions`, `/hooks`, `/context`, `/mcp`, `/output-style`, `/skills`, `/plugin`, `/export`, `/copy`, ...), and the rest are bundled prompts. They each have a row in the table where they run, so the agent gate, `/help` and the per-surface refusals cover them like any other command. `/permissions` and `/hooks` read `src/permissions` and `src/hooks` through the loaders a session uses: the rules grouped by where they were written, and the hooks by event with matcher, handler type and source. `/loop` writes entries for the scheduler, `/branch` and `/rename` work on the session file, `/fast`, `/sandbox` and `/privacy-settings` are small real commands (the Anthropic provider's speed tier, the sandbox deny rules, and where data leaves the machine), and the commands with no Wizard counterpart (`/voice`, `/advisor`, ...) answer with one sentence saying so. Their bodies are in `src/commands/` (`views.rs`, `loop_cmd.rs`, `session_cmds.rs`, `misc.rs`). `docs/claude-code-parity.md` lists every Claude Code command and flag, which of these it is, and what is missing and why.

## Custom slash commands

Two ways to put reusable text in front of the model: `/commands` you define as markdown files, and `@path` tokens that inline file contents. Both work identically in the TUI and in headless `-p` runs: one shared preprocessing pipeline (`commands::preprocess`) handles them.

A custom command is a markdown file whose body is a prompt template:

- `~/.claude/commands/`, then `~/.wizard/commands/`: global, available in every project
- `<project>/.claude/commands/`, then `<project>/.wizard/commands/`: per project, shadows a global command with the same name
- the commands of enabled Claude-format plugins, as `/plugin:name`
- the prompt commands that ship in the binary (`/init`, `/review`, `/security-review`, `/pr-comments`, `/simplify`, `/recap`, `/add-dir`, `/debug`, `/verify`, `/update-config`), which any of the above replaces

Claude Code's `.claude/commands` is read in place, so a project set up for Claude Code works with no import. At the same level a `.wizard` file beats a `.claude` one.

The file stem is the command name: `review.md` defines `/review`. A subfolder namespaces it, as in Claude Code: `team/deploy.md` is `/team:deploy`. An optional frontmatter block (the same `---`-fenced convention as skills) carries a `description` shown in the TUI suggestion popup:

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
