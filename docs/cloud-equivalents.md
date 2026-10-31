# Cloud features, run locally

Some Claude Code features sit on Anthropic's servers: published artifacts, cloud routines, cloud sessions, remote control, the advisor. Wizard has no such backend, so each of these is rebuilt from what Wizard does have. Every one runs on a machine you own, and every reply says so. Where nothing honest exists, the last section says that too.

| Claude Code | Wizard | Where it lives |
| :- | :- | :- |
| `Artifact`, `/artifacts` | local pages with `file://` and localhost links | `~/.wizard/artifacts/` |
| `RemoteTrigger`, `/schedule` routines | triggers that run a headless session on a schedule or an HTTP call | `~/.wizard/triggers/`, `schedule.toml` |
| `/teleport` | move a session between your machines | `wizard teleport` |
| `/remote-control`, `/remote-env` | how to attach to this machine; where triggers run | read-only report |
| `/desktop`, `/mobile` | Wizard GUI; the Android app over ssh | separate apps |
| `/advisor` | the `advisor` tool and `advisorModel` | `~/.wizard/settings.json` |
| `/voice` | dictation from a local recorder and speech-to-text | `[voice]` in `config.toml` |
| Bedrock, Vertex, Foundry | the Claude Code provider's cloud route | `env` in `~/.wizard/settings.json` |

## Artifact and /artifacts

The `Artifact` tool takes Claude Code's input (every property of its schema, none required) and writes the page instead of uploading it.

```
Artifact {file_path: "report.html"}
Published locally, not hosted: file:///home/me/.wizard/artifacts/cost-chart-3fa91c/index.html
```

- A page lands in `~/.wizard/artifacts/<slug>/index.html`. The slug comes from the page's `<title>` and a random suffix. A record sits beside it in `.artifact.json`, which is never served.
- `url` (the `file://` link, the served link, or the slug) updates that artifact in place and bumps its version. `edits` change the source file first, each `old_str` exactly once, all or none, and the source file keeps them. `files` copies supporting files in (`{"app.js": "src/app.js"}`, `null` removes one, `{artifact, path}` takes one from another local artifact). `asset: true` uploads images and fonts and returns relative `assets/<id>.<ext>` URLs that work over `file://` and over the server.
- `list`, `read`, `open`, `pin`, `unpin` and `delete` work, as do the `files` and `assets` scopes. `open` starts the system browser when a display exists.
- Limits are Claude Code's: 16MB for a page or text file, 15MB per binary file, 255 files and 64MB per publish, 511 files and 256MB per artifact. Published paths must be relative, with no `..`.
- `wizard artifact serve` serves the directory read-only on `127.0.0.1:8765/<slug>/`. It refuses dotfiles and anything outside an artifact. `wizard artifact list` and `wizard artifact path <slug>` print what exists.
- `/artifacts` lists them with both links; `/artifacts open|delete|pin|unpin <slug>`.

Not available, and named in the reply when a call asks for it: `capabilities` (live data, shared state, viewer identity, Claude calls from the page), artifact types and design systems, comments, the shared database, sharing. A local page is static, and nobody but this machine can open its link.

## RemoteTrigger, and scheduled routines

`RemoteTrigger` keeps Claude Code's schema (`action`, `trigger_id`, `session_id`, `cursor`, `body`) and its reply frame (`HTTP 200`, a JSON body, `→` summary lines). A trigger is a prompt that Wizard runs as a headless session (`wizard --mode sovereign -p ...`) in a directory.

| Action | What it does |
| :- | :- |
| `list`, `get` | triggers with `next_run_at` |
| `create`, `update` | `body`: `name`, `prompt`, `cron_expression` (5 fields, local time) or `run_once_at` (RFC 3339), `webhook: true`, `cwd`, `mode`, `max_hours` (default 1), `enabled`. Claude's `job_config.ccr.events` shape is read for its prompt |
| `run` | start it now, detached; `body.text` reaches the run as data |
| `create_webhook_trigger` | give an existing trigger (`body.routine_trigger_id`) a webhook token |
| `list_runs`, `get_run_log` | runs started here and the scheduler's job logs, newest first; the last 200 lines of a log |

- A scheduled trigger is an entry `trigger-<id>` in `~/.wizard/schedule.toml`, so it fires only while `wizard scheduler` runs (`wizard scheduler install` keeps it running). The reply warns when no scheduler holds the lock. A `run_once_at` trigger becomes a one-shot entry.
- A webhook is `POST /v1/triggers/<id>/run` with `Authorization: Bearer <token>` against `wizard trigger serve` (loopback, port 8787; `--bind` for another interface). The request body goes to the run inside `<payload>` tags marked as data from the sender. A wrong token and an unknown trigger both answer 401. The token is stored in `~/.wizard/triggers/triggers.json` (mode 0600) and never appears in a `RemoteTrigger` reply: the model does not see it. `wizard trigger token <id>` prints it for you.
- Runs write `~/.wizard/triggers/runs/<trigger>/<run>.log` and an `.exit` file, so `list_runs` can say completed, failed or running.
- `wizard trigger list|add|remove|enable|disable|run|token|runs|serve` is the same thing from a shell.

The difference from the cloud: nothing fires while this machine is off, there are no GitHub-event sources (any authenticated POST fires a webhook), and there is no sandbox beyond the directory and `max_hours`.

`/schedule` and `/loop` already map onto `wizard schedule`; see [scheduler](scheduler.md).

## /teleport

A session is one JSONL file under `~/.wizard/sessions/`. `wizard sync` carries config only and the mesh gives a read-only live view, so neither moves one. Teleport wraps the file in a small gzip envelope and carries it over a file, a URL or ssh.

```
/teleport                      what it does and what to set up
/teleport pack [id]            write ~/.wizard/teleport/<id>.gz for the current session
/teleport user@host            pull the newest session from that machine over ssh
/teleport user@host:<id>       a named one
/teleport ./s.gz | https://…   an envelope from a file or URL
wizard teleport pack [id] [--out FILE|-]
wizard teleport pull <source>
```

Setup: Wizard 3.12 or later on both machines, `wizard` on the PATH of a non-interactive ssh login, and key-based ssh from this machine to the other (`ssh-copy-id`). The pulled session keeps its history, gets a free id (`-tp2` if the id is taken) and has its recorded directory set to where you pulled it, so `/resume` here finds it; the command then resumes it. The envelope is not signed, and the model replays what is in it: pull only from machines and links you trust.

## /remote-control and /remote-env

There is no Anthropic relay to register with. `/remote-control` prints the ways to reach this machine directly, with your user and host filled in:

- ssh to the machine and `wizard --resume` (the Android app does this)
- `ssh user@host wizard acp` for editors and the app
- `wizard gateway setup` for a Telegram bot in front of the same agent
- an HTTP trigger: `/remote-control on` creates a webhook trigger named `remote-control` for the current directory and prints its token and the `curl` line; `/remote-control off` deletes it and revokes the token. Anyone holding the token can make this machine run the agent in that directory, so it is off until you ask.

It is terminal-only, because it prints a token. `/remote-env` lists where triggers run (directory, mode, cap) and whether the scheduler is up; an environment here is a directory on a machine you run. `--remote-control` on the command line is still accepted with a pointer to the command.

## /desktop and /mobile

`/desktop` reports whether Wizard GUI is installed and `/desktop open` starts it in the current directory (same as `wizard gui`). It is a separate app with its own session, not a handoff of this one; `/resume` there continues it. `/mobile` describes the Android app (`android/`), which connects to your machines over ssh and drives `wizard acp`, and gives the APK link. There is no QR code or pairing server.

## /advisor

```
/advisor                 current setting
/advisor opus            a model tag asked of the active provider
/advisor big             a configured provider, with its own model
/advisor big/huge-2      another model on that provider
/advisor off
```

The setting is `advisorModel` in `~/.wizard/settings.json` (also `--advisor MODEL`, and `WIZARD_ADVISOR_MODEL`). The `advisor` tool, listed by `tool_search`, takes a `question` and a `context` summary the working model writes, sends them to the advisor model as one plain completion with no tools, and returns the advice. The advisor sees only that summary and never acts. Its tokens are billed to the session in `/cost`.

This differs from Claude Code, where the API runs the second model over the whole transcript server side and returns an opaque block. Wizard cannot decode that block, so the working model chooses what the advisor sees. For a standing panel see [/fusion](fusion.md) and [/ultra](ultra.md); the advisor is the one-shot, on-demand form.

## /voice

```toml
# ~/.wizard/config.toml, all optional
[voice]
command  = "my-stt {wav}"      # prints the text on stdout; {wav} is appended if absent
recorder = "my-rec {out} {seconds}"
model    = "~/models/ggml-base.en.bin"   # for whisper-cli
seconds  = 30
```

`/voice` starts recording and returns; speak, then `/voice stop` (or let the time limit pass). The text arrives in the composer for you to edit and send. `/voice 10` sets the limit, `/voice status` shows what was found. It is terminal-only.

Recorder, first found: `arecord`, `ffmpeg` (PulseAudio on Linux, AVFoundation on macOS), `sox` or `rec`, or `[voice] recorder`. It records 16 kHz mono WAV. Transcriber: `[voice] command`, else `whisper-cli` (also `whisper-cpp`) with a ggml model (`[voice] model`, `WHISPER_MODEL`, or a `ggml-*.bin` in `~/.wizard/models/whisper`, `~/.cache/whisper`), else `vosk-transcriber`. With nothing installed, the reply says which to install. All of these are local; nothing is uploaded unless your own command does it.

Tested here with a fake recorder and a fake transcriber script, including `/voice stop` interrupting a running recording. The `arecord`, `ffmpeg`, `sox`, `whisper-cli` and `vosk-transcriber` command lines are built to those tools' documented flags but were not run against the real programs on the build machine.

## Bedrock, Vertex and Foundry

Through the Claude Code provider (`--provider claude`), the real `claude` honors `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` and `CLAUDE_CODE_USE_FOUNDRY` and the cloud's own credentials. Wizard starts it with your environment unchanged, so anything exported works, and it adds the cloud-route names from the `env` block of `~/.wizard/settings.json` (and only those names: `CLAUDE_CODE_USE_*`, `ANTHROPIC_BEDROCK_*`, `ANTHROPIC_VERTEX_*`, `ANTHROPIC_FOUNDRY_*`, `AWS_*`, `AZURE_*`, `GOOGLE_*`, `CLOUDSDK_*`, `CLOUD_ML_REGION`, model names). The process environment wins over the file.

```
/setup-bedrock                         what is detected, and what to run
/setup-bedrock us-east-1 profile=dev model=<inference profile or model id>
/setup-bedrock off
/setup-vertex                          likewise
/setup-vertex my-project region=us-east5 model=<model id>   (region defaults to global)
/setup-vertex off
```

They write the `env` block and never a key; credentials stay in your shell, `~/.aws`, or `gcloud`'s application-default credentials. `wizard doctor` gains a `cloud route` row when the Claude Code provider is configured or a route is on: it names the cloud and what is still missing (region, credentials, project, resource), and reads files and variables without calling anything.

Wizard's own `anthropic` provider reaches **Foundry** with no code: set `base_url = "https://<resource>.services.ai.azure.com/anthropic"` and the Foundry API key, since Foundry serves `/v1/messages` there. That path was not exercised against a live Foundry resource. **Bedrock** (AWS signing plus a binary event stream) and **Vertex** (a per-model `:streamRawPredict` URL, a changed body and Google OAuth) are not a base URL and a header, so they are not built into the `anthropic` provider; the Claude Code provider is the route for both.

## What stays unavailable

| Claude Code | Why |
| :- | :- |
| `Artifact` capabilities, types, design systems, comments, sharing | hosted claude.ai features; the reply names the ignored fields |
| Cloud routines run by Anthropic, GitHub-event sources | there is no Anthropic scheduler; triggers run on your machine and fire any authenticated POST |
| Cloud sessions (`--cloud`, `--remote`, `--environment`, `/web-setup`) | Anthropic's sandboxes; teleport and triggers are the machine-to-machine versions |
| The claude.ai relay behind `/remote-control`, QR pairing to the Claude mobile app | Anthropic's service; direct ssh, ACP and triggers replace it |
| The server-side advisor block | cannot be decoded; the `advisor` tool replaces it |
