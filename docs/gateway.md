# Messaging gateway

Run Wizard headless as a chat bot: each inbound message drives one autonomous agent turn in your project, and the reply comes back in the chat. Telegram is the supported transport.

```bash
cd ~/your/project
wizard --gateway
```

The gateway is a **long-running foreground process** (Ctrl-C stops it). **Nothing listens until this process is running**; that is the #1 reason Telegram messages get no reply after onboarding. It uses `$WIZARD_GATEWAY_CWD` as the project root when that is set, otherwise the current working directory, and builds **one** agent for the whole session, so the conversation continues across messages. It runs in sovereign posture: no terminal, no human in the loop, tool calls execute directly. Read [SECURITY.md](../SECURITY.md) before pointing a public bot at a machine you care about.

## Setup: `wizard gateway setup`

```bash
wizard gateway setup
```

One interactive command that walks the whole thing: token, check the token, find your chat id, write it down. It is the recommended way to get a bot working, and the only one that does not involve reading a number out of a log.

```
Wizard gateway setup — Telegram

No bot token yet. To create one:
  1. open Telegram and message @BotFather
  2. send /newbot and answer its two questions
  3. copy the token it replies with (like 123456789:AA…)

Paste the bot token: 123456789:AA…
Stored in /home/you/.wizard/credentials.toml (mode 0600). It is never written to config.toml.
Bot: @your_bot (Your Bot)

Now open Telegram and send any message to @your_bot.
Waiting up to 180 seconds for it (Ctrl-C to stop)…

Message received. Chat id 123456789 (private), from Teddy Tennant (@teddy).

Add chat 123456789 to gateway.allowed_chat_ids in /home/you/.wizard/config.toml? [y/N] y
Added chat 123456789 to gateway.allowed_chat_ids in /home/you/.wizard/config.toml.

Done. To run the gateway:
  cd <your project> && wizard --gateway   # foreground, Ctrl-C to stop
  wizard gateway install                  # keep it running in the background
Then send /help in the chat to see what it understands.
```

What each step does, and what it will not do:

- **The token.** If Wizard already has one — pasted during `wizard --onboard`, written into `credentials.toml`, or exported as `$WIZARD_TELEGRAM_TOKEN` — it is used and nothing is asked. The precedence is the gateway's own (stored credential first, then the env var); setup does not have a second one. A token you paste here goes to `~/.wizard/credentials.toml` at mode 0600 and nowhere else: never into `config.toml`, never into a log, never into an error message.
- **The check.** `getMe` runs before anything is written to `config.toml`, so a mistyped token fails here, in one line, rather than at the first poll of a running service. A token that setup itself stored and that Telegram then rejected is removed again, so a bad paste cannot sit in the store shadowing a working env var.
- **The chat id.** Setup first discards whatever is already queued for the bot and says how many messages that was, then waits for one you send while it is watching. That is deliberate: a bot's username is public, and the id it offers you has to be the one you just sent, not a stranger's message that was sitting in the backlog. It reports the chat id, the chat type, and who sent it. Ctrl-C leaves at any point; it gives up on its own after 180 seconds.
- **Writing it down.** Only after you answer `y`. It edits `config.toml` as text, so comments and every other key survive — nothing else in the file moves. Setting `kind = "telegram"` comes with it. An id that is already on the list is not added twice.
- **Group chats.** A negative chat id is a group or supergroup, and setup says so, in the same words `wizard --gateway` and `wizard doctor` use, *before* the question — see [below](#the-allow-list). It does not refuse: allow-listing a group you control is a legitimate thing to do deliberately.

Setup is interactive. With stdin or stdout not a terminal — a pipe, a CI job, a systemd unit — it refuses and prints the two things to do by hand instead, rather than blocking on a question nobody will answer or taking a stray byte on a pipe as consent to allow-list a chat. It works on hosts where the service verbs do not (Termux, a Linux without systemd): it writes no unit and asks no supervisor anything.

Run it again any time to add another chat id.

## Setup by hand

1. Create a bot with [@BotFather](https://t.me/BotFather) and copy the token.
2. Give Wizard the token (checked in this order):
   - **Preferred:** paste it during `wizard --onboard` when you pick Telegram; it is stored under `telegram` in `~/.wizard/credentials.toml` (file mode 0600).
   - Or write it yourself:

     ```toml
     # ~/.wizard/credentials.toml  (chmod 600)
     [keys]
     telegram = "123456:ABC-..."
     ```

   - Or export it in the gateway's environment (`WIZARD_TELEGRAM_TOKEN` by default). The env var is only consulted when no non-empty `telegram` credential is stored.
3. Add a `[gateway]` section to `~/.wizard/config.toml`, or pick Telegram in onboarding (`wizard --onboard`), which writes the same thing:

```toml
[gateway]
kind = "telegram"
allowed_chat_ids = [123456789]
```

4. **Start the gateway and keep it running:**

```bash
cd ~/your/project
wizard --gateway
```

## The allow-list

**`allowed_chat_ids` is not optional.** The allow-list is closed: a chat id must be listed explicitly, and an empty list allows nobody. A gateway turn runs in sovereign posture with the full tool set on this machine, so "no list configured" means "refuse everything", not "allow everything". A message from an unlisted chat gets `unauthorized: this chat is not allowed` back and nothing runs.

**It authorises a chat, not a person — so use one-to-one chats.** Wizard checks the chat id and nothing else; the sender is never part of the decision (it is read only so `wizard gateway setup` can print a name next to the id it found). Allow-list a group or supergroup id (they are negative, like `-1001234567890`) and *every member of that group* can drive sovereign turns on this machine — including anyone added to it later, by anyone in it who can add people. There is no per-user restriction to fall back on. `wizard gateway setup` warns before it writes such an id, the gateway prints the same warning at startup, and `wizard doctor` says so too, but none of them refuses: it is a legitimate thing to do deliberately, on a group you control, understanding that group membership is now equivalent to a shell account on this box.

To get your own chat id, run `wizard gateway setup` — it asks you to message the bot and reports the id. Failing that, there are two manual routes:

- Start the gateway with the list still empty, message the bot once, and copy the id out of the refusal line the gateway prints on stderr:

  ```
  refused chat 123456789 (not in gateway.allowed_chat_ids, which is empty, so every
  chat is refused); add it to ~/.wizard/config.toml to allow this chat
  ```

  Add it to `allowed_chat_ids` and restart.
- Or read `chat.id` from `https://api.telegram.org/bot<token>/getUpdates` after messaging the bot. Note that only one process may long-poll a bot at a time: Telegram answers 409 to the second, so stop a running gateway (and finish any `wizard gateway setup`) first.

The allow-list is read once, when the gateway starts, so an edit needs a restart — including one `wizard gateway setup` just made. On startup with an empty list the gateway prints a warning saying every message will be refused; with a non-empty one it prints the ids it will accept.

## Always-on: `wizard gateway install`

The gateway does not daemonize itself, so instead of keeping a terminal (or a tmux session) open, install it as a background service:

```bash
cd ~/your/project
wizard gateway install
```

That writes a systemd **user** unit on Linux (`~/.config/systemd/user/wizard-gateway.service`) or a launchd LaunchAgent on macOS, enables it, starts it, and gives you your prompt back. It captures the directory you ran it from as the gateway's project root, and points the unit at the absolute path of the running binary. Then:

```bash
wizard gateway status      # installed? running? since when? last error?
wizard gateway logs -f     # the journal, or ~/.wizard/logs/wizard-gateway.log on macOS
wizard gateway restart     # after editing config.toml, or after replacing the binary
wizard gateway start
wizard gateway stop
wizard gateway uninstall   # stop, disable, remove the unit
```

After `wizard update`, restart the service so it picks up the new binary. See [Services](services.md#after-replacing-the-binary).

Two things worth knowing up front, both covered in full by [Services](services.md):

- **The bot token.** A service inherits no environment, so a token that only exists as `export WIZARD_TELEGRAM_TOKEN=…` in your shell would never reach it. `install` copies it into `~/.wizard/credentials.toml` (mode 0600) and says so; it is never written into the unit, which is world-readable.
- **Lingering.** On Linux a user service stops when you log out unless lingering is on. `install` checks and prints the exact `sudo loginctl enable-linger <you>` to run.

On Termux and on Linux without systemd, the service verbs refuse — every one of them, not just `install` — and name what to use instead (`termux-services`/runit, or your own supervisor) rather than writing a unit nothing will read. See [Services](services.md). `wizard gateway setup` still works there: it writes no unit and asks no supervisor anything.

`install` also refuses outright when it can find no bot token at all, and warns when `allowed_chat_ids` is empty, rather than installing a service that would crash-loop or answer nobody. Run `wizard gateway setup` first and neither applies.

The unit's `RUST_LOG=info` is inherited by anything the gateway spawns; it does **not** select what Wizard itself logs. Wizard's own diagnostics go to `~/.wizard/logs/<timestamp>-<pid>.jsonl` and are filtered by `WIZARD_LOG` (see [Logs](logging.md)), which `install` carries into the unit when it is set in your shell. What the gateway prints itself, including the allow-list warning and every refusal, goes to the journal, which is what `wizard gateway logs` shows. Reading a chat id out of a refusal there is the fallback, not the procedure: `wizard gateway setup` reports it directly.

To write a unit yourself instead (a system unit, a different user, a hardened sandbox), `contrib/wizard-gateway.service` is still there as a starting point; set `WorkingDirectory` to your project and `ExecStart` to the absolute path of your binary.

## `[gateway]` config keys

| Key | Default | Meaning |
|-----|---------|---------|
| `kind` | `"none"` | Which transport to run: `"none"` (terminal only; `--gateway` errors with instructions) or `"telegram"` |
| `token_env` | `"WIZARD_TELEGRAM_TOKEN"` | Name of the env var holding the bot token, consulted when no `telegram` entry exists in `~/.wizard/credentials.toml`. Only the *name* is stored; the token itself is never persisted to config |
| `allowed_chat_ids` | `[]` | Inbound chat IDs allowed to drive the agent. The list is closed, so **the default empty list refuses every message**: set it or the bot answers nobody. Unauthorized chats get an "unauthorized" reply, the refusal (with the chat id) is printed and logged, and nothing runs |

To fill `allowed_chat_ids`, run `wizard gateway setup`: it discovers the id from a message you send and, with your say-so, writes it here without disturbing the rest of the file.

## Behavior

- **Access control.** Every inbound message is checked against `gateway.allowed_chat_ids` before anything else happens to it. The check is fail-closed: an id that is not on the list is refused, and an empty list refuses everyone. A refused message costs nothing beyond the one refusal: no attachment is downloaded, no chat action is sent, no agent turn starts. Refusals are printed and logged with the chat id (so it can be copied into the config, which is why the gateway's output is worth keeping under systemd) and answered with a flat "unauthorized" that says nothing about the machine.
- **Transport.** Long-polls `getUpdates` (30 s window, `allowed_updates=["message"]`) and replies via `sendMessage`. Sends a `typing` chat action while the agent turn runs. Transient network errors are retried with jittered exponential backoff (`retry_base_secs` / `retry_max_secs` from the top-level config) **forever** — a DNS failure, a 502, a laptop that slept, a revoked token: none of them ends the loop, because a bot that gave up is a bot that stopped answering with nothing in the chat to say why. Telegram answers a second long-poller with 409 Conflict, which is reported by name (stop the running gateway, or finish `wizard gateway setup`, and it recovers on its own).
- **Nothing is dropped when a poll is interrupted.** The gateway polls *while* a turn runs, so the poll in flight is abandoned the moment that turn finishes. `getUpdates` has already advanced Telegram's cursor by then, so updates it had fetched but not yet handled are held on the transport and finished by the next poll instead of being lost. The cost of that is a duplicate attachment download, or a repeated "unsupported message type" line, in the rare case the interruption lands mid-download.
- **Outbound failures degrade, they do not vanish.** Replies go out as HTML; a message Telegram will not parse is resent as plain text, so a conversion bug costs formatting rather than the answer. A 429 is waited out on the `retry_after` Telegram states, and a 5xx on a short ladder, bounded by four attempts and a ten-second budget so a flood-wait cannot wedge the loop (which would also make `/stop` unhearable). A 4xx that is not a rate limit is a refusal and is not retried. If parts of a reply still do not land, the chat is told how many — an answer with a silent hole in it reads as the agent having thought something odd.
- **A bad turn is reported, not fatal.** A turn that returns an error answers with the error. A turn that *panics* — a tool indexing past the end of something the model produced, an `expect` on a tool's output — is caught, reported into the chat ("that turn hit a bug and stopped… the conversation is intact"), and the loop keeps serving. The panic and its backtrace still go to stderr, so it is still a bug worth chasing; it just no longer takes the service with it. The same guard covers slash commands and `session_start` hooks.
- **One turn per message.** Each text, caption, photo, or image-document message runs a full agent turn (tools, file edits, shell) and the final response is sent back. A captioned non-image document counts too: the caption is the prompt, and with no caption text the prompt becomes `Please look at the attached file (<name>).` Photos/documents are downloaded under `~/.wizard/gateway-attachments/` (0700, each file 0600 and never written over a name that already exists) and the agent prompt includes `[attached: /absolute/path]`; a download that fails still delivers the caption, without the attachment. On a host where no state directory can be resolved at all (a hardened unit with `ProtectHome=yes` and no `WIZARD_HOME`, a container with no home directory) that becomes `<system temp dir>/wizard-gateway-attachments`, with the same modes but outside `~/.wizard` and at a guessable path. Photo-only messages use the prompt `Please look at the attached image.` Stickers, voice, and other unsupported types get `unsupported message type — send text, a photo, or an image document` instead of silence. Replies are capped at 24,000 characters (anything past that ends `… (reply truncated)`) and split into Telegram-sized chunks (≤ 4,000 characters, breaking on line boundaries).
- **In-chat commands.** The chat runs the same command table every other surface does — `/status`, `/plan`, `/omakase`, `/cost`, `/clear`, `/help` and the rest — and publishes it to Telegram's own `/` menu, so the menu can never offer something the gateway would refuse. Commands that need a screen (`/vim`, `/ui`, `/dashboard`) or a terminal (`/quit`) refuse by name. A leading slash alone does not make a command: `/etc/hosts` and `/deploy the release to prod` are prompts for the model, because a chat is where people paste paths.
- **`/stop` and `/ping`.** Two controls the gateway answers itself, because the table has nothing to say about either. `/stop` is this chat's Ctrl-C: it interrupts the turn in flight, leaves the session intact, and the next message carries on. `/ping` answers immediately — from the poll loop, jumping the backlog — with uptime, how long ago the last poll came back, messages served, whether a turn is running, how many messages are queued behind it, and any run of consecutive poll failures. It is the one way to tell a busy bot from a wedged one *from the chat*, which is where you are when a bot has gone quiet. Both are allow-listed like everything else: a stranger's `/ping` learns nothing.
- **One turn at a time, and messages queue.** There is a single agent, so a message that arrives during a turn waits in arrival order and is told it is waiting rather than vanishing. `/stop` and `/ping` are the exceptions and are acted on immediately.
- **Step budget.** The gateway runs in sovereign posture, so a *capped* `max_steps` below 100 is raised to 100; there is no human to hand back to mid-task. The default (`max_steps = 0`, no limit) is already more permissive and is left alone: a turn runs until the model stops calling tools.

## Is it still there?

A working gateway with no traffic prints nothing, for days. That is indistinguishable from one whose process exited, whose poll loop wedged, or whose allow-list refuses the only chat that talks to it — and you normally find out which by messaging the bot and getting nothing back. Three things answer the question:

- **`/ping` in the chat.** The fastest answer, and the only one available when all you have is a phone. It is answered by the poll loop itself, ahead of anything queued, so a reply proves the loop is turning right now — not that it was turning when the current turn started.

  ```
  gateway alive — up 2d7h, last poll 4s ago, 118 message(s) served, a turn is running, 1 waiting
  ```

  `last poll` is the useful number: a gateway with no traffic for a week is healthy, one whose last poll was an hour ago is not. A run of consecutive poll failures is appended when there is one, so "alive but not reaching Telegram" does not read as "fine".

- **The heartbeat.** Every ten minutes the gateway prints the same line to stdout, which under a service is the journal. A gap in it is the thing to look for.

  ```bash
  wizard gateway logs -f
  ```

- **`wizard doctor`**, below, for the configuration rather than the liveness.

## Diagnose with `wizard doctor`

When `gateway.kind = "telegram"`, doctor reports:

- gateway kind
- whether a telegram token is present (credentials or env; never prints the secret)
- whether `gateway.allowed_chat_ids` names at least one chat — an empty list is a hard failure, since the bot would answer nobody
- whether a `wizard --gateway` process appears to be running

It also fails when a telegram token is stored but `gateway.kind` is still `"none"`.

```bash
wizard doctor
```

## It is a plugin

The gateway ships behind `--features gateway`, on by default, so every release
binary has it. Leaving it out is a build somebody assembled deliberately, and on
one of those both `wizard --gateway` and `wizard gateway <verb>` print a line
naming the flag that puts them back rather than failing to parse — `clap` owns
the subcommand either way, so `wizard --help` keeps listing it.

What stays in core when the plugin is gone is the part that outlives it:
`[gateway]` in `config.toml` still parses and round-trips (a config file that
was valid yesterday must not stop being valid because of a build flag),
onboarding still offers to store a bot token, and `wizard doctor` still checks
the allow-list and still warns about a group chat id in it. See
`docs/plugins.md`.
