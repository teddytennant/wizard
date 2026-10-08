# Token profiles

A token profile decides how much of the harness goes out with every model
request. The default is `safe`.

| Profile | What it changes |
|---|---|
| `stock` | Nothing. The harness as it was through 3.2.5. |
| `safe` | Skill frontmatter is read as YAML (a nested `always: true` no longer puts a skill's whole body in every prompt). Subagents are listed one line each. A continuous run's cycle prompt points at the pinned mission instead of repeating it. An identical re-read of an unchanged file returns a short stub. `subagent_status` stops repeating a report the completion note already delivered. MCP tools are listed by name and loaded on first call. Old tool-call arguments are digested along with old results. |
| `lean` | `safe`, plus every tool outside the everyday set (`execute`, `read_file`, `edit_file`, `write_file`, `search_files`, `todo`, `spawn_subagent`) is deferred, and `execute` output is capped at 12 KB with the rest spilled to a file. |
| `min` | `lean`, plus a short system prompt and terse schemas for four core tools. |

Deferred tools run when the model calls them by name. `tool_search` returns a
schema when the model wants one first.

## Picking one

```bash
wizard --token-profile stock        # this run
WIZARD_TOKEN_PROFILE=min wizard     # this shell
```

or in `~/.wizard/config.toml`:

```toml
token_profile = "lean"
```

The flag wins over the environment, and the environment wins over the config.
An unknown name falls back to `safe` with a warning.

## Measured

First request, empty directory, Grok 4.6 over xAI, with a config that has the
Playwright MCP server and one `session_start` hook:

| Profile | prompt tokens |
|---|---:|
| stock | 15,517 |
| safe | 10,087 |
| lean | 5,852 |
| min | 1,729 |

Of `min`'s 1,729, about 180 are the hook's own text.

Per task, on 8 tasks taken from a real project's history (a commit that adds
failing tests, then the commit that makes them pass; the run is graded on the
original tests and fails if it edits them), two tries each, 64 runs:

| Profile | passed | mean prompt tokens | est. cost | wall |
|---|---:|---:|---:|---:|
| stock | 16/16 | 624k | $0.61 | 243 s |
| safe | 16/16 | 522k | $0.54 | 262 s |
| lean | 16/16 | 578k | $0.66 | 405 s |
| min | 16/16 | 568k | $0.66 | 349 s |

These runs predate MCP deferral under `safe`, so its per-task number above
still carries the Playwright schemas on every call. They also predate
digesting old tool-call arguments on `safe`.

`lean` and `min` send far less per request but take 3 to 5 more calls per
task, and each extra call adds uncached tokens and output. The trimmed prompt
was mostly the cached part of each request, so they came out slightly more
expensive than `stock`. Use them when the prompt size itself matters, for
example a small local model with a short context window. `safe` is the
default because it was cheapest with no loss on these tasks.
