# Changelog

Notable changes, newest first. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases before 2.0.0 (v1.6.0 through v1.8.0) predate this file; their notes are on their [GitHub release pages](https://github.com/teddytennant/wizard/releases).

## [Unreleased]

### Added

- **Code mode: `run_code`, one LuaJIT program per call, able to call Wizard's own tools.** Off by default (`code_mode = true`, or `WIZARD_CODE_MODE=1`). It is for the case where three or more calls would otherwise be a fixed sequence: read forty files and print the three that match, in one call, with only those three lines entering the context. The interpreter is the one already embedded for scripted tools, so there is nothing to install. Tools are called as `tool.read_file{path="x"}` and go through the same dispatch pipeline a direct call does — hooks can rewrite or veto them, and edits are snapshotted for `/rewind`. Nothing survives the call: no globals, no functions, no loaded data, because a Lua heap cannot follow a compaction, a `/rewind`, a `/resume` or a `/fork`. Never registered for a model without native tool calling, whatever the config says. See [docs/code-mode.md](docs/code-mode.md).

### Fixed

- **The sandbox's time bound now actually stops a runaway loop.** It turned the JIT off with `jit.off(true, true)`, which selects the *calling* function rather than the compiler — so it disabled compilation for a one-line chunk that had already finished, and left it on for the script. A registry tool's `while true do end` compiled into a trace on its second pass, where the instruction hook does not fire, and burned a core for the life of the process; only the wrapper's wall-clock timeout noticed, and that abandons the thread rather than stopping it. An allocating loop still tripped, because `string.rep` is a C call the recorder will not trace, which is what made the bound look installed. It is `jit.off(); jit.flush()` now.
- **The compiler switch is gone from a bounded chunk, so the time bound cannot be turned off from inside it.** Turning the JIT off is what makes the instruction hook fire at all, and `jit` stayed reachable in the globals — `jit.on()` re-enabled the compiler, the hook went quiet, and the deadline, the memory ceiling and the cancel handle all stopped applying. That reached both profiles: a registry-installed (stranger-authored) scripted tool, and a code-mode program. `disable_jit` now replaces the table with a frozen one that keeps `version`, `os`, `arch` and a truthful `status`, and repoints `package.loaded.jit` at it.
- **A bound cannot be swallowed by `pcall`, and a coroutine is bounded too.** The hook signalled a bound with an ordinary catchable Lua error and nothing rechecked afterwards, so `while true do pcall(f) end` turned every bound into a return value — and wrapping calls in `pcall` is the idiom code mode's own documentation recommends. `pcall`, `xpcall` and `coroutine.resume` now re-raise once a bound has latched; an ordinary `error()` is caught exactly as before. Separately, coroutines were not hooked at all: mlua's per-thread hook is keyed by thread, and its trampoline *uninstalls* the hook when it cannot find one — which on LuaJIT, where the hook mask is global, took the bounds away from the whole VM. The hook is installed globally now.
- **A sandboxed script that runs out its budget is stopped, not abandoned.** The in-VM deadline and the wrapper's wall clock were armed for the same instant and the wrapper won every time, because its clock starts before the worker thread does — so the caller got a plain timeout and the chunk kept running for the life of the process, which is what the deadline existed to prevent. The wrapper now allows the hook two seconds to do the stopping, and only reports a timeout when the hook could not (a chunk parked in a C call), which is the last resort it was written to be.

- **Mid-turn `compact` no longer folds one earlier note and leaves the tool tail.** The kept tail can now start on an assistant message, so a long in-flight tool loop is cut to the token budget instead of walking back to the only user prompt. That walk is what made one session spend an hour summarizing a single message 69 times while pressure stayed elevated.
- **`wizard update` builds from source when it cannot install a prebuilt.** A binary compiled with the placeholder signing key, or a host no published asset runs on (NixOS without a static musl loader, Termux), used to refuse every release. It now clones the tag and `cargo build --release --locked`, the same trust as `WIZARD_BUILD_FROM_SOURCE=1`. A failed signature or digest is still fatal. Background auto-update stays download-only.

### Changed

- **A registry-installed (sandboxed) scripted tool runs interpreted, and is slower for it.** The JIT fix above is the first time the compiler is really off for those tools, which costs roughly 5x on a tight numeric loop (measured: 462 ms against 101 ms for a 40M-iteration loop). A tool whose manifest leaves `timeout_secs` unset now has that much less work available inside the default timeout. Locally authored tools under `~/.wizard/tools` are unbounded and unaffected — they keep the compiler.
- **Skills are an index, not a dump.** The system prompt lists each skill's name, description, and path. The body is read from disk when the skill matches. A skill can set `always: true` to inline its body; that is the exception. A long skill no longer rides along on every turn that is not using it.

## [2.0.1] - 2026-08-12

The first patch on the 2.0 line. Compaction stops looping on a still-full window, tools stop flooding the context with listings, the composer stays with the agent while a command that will never prompt is running, and xAI's default is grok-4.6.

### Added

- **grok-4.6 is the default xAI model.** Both Chat Completions and OAuth point at it, it is first in the onboarding picker, and its 500K context window and published rates are mapped. grok-4.5 stays available behind it.

### Changed

- **Compaction keeps a token budget, not a message count.** The kept tail is the newest messages that fit 40% of the window, still capped at 10. A pass that used to leave the window over the 80% trigger (and fire again on the next step, 302 times in one session) now leaves it half empty. The summarizer's tokens are recorded in `wizard usage`. A span that is mostly a note an earlier pass wrote is left alone rather than fed back through the model.
- **Stale `read_file` results are stubbed** when a later `write_file` or `edit_file` changed the same path, so the wrong contents stop riding along on every step until a full pass happened to sweep them up.
- **Tools have their own output budgets.** `execute`, `read_file`, web fetches and manual sections keep 30 KB. `git_diff` gets 16 KB, `search_files` 12 KB, listings and `git_status` 8 KB, and a tool's error text 4 KB.
- **The Anthropic preamble is cached for an hour.** Tool schemas and the system prompt are written once a session; the previous five-minute TTL dropped them on any pause and cost a full cold write at 1.25x. History breakpoints stay on five minutes.
- **ACP speaks agent-client-protocol 2.0.** The stdio agent uses the builder + handler model, and long prompts are spawned off the dispatch loop so session/cancel still works.
- **Dependencies.** ratatui 0.30.2 (with ratatui-image 10 and crossterm 0.29), mlua 0.12, agent-client-protocol 2.0, ed25519-dalek 3, getrandom 0.4.2, plus a cargo-minor-and-patch sweep.

### Fixed

- **A command that never prompts no longer takes the composer.** Every foreground command opened a console and the TUI switched Enter into it; `ls` and `cargo build` held the agent hostage for as long as they ran. The surface still claims the writer the moment the console opens, but it keeps the console aside until the tool reports the command is waiting.

## [2.0.0] - 2026-08-10

The 2.0 line: the browser GUI is replaced by a window that links the agent core in process, the gateway becomes something you can leave running, a project's own hooks stop running unasked, and every long-running surface stops dying on failures it could have ridden out. It is a breaking release, and three of the breaks are silent — nothing errors, the thing just stops happening. Read [Breaking changes](#breaking-changes) before upgrading a machine that is doing work for you.

### Breaking changes

- **The gateway allow-list is fail-closed.** In 1.8.0 `gateway.allowed_chat_ids` was fail-open: an empty list allowed every chat (`is_authorized` returned `allowed.is_empty() || allowed.contains(&chat_id)`), and the documentation said so. A gateway turn runs in sovereign posture with the full tool set on the machine it is on, so "no list configured" now means "refuse everything".

  **What breaks:** a bot that was working on an empty list goes mute. Every message gets `unauthorized: this chat is not allowed`, no turn runs, and the only trace is the refusal line in the journal.

  **Remediation:** run `wizard gateway setup`, which discovers your chat id by having you message the bot and writes it down; or add it by hand and restart the gateway (the list is read once, at startup):

  ```toml
  # ~/.wizard/config.toml
  [gateway]
  kind = "telegram"
  allowed_chat_ids = [123456789]
  ```

  The id is in the refusal line if you would rather read it out of the log: `refused chat 123456789 (not in gateway.allowed_chat_ids, which is empty, so every chat is refused)`. A negative id is a group, and allow-listing one gives every member of that group — including anyone added later — the equivalent of a shell account on that box. See [gateway.md](docs/gateway.md#the-allow-list).

- **A project's own hooks need a trust decision before they load.** `<project>/.wizard/hooks.toml` arrives with a `git clone`, and `session_start` fires before the model has said a word, so a repository that ships one is shipping code that would run the moment you launch Wizard in its directory. Cloning is not consent, so there is now a gate (`src/trust.rs`, new in this release). `~/.wizard/hooks.toml` is yours by construction and is unaffected.

  **What breaks:** an existing `<project>/.wizard/hooks.toml` that has been firing for a year stops firing, silently, until the project is trusted. On the TUI you get one question. So do `wizard -p`, `--mode sovereign` and `--continuous`, provided they are in the foreground on a real terminal and printing text — the terminal facts are checked, so a pipe on stdin or a background process is not asked. On every surface that cannot ask — those same runs when unattended, `--output-format json`/`stream-json`, the gateway, `wizard gui`, `wizard acp`, `wizard fleet`, scheduler daemon jobs, CI — the answer is no and the reason goes to stderr.

  **Remediation:** start Wizard once interactively in that directory and answer `y`. One yes settles it for every other surface afterwards; it is recorded in `~/.wizard/trusted_projects` and keyed to the canonicalised project root and a sha256 of the hooks file, so editing that file re-opens the question. For a machine that has no interactive first run — a CI job, a systemd unit running the gateway on a repo you control — set `WIZARD_TRUST_PROJECT=1` for that process instead. It answers an open question only: a recorded **no** outranks it, and to be asked again you delete that project's line from `~/.wizard/trusted_projects`. See [hooks.md](docs/hooks.md#project-trust).

- **The graphical release assets are renamed `wizard-desktop-*` to `wizard-native-*`.** 1.8.0 published `wizard-desktop-<target>.tar.gz` for the four gnu and darwin targets; 2.0.0 publishes `wizard-native-<target>.tar.gz` in exactly those slots, because the binary inside is a different program (see the next entry).

  **What breaks:** an already-installed `wizard-desktop` 1.8.0 binary updates itself by asset name. It will look for `wizard-desktop-*` in every future release, find nothing, and report a 404 forever. Nothing about the failure says the asset was renamed.

  **Remediation:** reinstall, then delete the old binary. There is no in-place path from one to the other.

  ```bash
  curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
    | WIZARD_NATIVE=1 WIZARD_BUILD_FROM_SOURCE=1 bash
  rm "$(command -v wizard-desktop)"
  wizard-native gui
  ```

  `WIZARD_BUILD_FROM_SOURCE=1` is one way to skip the signature check by building from a git ref. `WIZARD_APP=1`, the old spelling, is honored as a deprecated alias for `WIZARD_NATIVE=1` so an existing provisioning script gets a window rather than silently getting nothing. `wizard update` keeps the two apart (`native_assets` in `src/update.rs`): a native build updates to a native asset and never to the plain one.

- **`wizard app` is removed, the browser GUI is deleted, and `wizard gui` no longer takes `--port`, `--no-open` or `--assets`.** The window is now a native [iced](https://iced.rs) application that links the agent core in process: no webview, no loopback HTTP server, no port, and no JSON round trip per streaming token. What went with the page it replaced: the axum server, the WebSocket protocol, `gui/assets/`, and `docs/gui-protocol.md`, the ~500-line document that specified those frames for integrators. There is no replacement protocol, because there is no longer a socket to speak it on.

  **What breaks:** `wizard app` exits with an unrecognized-subcommand error. `wizard gui --port 4680`, `--no-open` and `--assets <dir>` are hard clap errors rather than ignored flags. Anything that drove Wizard by connecting to `127.0.0.1:4680` and speaking the GUI WebSocket protocol has nothing to connect to. The `desktop` cargo feature is gone, replaced by `native`; `--features desktop` fails to build.

  **Remediation:** `wizard gui` from a build with `--features native`, or `wizard-native gui` from the installer. `wizard gui --native` is still accepted and does nothing, because that spelling is written into aliases and scripts from the period when a plain `wizard gui` served a page. For driving Wizard from another program, the supported seams are `wizard acp` (Agent Client Protocol, for editors), `wizard mcp-serve` (Wizard's tools over stdio MCP), `wizard -p` with `--output-format json` or `stream-json`, and the Telegram gateway. See [native-gui.md](docs/native-gui.md), and [What went with the browser GUI](docs/native-gui.md#what-went-with-the-browser-gui) for the full list of what was ported and what was cut on purpose.

- **The installer and the updater refuse a release they cannot verify.** Releases are now signed with minisign: `install.sh` fetches `checksums.txt` and `checksums.txt.minisig`, verifies the signature under the public key inlined in the script, and verifies each asset's SHA-256 against that file. Every failure aborts — no signature, a bad one, one from another key, no `checksums.txt`, no entry for that asset, a digest mismatch, or a host with neither `sha256sum` nor `shasum`. `wizard update` applies the same rules with the key compiled in.

  **What breaks:** a scripted `curl … | bash` that used to install a binary now exits non-zero on any of the above. Verifying a signature needs `minisign` on PATH or an OpenSSL that does ed25519 and blake2b; macOS ships LibreSSL, which has neither, so `brew install minisign` first. 
  **Remediation:** install `minisign`, or add `WIZARD_BUILD_FROM_SOURCE=1`, which skips the signature question entirely by building from a git ref.

- **The Home Manager module is exported as `homeModules.default`.** 1.8.0 exported it as `homeManagerModules.default`, which is not a flake output name Nix knows, so `nix flake check` reported it as unknown and checked nothing inside it. `homeModules` is the name Home Manager settled on and the one it reads first.

  **What breaks:** nothing, on purpose. `homeManagerModules.default` is kept as an alias for the new attribute, so an existing `imports = [ inputs.wizard.homeManagerModules.default ];` keeps evaluating; Nix prints a warning about the unrecognized output and carries on.

  **Remediation:** none required. Switch the import to `inputs.wizard.homeModules.default` when convenient — that is the name the module will keep, and it is the one `nix flake check` actually checks.

### Added

- **The window (`wizard gui`).** An iced window over the agent core: one process, one binary, no webview, no port. It folds the same `AgentEvent`s into the same `TranscriptModel` the TUI reads, so it is the same agent on another surface rather than a reduced one. It carries settings, onboarding and OAuth, the git rail and diff pane, the session picker, the subagent rail, the context meter, the todo checklist, the command palette, the gate modals, the attachment tray and the image pane; it adds a transcript you can select across and a console that can answer a command that asks a question. Off by default (`--features native`) and shipped as its own binary, `wizard-native`. See [native-gui.md](docs/native-gui.md).
- **UI skins (`/ui`).** The TUI can wear another coding agent's terminal chrome: `wizard` (the house look), `codex`, or `grok`. Pick one at onboarding, change it live with `/ui <name>`, or cycle the Interface row in `/settings`; it persists as `[ui] skin`. A skin owns its whole frame and its own palette, so chrome and colours travel together. See [ui-skins.md](docs/ui-skins.md).
- **Mesh (`wizard peers`).** Other machines running Wizard, what each advertises, and what this machine has decided about each one: `address`, `add`, `list`, `trust`, `forget`, `ping`, `refresh`, `watch`. Trust is three-state (trusted / known / blocked). The QUIC listener is **off by default** — nothing accepts a connection until `[mesh] listen = true` — and there is no NAT traversal, no relay, and no delegated work; the scope is deliberately narrow. See [mesh.md](docs/mesh.md).
- **Answering a command that prompts.** A shell command the agent runs can ask the user a question and get an answer. Previously every spawned command got `/dev/null` on fd 0, so an installer asking `Do you want to continue? [Y/n]` read EOF and either aborted or spun, with its output buffered until it exited — the visible symptom was a hang, then a timeout. Both halves are fixed together, in the TUI and in the window's console. See [interactive-commands.md](docs/interactive-commands.md).
- **`wizard gateway setup`.** One interactive command that walks the whole first run: find or ask for the bot token, check it against Telegram with `getMe` before writing anything, discover your chat id by having you message the bot, and — only after a `y` — write that id into `allowed_chat_ids` as a text edit that leaves the rest of the file alone. It refuses without a terminal rather than taking a stray byte on a pipe as consent.
- **The gateway as a service.** `wizard gateway install | start | stop | restart | status | logs | uninstall` writes and drives a systemd user unit on Linux or a launchd LaunchAgent on macOS, copies the token into `credentials.toml` (never into the world-readable unit), and checks lingering. `wizard scheduler` gained the same verbs. On Termux and on Linux without systemd every verb refuses by name and says what to use instead, rather than writing a unit nothing will read. See [services.md](docs/services.md).
- **`/stop` and `/ping` in the chat.** `/stop` is the chat's Ctrl-C: it interrupts the turn in flight and leaves the session intact. `/ping` is answered by the poll loop itself, ahead of the backlog, with uptime, how long ago the last poll came back, messages served, whether a turn is running, the queue depth, and any run of consecutive poll failures — the one way to tell a busy bot from a wedged one from the chat, which is where you are when a bot has gone quiet. The same line is printed every ten minutes, so a gap in an otherwise empty journal is visible.
- **The gateway runs the command table it advertises**, publishes it to Telegram's own `/` menu, and sends formatted replies (HTML, falling back to plain text on a parse refusal). A leading slash alone does not make a command: `/etc/hosts` is a prompt, because a chat is where people paste paths.
- **`wizard doctor --bundle`.** A redacted bug-report bundle under `~/.wizard/bundles/doctor-<timestamp>/`: the check report, the allowlist-redacted config, the newest session transcript, the usage and evolution logs, and the most recent debug logs. Secrets are stripped; the transcript is your own text, so read it before attaching it to anything.
- **Structured file logging.** Diagnostics go to `~/.wizard/logs/<timestamp>-<pid>.jsonl`, filtered by `WIZARD_LOG`. See [logging.md](docs/logging.md).
- **The skills and tools registry client (`wizard skills search | install | update | list`).** Git-backed, no backend and no accounts. The public registry it points at is not published yet, so today it installs from any `registry.json` you point `WIZARD_REGISTRY_URL` at. See [market.md](docs/market.md).
- **`wizard resume`**, the subcommand spelling of `--resume`, and **`wizard resume --claude`**, which takes a conversation from Claude Code's own history (`~/.claude/projects/`), converts it into a Wizard session and continues it here. A Claude Code transcript is a DAG rather than a list, so `--leaf` picks which conversation inside a forked session to walk back from. The Claude Code side is strictly read-only.
- **`/resume-claude`**, the same import as a slash command. It opens the picker `/resume` opens, with the same keys, over the conversations Claude Code recorded for the working directory; Enter imports the selected one and continues it, and everything after the import is the ordinary resume path. In the window it unfolds the sidebar's Claude Code section, which is where those rows already live. It is a separate command rather than a row inside `/resume` because opening one is a different act: a `/resume` row reopens a file Wizard owns, and this one copies a conversation out of another program. `wizard resume --claude` remains the shell spelling and is still what a script wants.
- **Buzz rooms.** Join [Buzz](https://github.com/block/buzz) as an ACP member via `buzz-acp` or a Desktop custom harness. See [buzz.md](docs/buzz.md).
- **`x_search`**, a native tool for searching X/Twitter through xAI, by OAuth account or API key. Read-only, so it stays available in plan mode. See [web.md](docs/web.md#x_search).
- **Computer use — the `computer` tool.** Desktop control on Linux and macOS: screenshot, mouse move, click, drag, type, key chords and scroll, in real screen-pixel coordinates. A screenshot reports the true screen size and rides back to the model as an image on a follow-up user message, so a vision-capable model can look, act and look again; a text-only model can still act on coordinates it is given but cannot read the screen. Input goes through ydotool (uinput, so Wayland and X11 both work) with grim/maim/ImageMagick for capture on Linux, and CoreGraphics with `screencapture` on macOS, reported in logical points so clicks land correctly on Retina displays. `wizard desktop-setup` detects the distribution (apt/dnf/pacman/zypper), installs what is missing, drops the uinput udev rule, adds you to `input` and enables `ydotoold`; on NixOS it prints the configuration to add, and on macOS it names the Accessibility and Screen Recording permissions to grant. The tool is `Execute` access, so plan mode refuses it and a read-only subagent never gets it — but nothing prompts before an action: this is real control of your machine, with your privileges, and no per-action gate stands between the model and your desktop. [SECURITY.md](SECURITY.md) applies. See [computer-use.md](docs/computer-use.md).
- **Release signing (minisign) and a download mirror.** `WIZARD_MIRROR` is tried before GitHub Releases and falls back to it, and it cannot be a weaker path: a mirror's bytes face the same signature and checksum gates, and a fatal verification failure deliberately suppresses the GitHub fallback so a compromised mirror cannot be quietly routed around. See [SECURITY.md](SECURITY.md#release-signing).
- **Durable failure and liveness state for continuous missions.** `mission.toml` gains a phase/heartbeat stamp and a consecutive-failure count, so an operator can tell a long turn from a wedged one, plus `max_consecutive_failures` (default 5, `0` disables) as the bound on how many cycles in a row may end badly before a perpetual run gives up.

### Changed

- **A hard error or a tripped circuit breaker ends the cycle, not the mission.** A `--continuous` run rolls the cycle back, records it, backs off, and starts a cycle whose prompt names the failure and demands a different approach. `--loop N` keeps its old behaviour. Bounded by `max_consecutive_failures` so a genuinely broken setup still exits.
- **A signalled sovereign run winds down instead of dying.** The first SIGTERM/SIGHUP/SIGINT is treated as the loop-control file's `stop`: cancel the turn in flight, refuse to start another cycle, then write the final mission stamp, run `session_end` hooks, and flush a structured output stream. The second signal is left to the default handler, so a run wedged in a tool call is still killable by the key people reach for first. Previously `systemctl stop` or a closing terminal left a `mission.toml` claiming a cycle was still running.
- **Tool-failure breakers grade a result.** A tool that ran and reported a non-zero exit or a missing file is diagnostic signal; a tool that could not be run at all is a fault. Only faults trip the identical-call breaker, and it nudges at three repeats instead of tripping there — three identical `cargo test` failures is the middle of a fix, not a malfunction. The per-tool counter is cleared by a success of any tool, so a failing build interleaved with successful edits no longer walks to eight strikes across a session.
- **An endpoint outage is waited out rather than fatal**, with a per-trip escalating cooldown (30s doubling to a 15 minute cap, reset by a probe that succeeds) so waiting is not hammering, and a ceiling — half a day, or the run's own `--max-hours`, whichever comes first — because `error_is_transient` defaults an unrecognized error to transient and some permanent failures are indistinguishable from a provider being down.
- **A reply cut off by the context window compacts and retries**, rather than getting the advice meant for a reply cut off by the output ceiling, which cannot work when length was never the problem.
- **Subagents run the real agent loop** rather than a copy of it, and the TUI reads the shared transcript model instead of shadowing it — one implementation, one set of behaviours, on every surface.
- **`/cost` prices open-weight models by who served them**, so the same weights on two providers are not billed at one rate.
- **`/quit` and `/exit`** are both in the command table (`/q` is accepted by the parser but is not offered by tab-completion), and both refuse on the surfaces that cannot honour them: the gateway is a long-running service shared by every allow-listed chat, and one message must not take it down for the rest.
- **The agent's slash-command set is an explicit allowlist**, not "everything minus a few", and the refusals are broader than the obvious ones: anything that would park the agent at an interactive picker, end or rewind the session, set up providers, rewrite the binary, manage the local server, or repaint your terminal. See [usage.md](docs/usage.md#agent-run-slash-commands).
- **Documentation.** New: [native-gui.md](docs/native-gui.md), [graph-explorer.md](docs/graph-explorer.md), [mesh.md](docs/mesh.md), [ui-skins.md](docs/ui-skins.md), [services.md](docs/services.md), [interactive-commands.md](docs/interactive-commands.md), [logging.md](docs/logging.md), [buzz.md](docs/buzz.md), and `CONTRIBUTING.md`. `docs/gui-design-spec.md` is now marked as the design record it is and linked from [native-gui.md](docs/native-gui.md#where-the-look-is-specified).

### Deprecated

- **`WIZARD_APP=1`** is a deprecated alias for `WIZARD_NATIVE=1`, kept so an existing provisioning script installs a window instead of silently installing nothing.
- **`wizard gui --native`** is accepted and does nothing. It named the window back when a plain `wizard gui` served a page; a hidden no-op is a better answer than a clap error for a flag that is written into every alias from that period.
- **`WIZARD_BESPOKE=1`** remains a deprecated alias for `WIZARD_MINIMAL=1`.

### Removed

- **`wizard app`** and the webview shell behind it (tao + wry over the loopback GUI server), along with the `desktop` cargo feature and `docs/desktop.md`.
- **The browser GUI**: the axum server, the WebSocket protocol, `gui/assets/`, `wizard gui`'s `--port` / `--no-open` / `--assets`, and `docs/gui-protocol.md`.
- **`/theme` and the theme selection layer.** Colours resolve through `crate::theme` tokens exactly as before, but a palette is no longer chosen independently of the chrome around it: each skin brings its own, reachable by name only. `wizard doctor` reports the detected colour depth, which was the one genuinely useful thing `/theme` printed. (Neither the command nor the layer shipped in 1.8.0; both were introduced and removed within this cycle.)

### Fixed

- **A crashing turn no longer ends a TUI session.** A turn task that unwound sent no `Done`, so the status stayed busy, the agent never came back to its slot, and every later message queued forever. Turns now run inside `catch_unwind`: the crash lands in the transcript as a failed turn and the composer works. The same treatment covers the background tasks behind `/model`, a provider switch, `/fusion`, `/compact` and MCP connection, each of which used to leave its latch set for the rest of the session. A rebuild that came back without an agent is retried from the session file, bounded, and says plainly when it has stopped trying. One failed `Terminal::draw` is ridden out for three seconds before it is believed, and an event-handler error is a transcript line rather than the end of the process.
- **A crashing turn no longer ends the gateway either.** Turns, slash commands and `session_start` hooks run under a guard that turns a panic into an answer naming what failed; the loop keeps serving and the backtrace still reaches stderr. A turn that returns `Err` without emitting an error event no longer reads as `(done, no reply)`.
- **The gateway stops losing messages and replies at the transport.** `getUpdates` advances Telegram's cursor as soon as a batch is decoded, and the serve loop drops the poll future every time a turn finishes, so a routine cancellation used to discard messages Telegram considered delivered. Updates are now staged before anything is converted, and a resumed poll finishes them first. Outbound, a 429 is waited out on the deadline Telegram states and a 5xx on a short ladder, bounded by attempts and by a total budget so a flood-wait cannot wedge the select that keeps a turn interruptible; undelivered chunks are reported into the chat rather than only to a log nobody is reading.
- **A cut provider stream is a failure, not a silent success.** Every streaming decoder synthesized its own final chunk, so a connection that died mid-generation produced a clean `done: true` carrying whatever text had arrived — and the agent has no reason to retry a success. The decoders now track whether the provider ever *said* the reply was over ([DONE], a finish reason, `message_stop`, `done: true`) separately from having run out of bytes, and raise a typed transient error when only the second happened. This was the shape of the reported "it randomly stops".
- **Error objects carried inside a 200 stream are decoded**, rather than parsing fine and being ignored: an OpenAI-compatible gateway relaying an upstream 429/502 as a choice-less chunk, Anthropic's `event: error` overload, and Ollama's in-band `{"error": …}` line (whose "model not found" is now classified permanent instead of being retried for hours).
- **OAuth token endpoints get a timeout and a retry class.** The refresh path used a bare client with no connect or request timeout, so a token host that accepted the connection and went silent parked the turn for the life of the process while holding the token source's mutex. Refresh failures are now classified — an unreachable host retries, a revoked grant stops at once with the instruction, a 429/5xx carries its `Retry-After` — and the cache lock is held across the whole read-refresh-write, so two concurrent 401s can no longer spend a single-use refresh token twice and sign you out of a valid session.
- **Signing in over SSH works.** Both providers redirect to a fixed loopback address, and loopback is resolved by the machine running the browser, so over SSH the redirect landed on the laptop while the listener sat on the server and the flow had nothing to do but wait out five minutes.
- **The renderers stop panicking on small terminals.** Drawing every skin at every shape a terminal can be, over content chosen to break a width calculation, found three real panics mid-draw — the worst place for one, because it unwinds the frame and runs the panic hook that tears the terminal down. The status bar and the diff sidebar each built a height-1 rect without checking the area had a row, and Codex's session header computed a budget with a bare subtraction on a card narrower than its widest key.
- **The session ends when terminal input does.** When crossterm's event stream ended — stdin closed, the pty detached, an SSH session dropping — the reader task simply returned while the tick task kept repainting at full cadence over a session that could never receive another keystroke. A read error was worse: it retried immediately, spinning the loop. Input ending, or failing repeatedly, now quits with the reason in the transcript.
- **A killed turn hands the whole surface back.** The abort path and the crashed-task path each undid a different subset of what a turn had set up; the second closed the subagent panes and nothing else, leaving the composer typing into the stdin of a command whose console was gone — which looks exactly like Enter having stopped working.
- **Background tasks that take the agent out of its slot are bounded.** A rebuild, a model switch and `/compact` all park on things that can stop answering (a shared MCP mutex, a provider's `list_models`, one model call over the whole conversation); each now runs under a deadline, because an agent parked on a lock nobody will release was already lost.
- **Headless output survives a closed pipe.** `print!`/`println!`/`eprintln!` panic when the reader hangs up, so `wizard -p … | head` killed the printer task partway through a run. Every write goes through a helper that drops the line instead, and a printer task that died no longer turns a finished run into an error.
- **A failed `/rewind` could leave your files half-reverted.**
- **`/cost` billed every cached token at the full input rate.**
- **The context byte threshold no longer trips `critical` on a known window.**
- **`/bashes` was unavailable during the only turn that could fill it.**
- **The TUI ignored the keyboard inside tmux.**
- **Escape threw away the command you were typing.**
- **Vim-mode fixes:** line motions measured the whole draft rather than the line, Ctrl-K deleted lines the caret was not on, Ctrl-C warned nobody, and four more behaviours that differed from vim's.
- **A pile of TUI layout bugs:** a three-row terminal that looked like a hung application, a status bar cut mid-word by the frame edge, a slash menu that ran off its own border and then filled with ellipses, a picker printed on top of the composer, "N more" hints that ate the text underneath them, a deep project path that wrapped the whole usage table, a diff scrollbar sitting on the window frame, a deleted line that looked highlighted rather than deleted, and text selection that highlighted the wrong lines in a scrolled transcript.
- **`wizard update`'s auto-update reported its result** instead of discarding it (`let _ = download_and_install(…)`), which had turned "a host served bytes the release key did not sign" into silence.
- **`install.sh` was run on four distros** and the failures that found were fixed, along with its memory arithmetic and a precision warning.

### Security

An adversarial audit ran against 2.0.0 before release. Its findings, all fixed here:

- **The registry sandbox was escapable, twice.** A registry-published tool is advertised as having no `os`, `io` or `package`; `load` accepted LuaJIT bytecode, which does not go through the compiler those globals restrict, and a second route did the same job. Both are closed.
- **A sandboxed tool could wedge or OOM the whole agent.** There was no step, time or memory bound inside the VM, and the per-call timeout cannot supply one, because there is no way to abort a foreign Lua stack. A stranger's tool also could not take a built-in's name.
- **`web_fetch` followed redirects past half its own SSRF guard.** The pre-flight check resolved the host and rejected private addresses; every redirect after that was checked by a variant that returned `Ok` for any host that was not a literal IP or a `localhost` name, without resolving it. One hop was enough to reach a link-local metadata endpoint.
- **A mirror could serve a genuinely signed older release.** Release asset names carry no version, so a host answering `<mirror>/v2.0.0/…` could hand back an older release's real, key-signed files and pass every cryptographic check. Both install paths now require the signature's trusted comment — which the release workflow writes the tag into, and which the global signature covers — to name the release being installed: `wizard update` and `install.sh` alike, since the installer is the `curl | bash` path and the one that reads the mirror first. This one arms when the signing key is seeded rather than being live today.
- **A credential in a URL query string reached the support bundle.** `?key=…` survived every layer of the scrubber: `?` and `&` are word separators, the secret-name rule required a name strictly longer than its stem, there was no `@` for the userinfo pass to see, and the MCP walk covered `[server.env]` and `[server.headers]` but never `url`.
- **Untrusted text reached trusted surfaces as a bare `String`.** Text from a peer the user explicitly trusted went through a newtype that cannot be bypassed, while a fetched page — which anyone can author — went straight into the transcript, the session JSONL and `--output-format stream-json`.
- **A release tag is a release tag, not a path.**
- **One space defeated the mesh's blank-line cap.**
- **Two of the deep-evolve gate's three rungs were unbounded.**
- **A download nobody has agreed to trust yet is bounded**, and a late-arriving tool can no longer win a name.

### Known issues

- **The window is still settling.** The TUI remains the surface everything ships to first.

- **The graph explorer is deferred.** The mesh-as-a-picture screen inside the window is not reachable in 2.0.0: it has no button, no screen and no message, and `docs/graph-explorer.md` describes something you cannot currently open. It was too unfinished to put in front of users, and the honest fix was to take the door off rather than ship the room.

  What is *not* affected: `wizard peers` and the mesh itself are unchanged, and the model and layout under the explorer (`src/graph/`) keep building and keep running their tests. The code is wired out, not deleted — `src/native/graph/mod.rs` lists the four seams that put it back, and `the_window_has_no_route_into_the_graph_explorer` fails the build if one of them returns by accident.

[2.0.1]: https://github.com/teddytennant/wizard/compare/v2.0.0...v2.0.1
[2.0.0]: https://github.com/teddytennant/wizard/compare/v1.8.0...v2.0.0
