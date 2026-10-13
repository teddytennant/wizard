# Decision routing

A decision model is a small classifier endpoint that answers yes/no questions
about a piece of text with a probability, in a few hundred milliseconds. With
one configured, Wizard asks it which parts of the prompt a request needs and
sends only those. Without one, nothing changes: no call is made and every
request is what it was.

## Turning it on

```toml
[decisions]
provider = "openai"
```

and a key for that provider in the environment (or stored under the
provider's name in `credentials.toml`). A `[decisions]` table with no key is
off, silently.

| Provider | Model | Endpoint | Key |
|---|---|---|---|
| `openai` | `gpt-6-luna` | `POST https://api.openai.com/v1/decisions` | `OPENAI_API_KEY` |
| `subq` | `subq-snap-preview` | `POST https://api.subq.ai/v1/systemone` | `SUBQ_API_KEY` |
| `jev` | `jev-1.13.0` | `POST https://api.typesafe.ai/v1/systemone` | `JEV_API_KEY` |

OpenAI's endpoint takes an API key, not a ChatGPT sign-in. Jev also answers
through OpenRouter: set `base_url = "https://openrouter.ai/api"` and point
`api_key_env` at an OpenRouter key.

Every other key is optional:

```toml
[decisions]
provider = "openai"
model = "gpt-6-luna"            # the provider's default
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
timeout_ms = 2000
skill_min = 0.6
section_min = 0.4
```

## What is routed

One call, before the first model step of a session, scores the prompt against
two things.

**Sections of the sovereign prompt.** Five parts of it apply only to some
tasks. One that scores under `section_min` is left out:

| Section | Kept when the request |
|---|---|
| `codebase` | changes code in an existing project |
| `deliverables` | names an output file, format, schema or fixed list of IDs it will be judged by |
| `services` | needs a process left running after the work is done |
| `native-install` | builds or installs a package from source, or compiles a native extension |
| `images` | involves looking at an image, screenshot or board |

Genie mode, the `min` token profile and a `system_prompt.md` override have no
conditional sections, so only skills are routed there. Chat mode is never
routed.

**Skills.** Every listed skill's name and description is scored. Up to three
that reach `skill_min`, best first and within 8000 characters together, have
their bodies loaded after the skills index. The index itself is unchanged, so
the rest can still be read on demand.

The result shows as a notice, and in the turn's `routing` field in
`usage.jsonl`:

```
~ routed in 322 ms: sections services; skills none; system prompt ~1906 tokens, ~2497 unrouted
```

## Later prompts and the prompt cache

The system prompt is the prefix of every request, so it is decided once, at
the first prompt, and not rewritten after that. A later prompt is still
scored. A section or skill it needs that is not loaded yet goes in as a note
after that prompt, which leaves everything before it byte-identical. Nothing
is ever taken back out.

## When the call fails

A call that errors or passes `timeout_ms` is dropped and the turn goes ahead
with the full prompt. The reason is in the log, not the transcript. Since the
full prompt has then gone out once, it stays for the session; later prompts
can still add skills.
