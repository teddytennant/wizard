# Doctor and status

## `wizard doctor`

Diagnoses the environment and prints one line per check:

```
✓ config            /home/you/.wizard/config.toml parses
✓ active provider   'local'
✓ provider local    llama.cpp:qwen3-30b (qwen3-30b) reachable
– provider openai   $OPENAI_API_KEY not set and no stored key
– gateway           kind = none (terminal only; set kind = "telegram" to enable)
✓ system prompt     4 section(s), 12.3 KiB, ~3100 tokens; cacheable through 9.8 KiB; personality …, charter …
✓ credentials       3 stored key(s), permissions ok
✓ secret storage    4 path(s) owner-only
✗ mcp playwright    failed to spawn MCP server 'playwright' (command: npx): No such file or directory (os error 2)
✓ native tools      22 tools registered
✓ platform          linux/x86_64
✓ color depth       truecolor (COLORTERM=truecolor, TERM=xterm-256color)
– hooks (global)    /home/you/.wizard/hooks.toml absent (no hooks)
✓ ~/.wizard         /home/you/.wizard writable
✓ hooks (project)   2 hook(s) in .wizard/hooks.toml
✓ project .wizard   .wizard writable
✓ sessions          /home/you/.wizard/sessions writable
✓ checkpoints       12 snapshot(s) across 4 turn(s)
```

Checks, in the order they run:

- **config**: `~/.wizard/config.toml` parses (missing file is fine: defaults apply)
- **active provider**: `active_provider` names a provider that is actually configured; a name that matches nothing fails and says which provider is being used instead
- **provider \<name\>**: each configured LLM provider answers its health probe; skipped (`–`) only when it has no key at all, meaning its API key env var is unset **and** nothing is stored for it in `credentials.toml`. A key you saved through the interactive `/provider` menu is a key, so that provider is probed over the network even with no env var exported
- **gateway**: when `gateway.kind = "telegram"`, four lines: the kind, **gateway token** (present or missing, naming `credentials.toml` or the env var it came from, never the secret itself), **gateway allow-list**, and **gateway process**. With `kind = "none"` it is one skip line, unless a telegram token is stored under `[keys]`, which is a failure because the gateway will never start
- **gateway allow-list**: `gateway.allowed_chat_ids` names at least one chat. An empty list is a **failure**, not a note: the list is a closed allow-list, so empty means the bot refuses every inbound message and replies to nobody. Configs written before that semantics changed still carry an empty list, which is exactly why this is loud
- **gateway process**: `pgrep -af wizard` finds a running `wizard --gateway`. Not finding one is a **failure**, not a note, on the same reasoning as the allow-list: a configured telegram gateway with no process behind it answers nobody. Only a `pgrep` that cannot be run at all is a skip. This is the check most likely to be red on a machine where the gateway is a systemd unit on another host, so read the caveat below before scripting the exit code
- **system prompt**: never a failure, always a measurement: section count, total size, estimated tokens, the size at which a provider-side prompt cache should be cut, and a per-section breakdown — every size in KiB. It measures the baked prompt for the configured mode, so skills, `AGENTS.md` and the memory index are left out
- **credentials**: `~/.wizard/credentials.toml` parses strictly (a corrupt file otherwise degrades silently to "no stored keys") and is not readable by other local users. An absent file is a skip, and so is the whole check on a filesystem that cannot answer the permission question
- **secret storage**: the paths beside it that are just as sensitive and have no check of their own are owner-only too: `~/.wizard` itself, `logs/`, `sessions/`, and the two OAuth token files. Absent paths are skipped, not failed. A loose path is a failure only when the filesystem could carry the fix, which doctor establishes by creating a private probe directory and reading its mode back: where it cannot (exFAT, FAT32, WSL DrvFs, a share without POSIX modes, all of which `WIZARD_HOME` is allowed to point at) the same finding is reported as a skip, because failing the exit code forever over a `chmod` the filesystem cannot honour would break the preflight idiom below. A path that exists but cannot be stat'd at all is a failure rather than a silent pass; a platform that has no answer to the question is a skip
- **cloud route**: shown when the Claude Code provider is configured or `CLAUDE_CODE_USE_BEDROCK`, `_VERTEX` or `_FOUNDRY` is on, in the shell or in the `env` block of `~/.wizard/settings.json` (`/setup-bedrock` and `/setup-vertex` write it). Names the cloud and what it still lacks (region, project, resource, credentials), reading variables and credential files without calling anything. A route that is on and incomplete fails; no route is a skip. See [cloud equivalents](cloud-equivalents.md#bedrock-vertex-and-foundry)
- **mcp \<name\>**: each `[[server]]` in `~/.wizard/mcp.toml` spawns and completes the MCP handshake (with tool count)
- **native tools**: the compiled-in tool set is registered
- **platform**: host notes (Termux source-build expectations, NixOS flake preference, or plain OS/arch)
- **color depth**: the depth the UI will paint at (`no color` / `16 colors` / `256 colors` / `truecolor`) and the environment variables that decided it (`NO_COLOR`, `WIZARD_COLOR`, `COLORTERM`, `TERM`). Never a failure: it answers "why is Wizard monochrome on this box", which is always one of those four
- **hooks (global)** then **~/.wizard**, then **hooks (project)** then **project .wizard**, then **sessions**: each `hooks.toml` parses, and each of those directories accepts writes. The two pairs interleave, which is why the sample above reads in that order
- **checkpoints**: the snapshot index parses; stale snapshot directories are counted

Every network probe is bounded, so doctor never hangs: provider health probes are capped at 5 seconds, and each MCP handshake gets the runtime's own 20-second connect budget — the same one the agent allows, so a slow-starting `npx`/`uvx` server that works in a session does not fail here. Exit code: 0 when no check failed (`–` skips are not failures), 1 otherwise. Use it as a preflight in scripts:

```sh
wizard doctor && wizard --mode sovereign -p "task"
```

Two caveats if you script the exit code, both of them telegram gateways:

- An existing gateway configured before the allow-list became closed fails **gateway allow-list** until you add a chat id, which flips a previously green `wizard doctor` to exit 1. That is a real finding rather than noise, since the bot is answering nobody, but it is a change in what a green run means.
- With `gateway.kind = "telegram"` set, **gateway process** fails whenever no `wizard --gateway` is running on *this* machine. That is the normal state of a laptop whose gateway runs on a server, and of any machine where you configured the gateway but only ever use the TUI, so the preflight above refuses to start a perfectly good sovereign run. Either keep `kind = "none"` on machines that do not host the gateway, or check the report instead of the exit code (`wizard doctor | grep '^✗'`).

`/doctor` in the TUI runs the same battery and prints the report to the transcript.

## Bug-report bundles

```bash
wizard doctor --bundle
```

Runs the same checks, prints the same report, then writes everything a bug report needs into one directory and tells you where it landed:

```
bundle: /home/you/.wizard/bundles/doctor-20260801T101422Z
  7 member(s): config.toml, session.jsonl, usage.jsonl, logs/2026-08-01T10-14-22-48213.jsonl, doctor.txt, manifest.json, README.txt
  absent: evolution.jsonl
Secrets are stripped by an allowlist, but the transcript is your own text: read the bundle before you send it anywhere.
```

Bundles land in `~/.wizard/bundles/doctor-<UTC timestamp>/` as a directory of plain files. Nothing is uploaded anywhere; attaching it to an issue is your call and your action.

The exit code matches plain `wizard doctor` (1 when a check failed), so either mode scripts the same.

### What it collects

| Member | From | Notes |
|--------|------|-------|
| `config.toml` | `~/.wizard/config.toml` | Passed through a field **allowlist**: anything not on the list of known-safe keys becomes `<redacted>`, including keys added after this build |
| `session.jsonl` | newest file in `~/.wizard/sessions/` | The most recent transcript only, and only its tail |
| `usage.jsonl`, `evolution.jsonl` | `~/.wizard/` | Token usage and the self-extension log, when present |
| `logs/*` | five newest files in `~/.wizard/logs/` | See [logging.md](logging.md) |
| `doctor.txt` | this run | The rendered check report, so the bundle stands alone |
| `manifest.json` | this run | Version, build commit, OS, arch, timestamp, and the member / omitted / truncated lists |
| `README.txt` | this run | What is in the bundle and how it was redacted, for whoever opens it |

Every input is optional: a fresh install has no sessions, most installs have no `evolution.jsonl`. Anything missing is listed under `absent:` and recorded in `manifest.json`, so a reader can tell "there were no logs" from "the logs were withheld". Members are capped at 2 MiB and cut from the *front* when they are larger (the tail is the part that explains a crash); anything cut is listed as truncated in the output and in the manifest.

`credentials.toml` and the OAuth token files are never copied in, at all.

### What is redacted

Three layers, in order:

1. **Known secrets, by value.** Every key in `~/.wizard/credentials.toml`, the xAI and ChatGPT OAuth tokens, the values of the environment variables `config.toml` names as key holders, and the **credential-named** entries of `mcp.toml`'s `[server.env]` / `[server.headers]` maps are substituted out of every member wherever they appear, including mid-sentence in the transcript. This is the layer that catches an opaque key with no recognizable shape.

   "Credential-named" is the filter layer 3 uses on key names, applied here to the entry's **key**: one of the denylisted names (`api_key`, `token`, `password`, `authorization`, `client_secret`, …) either exactly, or as the tail after a `_`, `-`, `.` or `:`, which is what catches the real-world `TAVILY_API_KEY` and `slack_bot_token` spellings. It is not optional and it is not a widening: both maps are mostly ordinary settings, and substituting an `ALLOWED_DIRECTORIES = "/home/you/projects"` or a `NODE_ENV = "production"` as a literal meant erasing that string everywhere it occurred, so every project path vanished from the transcript and "the reproduction steps" came out as "the re`<redacted>` steps". The cost is real: a genuine secret stored under an ordinary-looking key (`SENTRY_DSN`, `PROJECT_TOKEN_V2`) is not collected here, and only reaches a later layer if its *shape* is recognizable. Name such an entry `*_token` / `*_api_key` if you want it caught by name. An `env:VAR` value is the documented indirection, so the variable is resolved from the environment instead; an `Authorization = "Bearer <token>"` header also contributes the bare token, because that is how it appears in a request log.
2. **Credential shapes.** Words carrying a known vendor prefix (`sk-`, `ghp_`, `github_pat_`, `xoxb-`, `AKIA`, `AIza`, and friends), JWTs, Telegram bot tokens, URL userinfo (`https://user:pw@host`, which keeps its scheme and host and loses the credential), and PEM blocks are replaced.

   A PEM block is redacted from its `-----BEGIN …` header to the end of its `-----END …-----` footer, body included. The span is bounded by the PEM character set (base64, the label's letters and spaces, dashes, whitespace, `\`, `.`, `_`, `:`, `,`), not by the footer alone: the first character that cannot occur inside a block, `"` and `{` above all, ends it. So a `-----BEGIN` quoted in one JSON transcript record and an `-----END` quoted in a later one are two separate events rather than one `<redacted>` swallowing everything between them.

   This is the one layer that can drop text it never judged, so be clear about how far it reaches. A header whose footer never closes inside that bounded run takes the whole run with it, and how long the run is depends on the member: in a `.jsonl` transcript or log the next `"` is at most one record away, but a plain-text member (`~/.wizard/logs/*.log`) is made of the same characters a PEM body is, so an unterminated header there can withhold everything after it to the end of the file. It fails closed on purpose, and it is never silent: the member is listed in the `absent:` line and in `manifest.json` with the reason, which is the field a reader already consults to tell "there were no logs" from "the logs were withheld".
3. **Secret-looking key names, and auth schemes.** In free text (JSON logs, TOML, header dumps) the *value* following a name such as `api_key`, `token`, `password`, `authorization`, or `client_secret` is replaced. The match is on the tail of the name, so real-world spellings like `OPENAI_API_KEY=…`, `github_token: …`, and `client.secret=…` are caught too. The name has to be in assignment position — followed by `=` or `:`, in the word or across the gap — which is why a sentence like "the token count is 500" survives intact. The same layer replaces what follows a `Bearer` or `Basic` scheme word.

Everything replaced becomes the literal `<redacted>`. The bundle directory itself is created mode 0700 on unix, because it contains a transcript.

### Read it before you send it

Redaction goes after *credentials*. It cannot know that a path, a hostname, a customer name, or a paragraph you typed into the agent is sensitive, and the session transcript is your own text by definition. The bundle is a directory of plain files: open it, read it, delete what you do not want to share, then attach it.

`WIZARD_DOCTOR_BUNDLE=1 wizard doctor` does the same thing, for a wrapper or CI job that cannot easily add a flag. The TUI's `/doctor` prints the report only; bundles come from the CLI.

## `/status`

A one-shot snapshot of the running session. The TUI prints something like:

```
model: qwen3-30b
provider: local (LlamaCpp @ http://127.0.0.1:11435)
mode: genie
effort: default
session: 2026-06-11T09-30-00
usage: 1200 prompt + 240 completion tokens
background tasks: 1 running
todos: 2/5 done
plan mode: off
```

Lines that are only there when they have something to say: `steps:` (the configured step budget, printed right after `effort:`), `context: N tokens` (the size of the next model call, not a session lifetime sum), `background tasks:`, and `ultra:` — there is no `ultra: off`, the line is simply absent when ultra is not running. The TUI prints neither `steps:` nor `context:`; the GUI prints both. For a live next-call estimate in the TUI, use the status bar token readout instead of `/status`. Lifetime prompt/completion totals and cost estimates stay on `/cost`.
