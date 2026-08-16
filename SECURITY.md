# Security

Wizard is an agent that runs shell commands, writes files, and (if you ask it to) recompiles and replaces its own binary. This document describes what protections exist, what they actually cover, and where you are trusting the model, the tools, or yourself. It is written to be honest rather than reassuring.

## The short version

- Everything Wizard does runs **as you**, with your privileges. There is no sandbox.
- **There is no per-action y/n approval gate, by design.** In normal genie and sovereign use, tool calls run as soon as the model makes them. Plan mode still blocks non-read-only tools until a plan is approved, and hooks can block or rewrite calls. Sovereign mode is headless and autonomous for one task; perpetual self-direction is `--continuous` (which implies sovereign).
- MCP servers and scripted tools are programs you chose to run. Wizard scrubs their environment and bounds their time, but it cannot make an untrustworthy program trustworthy.
- A project's own `.wizard/hooks.toml` does not run until you approve that project once. Cloning a repository is not consent; unattended surfaces refuse by default.
- Deep `/evolve` is gated by a clean `cargo build --release --locked`, a passing `cargo test --release --locked`, and a smoke test, and keeps the old binary for one-`mv` rollback. There is no diff-approval step.
- The mesh opens no socket unless you turn it on, accepts no work at all, and treats everything a peer sends as display data. See "The mesh" below.
- API keys live in environment variables or `~/.wizard/credentials.toml` (written atomically, file mode 0600). Precedence differs by *resolver*, not by credential, and you have to know which you are rotating. The chat loop, `/provider`, and the settings column resolve an **LLM provider key** environment-variable-first. Three resolvers go the other way and read `credentials.toml` first, falling back to the env var only when nothing is stored: the **Telegram bot token**, the **web-search backend keys**, and **`generate_image`**, which resolves ordinary provider credentials (`xai`, the active provider's name) that way even though chat turns on the same provider do not. Rotating any of those three means editing or deleting the stored entry; exporting a new value alone leaves the old key in use, and for the image tool that means chat keeps working while image generation 401s. OAuth sessions land in `~/.wizard/xai_oauth.json` and `~/.wizard/chatgpt_oauth.json`, both created mode 0600; `wizard doctor`'s **secret storage** check is what tells you if one of them is loose anyway (a file copied between machines or restored from a backup keeps the mode it arrived with). `config.toml` only ever names the env var, never the key.

## No approval gate

Wizard has no per-action y/n confirmation in either mode (genie or sovereign). Outside of plan mode and blocking hooks, every file write, shell command, MCP call, scripted tool, and `/evolve` runs the moment the model calls it. The state-changing tools include:

- **File writes:** `write_file` and `edit_file`
- **Shell:** `execute` (this is also how git commits, pushes, and any other command happen)
- **Your desktop:** `computer` — mouse moves, clicks, drags, typing, key chords, scrolling and screenshots, on the real screen, in real screen coordinates. It is registered unconditionally on every platform (an unprovisioned machine gets a refusal at call time, not a missing tool), and no action it takes is confirmed with you. A model that has been prompt-injected can click a button, type into a window, or read whatever is on screen; on Linux the input goes through the kernel's uinput device, so a Wayland compositor does not stand in its way either. See [computer-use.md](docs/computer-use.md).
- **Scripted tools:** agent-authored scripts in `~/.wizard/tools/`
- **MCP tools:** every tool served by an MCP server
- **Subagents:** spawning a subagent (which runs its own loop, equally ungated)
- **Memory / images / tasks:** `memory`, `generate_image`, `task_kill`, `subagent_kill`, plus evolve and publish when registered
- **Slash commands:** `run_command`, which queues one of Wizard's own commands on the attached surface
- **`/evolve`:** runtime and deep evolutions run without confirmation

Read-only tools include `read_file`, `list_files`, `search_files`, `git_status`, `git_diff`, and also helpers that touch nothing outside the agent: `todo`, `manual`, `web_fetch`, `web_search`, `x_search`, `task_output`, `subagent_status`, and `compact` (which only rewrites the conversation's own history). MCP, scripted, and spawn tools are execute-class. `computer` is not among them: a screenshot reads your screen rather than the project, and the tool it belongs to is execute-class as a whole.

Three gates still exist, and they are intentional:

- **Plan mode** keeps non-read-only tools blocked until the agent presents a plan and you approve it (`exit_plan`).
- **Hooks** (`pre_tool_use`) can block or rewrite a call before it runs.
- **Project trust** decides whether a project's own hooks load at all (see "Project trust" below). It is a gate on what a *repository* may run, not on what the model may do.

There is no config key that restores a y/n gate for ordinary execute/edit. Earlier releases had an `auto_approve` flag; it was removed, and a config that still carries it loads fine: the key is ignored and never written back.

**Sovereign mode** is headless and autonomous for a single task: no TUI, no per-action gate, one mission, then exit. **Continuous mode** (`--continuous`, implies sovereign) is the perpetual loop: it keeps going after the task, self-directs, can self-improve via `evolve`, and persists a durable mission under `<project>/.wizard/mission.toml`. A confused or prompt-injected model (in any mode) can run arbitrary commands as you. Only run Wizard on tasks and machines where that is acceptable, and prefer a container or VM for anything you would not run by hand (see "No sandbox" below).

Tool calls run with your full privileges. The boundary that matters is the machine and task you point Wizard at, not a prompt.

## MCP servers

Wizard is an MCP client: servers declared in `~/.wizard/mcp.toml` (stdio or HTTP) have their tools merged into the registry. What Wizard does to limit the damage a server can do *by accident*:

- **Cleared environment.** Stdio servers are spawned with `env_clear()`, so they do not inherit the wizard process's environment: API keys and other secrets in your shell do not leak into child processes. Only an allowlist is forwarded from the parent: `PATH`, `HOME`, `LANG`, `LC_ALL`, `TERM`, `USER`, `SHELL`, `TMPDIR`.
- **Dynamic-linker variables are dropped.** `env` entries in `mcp.toml` are passed through to the child, *except* `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, and `DYLD_LIBRARY_PATH`. Each of those is a code-injection vector into the spawned process, so they are never forwarded (a warning is logged when one is dropped).
- **Per-request timeouts.** Connect/initialize is bounded at 20 s, `tools/list` at 30 s per page (with a hard cap on pagination), and each `tools/call` at 120 s. A wedged or malicious server can waste your time, not hang Wizard forever.
- **No name shadowing.** An MCP tool that advertises a native tool's name (`execute`, `write_file`, …) is namespaced `server__tool` instead of replacing the built-in.

MCP tool calls run without confirmation, like every other tool.

What Wizard does **not** do: an MCP server is an arbitrary program *you* configured Wizard to run. It executes with your full privileges, can open its own network connections, read your files, and do anything else your user can. The environment scrubbing limits accidental secret leakage and one specific injection vector; it does not contain a server that is itself malicious. Register only servers you trust, the same way you would vet anything you pipe to `sh`.

The same applies to scripted tools: they are scripts under `~/.wizard/tools/` that run as you. LuaJIT tools run in-process inside the Wizard binary; tools with an external `interpreter` spawn that process as you. **Embedding LuaJIT is not a sandbox.** A tool you wrote (or that `/evolve` wrote) is created with mlua's `ALL_SAFE` standard library, which drops `debug` and `ffi` but keeps `os` and `io`, so it can call `os.execute`, read and write files, and do anything else your user can. What is bounded is time (a per-call timeout, after which the worker is abandoned), not capability. Treat a Lua tool the same way you treat an MCP server: code you chose to run.

**Code mode (`run_code`) is the same standing, arrived at from the other direction, and it is off by default.** A program is code the *model* wrote, running in-process under the full standard library — `os` and `io` are live — with a bridge to every tool the agent already has. It claims nothing beyond that. Every tool a program calls goes through the same pipeline as a direct call, so `pre_tool_use` hooks can rewrite or veto it and its edits are snapshotted for `/rewind`; a program cannot call `run_code`, `spawn_subagent`, `evolve`, `publish`, `exit_plan`, `interview` or `run_command`. What is bounded is compute time (30 s by default, 120 s maximum, plus a 600 s wall ceiling), memory (64 MB of Lua heap plus 8 MB of printed output) and dispatched calls (64) — not capability. A bound that fires stops the program for good: it cannot be caught with `pcall`, a coroutine the program created is bounded like the main chunk, and the JIT compiler cannot be turned back on to silence the hook. Two limits follow from the bound being a hook between VM instructions, and both are real: a single allocation can pass the memory ceiling (`string.rep('x', 6e8)` is one instruction, and handing LuaJIT a failing allocator crashes the process on some platforms, so it is not used), and nothing fires while the chunk is inside a C call — a program that blocks in `os.execute("sleep 99999")` cannot be stopped from inside, which is why the supported way to run a command from a program is `tool.execute`, which has its own timeout. The turn is not held by it: the host stops waiting a couple of seconds past the budget and reports what the program printed, leaving the thread to finish its call. Removing `os.execute` from a program that can write `tool.execute{command="curl … | sh"}` on the next line would be a decoration, not a boundary, so it is not claimed; `os.exit` *is* removed, because there is no `tool.exit` and it is the only call that ends the host process rather than the program. Enable it with `code_mode = true`; it is never offered to a model without native tool calling. See [code-mode.md](docs/code-mode.md).

## Installed skills and tools

`wizard skills install` fetches a skill or tool from the registry, which is a supply chain: **installing a tool is running its author's code.** Two things bound that, and neither is a sandbox in the sense of containing a hostile author:

- **A registry tool is not given the full standard library by default.** It runs under an allowlist (`table`, `string`, `math`, `bit`, `jit`) with no `os`, no `io`, no `package`, no `dofile`/`loadfile`, and the host file helpers confined to the project directory. `load`/`loadstring` accept source only — a binary chunk is refused and `string.dump` is removed, because LuaJIT does not verify bytecode and loading a crafted chunk is the standard way out of exactly this allowlist. That is a real restriction and it is why a registry tool is weaker than one you wrote yourself.
- **It is bounded in what it can take, not only in what it can reach.** A sandboxed tool gets a memory ceiling and a deadline enforced inside the VM, so a runaway loop or an unbounded allocation ends as an error the tool reports rather than a wedged core or an out-of-memory kill of the whole agent. This does not apply to tools you wrote yourself, which are your own code and are bounded in time only.
- **The full library is an explicit grant, or the install fails.** A tool whose manifest declares capabilities is refused unless a human answers yes to a prompt naming the author, the version, the source URL and the sha256, or passes `--grant-full-stdlib`. With no terminal to ask on, it refuses rather than guessing. The grant is all or nothing: `os` and `io` arrive as whole tables, so Wizard cannot hand out `os.execute` and withhold `io.open`, and the prompt says so.

Every install verifies the published checksum, refuses a name that would shadow a built-in tool or a bundled skill, refuses to overwrite something you wrote, and refuses to update an entry whose author changed or whose grant would have to be re-given. See [market.md](docs/market.md#installing-a-tool-is-running-its-authors-code). What none of that does is make an author trustworthy: read the source at the URL the prompt printed.

## Project trust

Two things a repository can ship reach Wizard before you have typed anything: `<project>/.wizard/hooks.toml`, whose entries run through `sh -c` at lifecycle points (`session_start` fires before the model's first word), and instruction files (`WIZARD.md` / `AGENTS.md` / `CLAUDE.md`), which go into the system prompt. `git clone && wizard` must therefore not be arbitrary code execution, and it is not:

- **Project hooks are gated on one recorded decision.** A project that ships `.wizard/hooks.toml` contributes no hooks until there is a recorded yes for exactly that file; where Wizard can ask, it names the file and asks. The answer, yes or no, is recorded in `~/.wizard/trusted_projects` (mode 0600, written atomically), keyed on the *canonicalised* project root, so a symlinked or `..`-dressed path cannot ride another project's approval, and on a sha256 fingerprint of the hooks file, so editing, replacing, or newly adding it re-opens the question rather than inheriting the old yes. Delete the project's line from that file to be asked again. A project with no `.wizard/hooks.toml` is never asked about.
- **The default is no, and the default is also do not ask.** Asking parks a thread on stdin, so it is a capability each surface declares for itself, never something inferred from `isatty` (under the TUI `isatty` says yes and prompting would freeze the event loop). Two surfaces declare it, both before anything has taken the terminal over: the **genie TUI**, and a **headless run** (`-p`, `--mode sovereign`, `--continuous`) under the default text output format. Even there the terminal facts must agree: a tty on both stdin and stdout, with this process in the foreground process group, so a piped, backgrounded, systemd, cron, or CI invocation refuses instead of blocking. Everything else refuses outright, and says so where it can: the **gateway** on stdout, and the **GUI** (the native window) in the task's event stream. `wizard acp`, `wizard fleet`, the scheduler daemon and every mid-session agent rebuild get a log line and nothing else, because none of them has a channel of its own to report on; for the scheduler that line lands in the job's own log, since its child is a text-format headless run whose stderr is captured there. Nothing is recorded when nobody was asked, so the next run that can ask still gets to decide. `WIZARD_TRUST_PROJECT=1` is the explicit per-process opt-in for unattended machines whose project hooks are your own: it answers an open question only, it cannot override a no you recorded, and it is never persisted.
- **The gate is on loading, not on firing.** Hooks from an untrusted project are never loaded, so there is no surface (TUI, sovereign, gateway, GUI, fleet) and no event on which one can fire.
- **The global `~/.wizard/hooks.toml` is not gated.** It is yours by construction; gating it would prompt in every directory and close no hole.
- **Instruction includes cannot escape the repository.** An `@relative/path` line in an instruction file inlines that file into the system prompt, and a project's instruction file can only reach files inside its own directory subtree. Containment is checked after canonicalisation, so neither `../../.wizard/credentials.toml` nor a symlink committed to the repository resolves out of it; a refused include is named in a comment instead of inlined. Your own global `~/.wizard/WIZARD.md` is exempt from the confinement, on the same reasoning that leaves the global `hooks.toml` ungated.

The per-surface breakdown of which invocations can ask, which refuse, and where each one reports the refusal is in [hooks.md](docs/hooks.md#when-wizard-asks-and-when-it-just-refuses).

What this does **not** do: it does not gate the model's own tool calls (there is no per-action approval, see above), and it does not make a trusted project's hooks safe. Approving a project is approving whatever its `.wizard/hooks.toml` runs, as you, today and until that file changes. Read the file before answering yes; Wizard names it and refuses to print its contents for you, because those contents are the repository author's text and your terminal interprets escape sequences.

## Deep `/evolve` (self-recompilation)

`/evolve --deep` is the unusual one: the agent proposes a diff to Wizard's own Rust source, builds it, and replaces the running binary. There is no approval step; the gates are mechanical, in order.

**Be clear about what these gates are.** They decide whether a patch is *good enough to install*. They are not a containment boundary, and rung 1 is the point to understand: `cargo build` executes the patched tree's `build.rs` and every proc macro in it, as you, before any rung has reported a verdict. `--locked` does not prevent that — it rejects a patch that invents a *dependency*, and editing the build script that is already there invents nothing. So a rejected patch has already run its own code by the time it is rejected; what reverting protects is your source tree and your installed binary, not the machine. Each rung is bounded in time and killed as a process group, so a patch that will not terminate is a recorded failure rather than a hang, but bounded is not the same as sandboxed. Treat `--deep` the way you would treat running a pull request's CI on your laptop.

1. **Build.** `cargo build --release --locked` must succeed. `--locked` is on this rung, not only on the tests: the build is the first cargo invocation to touch `Cargo.lock`, so it is the one that either rejects a patch that quietly edited the lockfile (usually by inventing a dependency) or resolves that dependency and runs its build script for everything downstream. Bounded by the same timeout as the test rung and run in its own process group, because this is where the patch's own code first runs: an unbounded build meant a non-terminating build script hung the whole evolution with nothing reverted and nothing logged. On failure, the diff is reverted and the running binary is untouched.
2. **Test suite.** `cargo test --release --locked` must pass, carrying the same lockfile rule. The run is bounded at 45 minutes (`WIZARD_EVOLVE_TEST_TIMEOUT_SECS` overrides the bound; it cannot be disabled) and a timeout counts as a failing suite. On failure, the diff is reverted and the current binary is kept.
3. **Smoke test.** The freshly built binary is executed with `--version` and must exit 0 and print a `wizard` version string, within 60 seconds and in its own process group — it is a model-authored binary, so "never returns" is one of the things it can do. On failure, the diff is reverted and the current binary is kept.

Only after all three does Wizard commit the patch to the `~/.wizard/src` checkout and install the new binary over the running executable, first copying the old one aside as `<name>.prev` in the same directory. Copied rather than moved, so the running binary's path never stops resolving to a whole binary — an interruption cannot leave the directory with only a `.prev` in it. The install does not escalate with `sudo`: an install path this user cannot write fails the evolve at that point, with the source commit already made and the built binary left in the checkout's `target/release/`, and the error naming the `sudo install` command that would finish it. The running binary is untouched. To roll back a deep evolution:

```bash
mv /usr/local/bin/wizard.prev /usr/local/bin/wizard
```

(Adjust the path if you installed elsewhere; Wizard prints the exact rollback command when it installs.)

Be clear about what these gates are and are not. The suite is the real one, so a patch that breaks the agent loop in a way any test covers is rejected; that is a much higher bar than "it compiles and prints a version string", and it is why a deep evolve now takes minutes rather than seconds. It still does not prove the change is correct, safe, or what you asked for: an untested path stays untested, and a patch that is malicious but green installs. Deep evolve is the model rewriting its own agent loop, checked by the compiler, the existing tests, and nothing else. The record and the rollback are the rest of the safety net: every deep evolution is logged with its diff to `~/.wizard/evolution.jsonl` (rejected attempts too, with the gate that stopped them and the failing output), the source checkout at `~/.wizard/src` keeps the change as a git commit, and the prior binary stays one `mv` away.

## `wizard sync` bundles

`wizard sync` moves config, skills, commands, subagents, and scripted tools between machines as a signed bundle ([sync.md](docs/sync.md)). The mechanics:

- **Signed manifest.** Bundles are ed25519-signed: the manifest lists the sha256 of every file, `manifest.sig` signs the manifest, and the bundle embeds the sender's public key. Each machine's key seed lives at `~/.wizard/sync/key` (mode 0600).
- **All-or-nothing verification.** `pull` checks the signature, then the trust list, then every file hash; nothing is written to `~/.wizard/` unless all of it passes.
- **Trust on first use.** Like SSH: the first pull pins the sender's public key into `~/.wizard/sync/trusted_keys` and prints its fingerprint for out-of-band comparison (`wizard sync key` on the source machine). Later pulls reject bundles signed by unknown keys.
- **Signed, not encrypted.** Anyone who obtains a bundle can read it. Credentials (`credentials.toml`, `xai_oauth.json`) are excluded by default; `--include-credentials` opts them in, writes the bundle file 0600, and prints a warning. ChatGPT OAuth (`chatgpt_oauth.json`) is not packed today. Transfer such a bundle privately (`scp`), never over a public URL.

Be clear about what pinning a key means: a bundle carries commands, subagents, and scripted tools, which later run with your privileges. Trusting a machine's key in `trusted_keys` trusts whoever controls that machine to ship that state to this one.

## The mesh

New in 2.0, and the only feature in this release that puts Wizard on a network socket other than an outbound API call. The full description is in [mesh.md](docs/mesh.md); this is the part that belongs in a threat model.

- **Nothing binds a port until you say so.** `[mesh] listen` and `[mesh] mdns` are both `false` by default, and a default install never opens a socket or announces anything on the local network. A mesh that listened on install would be a surface nobody asked for, and this codebase has shipped fail-open defaults before (the Telegram allowlist, project hooks on session start), which is why the default is written down and pinned by a test rather than assumed.
- **There is no remote execution, because there is no frame for it.** Three message kinds cross the wire: liveness, announcement, and session-event subscription. Delegated work was cut from this release, `accepts_work` is `false` in every direction a default can be, and no command sets it. There is no task frame on the wire for a compromised peer to send.
- **The connection proves the identity.** Transport is QUIC with mutual TLS where each certificate is self-signed by the ed25519 key that *is* the node's name, so the handshake proves the peer id rather than a signature bolted onto a plaintext socket. There are no certificate authorities in this and `src/mesh/tls.rs` is the only verifier. The seed lives at `~/.wizard/node.key`, mode 0600.
- **Trust is a human decision in three states.** A pasted address lands at `known` and can be contacted but may do nothing; only `trusted` may open or receive a session stream; `blocked` may not be contacted at all. Nothing infers trust from behaviour and nothing a peer says about itself moves the dial. Revoking drops that peer's live subscriptions in the same call, in both directions, and writes the store before returning.
- **Watching is read-only in both senses.** A trusted peer's session events render in your transcript; you cannot drive that session, and nothing arriving on the stream can drive yours. Which event kinds may cross is one exhaustive match with no wildcard arm, so a new event does not compile until somebody decides whether it is a report or a request. Requests do not cross. A plan review and a prompting command's console do cross, because their text is the interesting part, but their approval tickets are voided on the way out: watching a peer's session never becomes typing into a peer's shell.
- **Nothing a peer sends is trusted input.** Every string is sanitised at construction and again on decode: control characters replaced, zero-width and bidirectional overrides deleted, length capped. Every physical line a peer wrote is prefixed with a marker derived from its public key, which it cannot forge or influence. There is no path from a peer's stream into a system prompt, a tool argument, or a command dispatcher, and sanitising would not make one safe if there were.

What this does **not** do. Trusting a peer means letting the operator of that machine read your session transcripts, which carry your prompts, your file contents, and your tool output. That is the whole point of the feature and it is also the whole exposure: treat `wizard peers trust` as the same kind of decision as handing somebody your screen. There is also one gap named rather than papered over: revoking in one terminal does not sever a stream held by a Wizard process already running in another on the same machine, because that process holds its own copy of the peer store. Revoke from the other machine, or end the session. See [mesh.md](docs/mesh.md#a-decision-reaches-the-process-that-made-it).

## No sandbox

All tools run directly with your user's privileges. The `execute` tool runs real shell commands and cannot be confined to the working directory: absolute paths, `cd ..`, pipes, and network access are all reachable. The same is true of MCP servers and scripted tools. Treat tool execution as full local access, because it is.

Also note that the model reads files and tool output as instructions-adjacent context. A hostile string in a repository you point Wizard at (a README, a test fixture, a commit message) can attempt to steer the model's tool calls (classic prompt injection). There is no confirmation gate to catch a steered tool call; the defense against prompt injection is isolation, per the recommendation below.

Recommendation: for any Wizard run on untrusted or semi-trusted tasks (third-party repos, code review of unknown patches, anything internet-derived), run Wizard inside a container or VM with only the project mounted. With a local provider (llama.cpp or Ollama) a fully offline container works; with a cloud provider, allow only that provider's API endpoint.

## Install-path trust

The recommended install is `curl | bash`, and you should be honest with yourself about what that means: you are executing a script from the network with your privileges (and it may use `sudo` to place the binary in `/usr/local/bin`). Mitigations, in increasing order of paranoia:

1. **Read it first.** Download `install.sh`, read it, then run it. Be realistic about what that costs: it is one bash script of 2,174 lines covering platform detection, four install flavors, a llama.cpp build, model downloads, release verification and its download-mirror fallback, and Termux and NixOS special cases. (A test fails if that number drifts from the file, so it is the real one.) Reading it properly is a sitting-down job, not a glance, and "read it first" is only a real mitigation if you actually do it. If you will not, prefer option 3.
2. **Signature and checksum verification, both mandatory.** The installer downloads release tarballs from GitHub releases (or from the mirror named by `WIZARD_MIRROR`, falling back to GitHub on any failure and saying which one it used), verifies each one's SHA-256 against the release's `checksums.txt`, and verifies `checksums.txt` itself against its detached minisign signature (`checksums.txt.minisig`) under the release public key inlined in the script. Every failure aborts: no signature, a signature from another key, a bad signature, no `checksums.txt`, no entry in it for the asset, a digest mismatch, or a host with no `sha256sum`/`shasum`. There is no flag or environment variable that installs something unverified; the way past a refusal is `WIZARD_BUILD_FROM_SOURCE=1`, which trusts the git history instead. The one thing the installer needs and cannot supply is a signature checker: it uses `minisign` when it is on PATH, falls back to an OpenSSL 1.1.1+ / 3.x that can do ed25519 and blake2b, and refuses when neither is present (macOS ships LibreSSL as `openssl`, which is neither, so `brew install minisign` is the macOS prerequisite). `wizard update` carries the same key compiled in and applies the same rules in-process, so it needs no external tool.
3. **Build from source.** Clone the repo, audit it, `cargo build --release`. This removes trust in the release pipeline entirely:

   ```bash
   git clone https://github.com/teddytennant/wizard
   cd wizard && cargo build --release
   install -m 755 target/release/wizard ~/.local/bin/wizard
   ```

## Release signing

Releases are signed with [minisign](https://jedisct1.github.io/minisign/) (ed25519 over a blake2b-512 prehash of the file). The signed file is `checksums.txt`, and every asset is verified against it, so one signature covers the release: a tarball that matches a digest in a `checksums.txt` that carries a valid signature is the release key's tarball.

**The public key lives at [`wizard-release.pub`](wizard-release.pub) in this repository.** The same key line is compiled into the binary (`src/update.rs` reads that exact file with `include_str!`) and inlined in `install.sh`, and a test asserts the two copies have not drifted. Verify a release by hand with:

```bash
minisign -Vm checksums.txt -x checksums.txt.minisig -P "$(grep -v '^untrusted comment:' wizard-release.pub)"
sha256sum --check --ignore-missing checksums.txt
```

What that gets you, and what it does not:

- **A missing or bad signature is fatal on both install paths**, with no flag, environment variable, or config key that bypasses it. `wizard update` fetches `checksums.txt` and `checksums.txt.minisig` before it downloads any tarball and refuses on a missing signature, an unparseable one, one made by a key this binary does not carry, one whose trusted comment was edited, or one that simply does not verify. `install.sh` does the same before it unpacks anything.
- **The signature is the part a compromised download host cannot forge.** `checksums.txt` arrives from the same GitHub release as the tarballs, so on its own it only proves the transfer was not corrupted. The signature is what makes the digests an assertion by whoever holds the release key.
- **A mirror is not a weaker path.** `WIZARD_MIRROR` (and any default mirror a future release ships) puts another host in front of GitHub Releases, which is a second place a release could be tampered with, and signing is the mitigation. There is no parameter, flag or branch by which a mirror-served file is checked less strictly: one function verifies `checksums.txt` and it is never told which host answered. A host that cannot serve the release is fallen back from; a host that serves bytes the release key did not sign **stops the install or update**, with the host named, rather than being quietly routed around — otherwise a compromised mirror would be invisible to the person it is attacking. The mirror is also never asked which version is current: the tag comes from GitHub. That alone was not enough, and the gap is worth naming because the fix is what closes it. Asset names carry no version, so a mirror answering `<mirror>/v2.0.0/…` could hand back an *older release's genuine, key-signed* files — right key, valid signature, digests matching their own `checksums.txt` — and every cryptographic check would pass. The release workflow writes the tag into minisign's trusted comment and the client verifies the signature covering it; it now also **requires that comment to name the release being installed**, so a signature is evidence of *which* release was signed rather than merely that one was. A signed downgrade is refused with the mismatch named.
- **It does not cover a compromised release *pipeline*.** The secret key lives in a GitHub Actions secret (`MINISIGN_SECRET_KEY`) and is used by `.github/workflows/release.yml` on a tag push. Anyone who can run that workflow, or who can read that secret, can sign. What signing removes is everything downstream of it: a tampered mirror, a swapped asset, a doctored `checksums.txt`. Option 3 above (build from source) is still the way to trust nothing but the git history.
- **Key rotation is a binary release.** The key is compiled in, so rotating it means shipping a new binary and a new `install.sh`; installs older than the rotation will refuse releases signed with the new key rather than accept an unknown one. That is the intended failure: rotate by publishing the new public key here, signing one release with both keys where possible, and saying so in the release notes.

The release keypair exists and `wizard-release.pub` holds the public half. If it ever has to be regenerated, seeding is one command, and the secret key never has to leave the machine that made it except to become a repository secret:

```bash
contrib/seed-release-key.sh          # writes the secret to ~/.wizard-release.key by default
gh secret set MINISIGN_SECRET_KEY < ~/.wizard-release.key
git add wizard-release.pub install.sh && git commit -m 'seed the release signing key'
```

A script rather than a bare `minisign -G`, because the bare command does not work here and the ways it fails are quiet. `minisign -G` refuses to write over the placeholder `wizard-release.pub` that is committed to this repository, so it needs `-f`. The public key has to be inlined into `install.sh` as well — a shell script cannot `include_str!` — and a test asserts the two copies are identical, so generating the key without that edit is a red suite and committing them separately is a commit whose installer trusts a key its binary does not. And the key must be passwordless: CI signs unattended with the secret on stdin, so a passworded key hangs the release job instead of failing it. The script does all four, refuses to run twice, and leaves the commit to you. A binary or an `install.sh` whose key line is still the placeholder refuses every release rather than installing something it cannot verify, which is the state this replaced.

The workflow verifies its own signature against the committed `wizard-release.pub` before it publishes anything, so a secret that does not match the published key fails the release instead of shipping ten assets nobody can install.

The default installer also downloads `llama-server` from llama.cpp's official GitHub releases and a GGUF from Hugging Face; with `WIZARD_USE_OLLAMA=1` it instead runs Ollama's official install script (`curl -fsSL https://ollama.com/install.sh | sh`) if Ollama is absent: same trust consideration, different vendor. Skip these with `WIZARD_SKIP_LLAMACPP_INSTALL=1` / `WIZARD_SKIP_OLLAMA_INSTALL=1` if you manage the model runtime yourself.

## Where your data goes

Inference goes to whichever provider is active: the core loop sends prompts, code context, and tool output to that endpoint and nowhere else. With the default local provider that endpoint is `llama-server` on your machine (`http://127.0.0.1:11435`); with a cloud provider it is that vendor's API, under their data-handling terms. The other network actors are the things you add: MCP servers and scripted tools can make whatever calls they like, and deep evolve clones the source repo and may install a Rust toolchain via rustup on first use.

What stays on disk stays on this machine. Session transcripts (`~/.wizard/sessions/`), diagnostic logs (`~/.wizard/logs/`, see [logging.md](docs/logging.md)), and memory are local files; nothing ships them anywhere.

Every directory that holds something private is created mode 0700 on unix, and an existing loose one is tightened on the next load, so the tree is private even on an install that never stored a credential. Every config load rebuilds `~/.wizard` itself plus `sessions/`, `logs/`, `memory/`, `tools/`, `skills/`, `subagents/` and `running/` that way; the TUI's editor `scratch/` and the gateway's `gateway-attachments/` get the same treatment when they are first needed. One caveat on that last one: when no state directory can be resolved at all (a systemd user unit with `ProtectHome=yes` and no `WIZARD_HOME`, a container with no passwd entry), the gateway's attachments go to `<system temp dir>/wizard-gateway-attachments` instead, outside `~/.wizard` entirely and at a path every local account can guess. It is created the same way there, 0700 asked for at creation time rather than chmod'd afterwards, and each attachment is written 0600 and `O_EXCL` so a name somebody else planted first fails the download instead of being written into. So the fallback is not a hole, but it is a different place: if you are looking for what a Telegram sender uploaded and `~/.wizard/gateway-attachments/` does not exist, that is where it went. `sync/`, the `update/` staging directory, and each `doctor --bundle` directory are stricter still: there they abort rather than warn when the mode cannot be set. It does **not** cover every directory the tree can grow: `~/.wizard/sync/backups/<timestamp>/` (written by `wizard sync pull` before it overwrites anything) and the llama.cpp scratch, model, and `bin/` directories are created at your umask, 0755 on a stock distro rather than 0700. What keeps those private is the parent rather than the directory itself — `~/.wizard`, and for the backups also `~/.wizard/sync/`, which is created strict because the signing key lives there — so a state tree whose top-level mode was ever loosened loosens these with it. The backups directory is the one to watch, since it holds copies of the files it is about to replace.

On a filesystem that cannot express unix modes (exFAT, FAT32, some CIFS/NFS mounts, WSL DrvFs, which `WIZARD_HOME` explicitly supports) the chmod fails, Wizard logs a warning naming the directory, and startup continues: the state is readable by other local users there. `wizard doctor`'s **secret storage** check reports that state on every run, and it cannot be satisfied on such a filesystem.

`wizard doctor --bundle` is the one command that gathers that state into one place for sharing. It strips credentials (a config field allowlist, known key values, vendor key shapes, secret-looking key names), writes the bundle 0700, and still tells you to read it before sending, because no redactor can tell which of your own prose and paths is sensitive. It uploads nothing; attaching it is your action. See [doctor.md](docs/doctor.md#bug-report-bundles).

## Reporting a vulnerability

If you find a security issue in Wizard:

- Open a private report via **GitHub security advisories** on [teddytennant/wizard](https://github.com/teddytennant/wizard/security/advisories)

Please include reproduction steps and the version (`wizard --version`). Reports are read by a human; expect an acknowledgment, not an SLA; this is a small open-source project.
