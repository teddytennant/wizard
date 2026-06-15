# Custom commands and @file references

Two ways to put reusable text in front of the model: `/commands` you define as markdown files, and `@path` tokens that inline file contents. Both run through one shared preprocessing pipeline (`src/commands.rs`) before the prompt reaches the agent.

## Custom slash commands

A custom command is a markdown file whose body is a prompt template:

- `~/.wizard/commands/*.md` — global, available in every project
- `<project>/.wizard/commands/*.md` — per project, shadows a global command with the same name

The file stem is the command name: `review.md` defines `/review`. An optional `---`-fenced frontmatter block carries a `description` shown in the TUI suggestion popup:

```markdown
---
description: review a file against the project conventions
---
Review $1 carefully. Check it against the conventions in @docs/architecture.md.
Focus on: $ARGUMENTS
```

### Placeholders

| Placeholder | Expands to |
|-------------|------------|
| `$ARGUMENTS` | everything typed after the command name |
| `$1` … `$9` | the whitespace-split positional arguments (missing ones expand to the empty string) |

Expansion is a single pass: `$`-like text inside the arguments themselves is never re-expanded.

### Invocation

- Type `/review src/app.rs` — custom commands show up in the same suggestion popup as builtins (builtins win a name collision). The transcript shows what you typed; the model sees the expanded template.
- A `/word` that matches no builtin and no custom command is passed to the model as a normal prompt.
- Command files are loaded at launch; restart Wizard to pick up new or edited ones.

## @file references

Any whitespace-delimited token of the form `@path` whose path resolves to an existing file expands to a fenced code block with the file's contents:

```
explain @src/main.rs and how it relates to @docs/architecture.md
```

- Paths resolve relative to the project root; absolute paths and `~/` work too.
- Contents are capped at 50KB per file, with a truncation note when cut.
- Image files (`.png .jpg .jpeg .gif .webp`) are replaced by a note — this build has no vision path to attach them to.
- A token that does not resolve to a file is left untouched, so email addresses (`user@host`) and decorators pass through. `@@path` escapes a literal `@path`.
- **TUI:** Tab completes the path under the cursor from its directory listing after you type `@`.

The expansion happens before the prompt reaches the agent, so the file contents land in the conversation history like any other user text.
