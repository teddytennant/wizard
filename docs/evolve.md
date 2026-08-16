# Self-extension (`/evolve`)

Wizard can extend itself. `/evolve` lets the agent add new capabilities: a skill, an external tool server, a LuaJIT scripted tool, a subagent, or, when needed, new Rust in its own core.

The design borrows from the two self-modifying agents that pioneered this pattern:

- **[Pi](https://newsletter.pragmaticengineer.com/p/building-pi-and-what-makes-self-modifying)** modifies its own installed source in place and `/reload`s it live, which works because it's interpreted (Node/TS) and has no compile step.
- **[Hermes](https://hermes-agent.nousresearch.com/docs/)** never recompiles. It adds capability through portable skills, MCP servers (the channel for things like computer use), programmatic scripted tools (`execute_code`), and isolated subagents.

Wizard is compiled Rust for the core and the TUI; you do **not** need a TypeScript interpreter or a bloated Electron runtime to extend it. Tier-1 scripted tools run through **embedded LuaJIT**, the just-in-time compiler, in-process: Pi-style live reload for glue, without shipping Node. Deep evolve still recompiles only when a change has to live in the Rust binary. Two tiers.

---

## Tier 1: runtime extension (default, no recompile)

Works on every install. `/evolve` writes config/data under `~/.wizard/` and `/reload` activates it live. Four channels — skill, MCP server, scripted tool, subagent — plus two things you can override by hand (the system prompt, and the whole harness bundle):

### Skills

A Markdown file of guidelines, workflows, or domain knowledge. The system prompt lists its name and description; the body is read from disk when the skill matches (or inlined if the skill sets `always: true`).

```
> /evolve add a skill for writing conventional commit messages
```

Wizard writes `~/.wizard/skills/conventional-commits/SKILL.md` and reloads the index. The body is read when the skill matches.

### MCP servers

The path for capabilities that live outside Wizard: computer use, browser control, databases, search, anything shipped as an [MCP](https://modelcontextprotocol.io) server. Wizard is an MCP client; registering a server merges its tools into the registry with no rebuild.

```
> /evolve give yourself computer use via an MCP server
```

Wizard adds the server to `~/.wizard/mcp.toml`, connects, lists its tools, and they become callable on `/reload`.

```toml
# ~/.wizard/mcp.toml
[[server]]
name = "computer-use"
transport = "stdio"
command = "uvx"
args = ["mcp-computer-use"]
```

### Scripted tools (LuaJIT by default)

The agent authors a small script (the Hermes `execute_code` analog), saved to `~/.wizard/tools/` and run by Wizard. **The default runtime is embedded LuaJIT**: the just-in-time compiler ships inside the binary, so evolve glue needs no `bash`/`python`/`node` on `PATH` and does not spawn a child interpreter.

```
> /evolve add a tool that slugifies a string
```

Saved as `~/.wizard/tools/slugify.lua` with a manifest; exposed as a normal tool after `/reload`. Arguments arrive as the Lua global `args`; print results with `print(...)` (or `return` a value). Host helpers live under `wizard` (`read_file`, `write_file`, `json_encode`, `json_decode`, `runtime`).

```toml
# ~/.wizard/tools/slugify.toml
name = "slugify"
description = "Slugify a string"
script = "slugify.lua"
runtime = "luajit"

[parameters]
type = "object"
required = ["text"]
[parameters.properties.text]
type = "string"
```

A program is the ad hoc form of the same rung. `run_code` runs one Lua program
now, against Wizard's own tools, and throws it away — no file, no manifest, no
`/reload`. Write a scripted tool when you will want it again; run a program when
the answer is "do these forty things once". See [code-mode.md](code-mode.md);
it is off by default.

```lua
-- ~/.wizard/tools/slugify.lua
local s = tostring(args.text or ""):lower()
s = s:gsub("[^%w]+", "-"):gsub("^%-", ""):gsub("%-$", "")
print(s)
```

Shell, Python, and Node scripts still work when you set an external `interpreter` in the manifest; LuaJIT is the default, not a mandate.

### System prompt override

The baked-in base personality prompt can be replaced at runtime by a file: `~/.wizard/system_prompt.md` (or the path in `$WIZARD_SYSTEM_PROMPT`, which wins). When present and non-empty, its contents replace the compiled prompt for the active mode; absent, behavior is identical to the default. The charter, skills, project-instruction, and memory sections are always appended on top, so this override tunes personality and instructions without dropping the charter. The charter section is a generated **digest** of the bundled `WIZARD.md`, not its text: the lead, the ladder's rung names, an index of topic ids, and the three rules that have to be in force on every reply. The rest of the charter is served on demand by the `manual` tool, so an override cannot evolve it away and does not pay for it on every step either. This is the surface external harness-evolution tooling (e.g. AHE) mutates to measure and improve prompt quality.

### Harness bundles

The full evolvable surface, not just the prompt, can be externalized as a *harness bundle*: a directory activated with `--harness-dir <dir>` (or `$WIZARD_HARNESS_DIR`) whose files shadow the compiled defaults per component:

```
<bundle>/
  system_prompt.md            # base personality prompt (highest-precedence override)
  tool_descriptions/<tool>.md # description advertised to the model for that native tool
  skills/<name>/SKILL.md      # shadows bundled and user skills by name
  subagents/<name>.toml       # shadows user-defined and built-in subagents by name
  HARNESS.md                  # generated guide for evolution agents
```

Any missing or empty file falls back to the compiled default, so a partial or broken bundle degrades gracefully and deleting a file reverts that component. `wizard harness export <dir>` dumps the current compiled defaults as a bundle: the seed an external harness-evolution loop (e.g. AHE) edits, measures, and hands back for review. Winning changes get baked into the source as new defaults and re-exported, which is what makes the loop recursive. Methodology credit: [Agentic Harness Engineering](https://github.com/china-qijizhifeng/agentic-harness-engineering) (arXiv:2604.25850).

The old local `wizard bench` trajectory recorder/replay runner has been removed; measuring and improving harness quality is AHE's job ([wizard-ahe](https://github.com/teddytennant/wizard-ahe)).

### Subagents

Configure a named, reusable subagent with its own prompt and tool scope, for fan-out or specialized sub-tasks. A subagent that names no `max_steps` takes the default ceiling of 50 steps; set a positive `max_steps` to raise or lower it, or `0` for no ceiling at all ([loadout.md](loadout.md#a-roster-of-subagents)).

```
> /evolve add a "reviewer" subagent that audits diffs for security issues
```

---

## Tier 2: deep evolve (recompiles core)

When a change needs new Rust in Wizard itself (a new built-in tool kind, a protocol change, a TUI panel), use `--deep`:

```
> /evolve --deep add a /status slash command showing token usage
```

The pipeline:

1. **Locate source**: `~/.wizard/src`, cloned from the upstream repo on first use (override the clone URL with `WIZARD_SOURCE_REPO` if you want a fork or mirror).
2. **Ensure a toolchain**: if `cargo` is absent, install it via `rustup --profile minimal` (~0.5–1 GB, first deep evolve only). The default installer ships no toolchain; you pay for the compiler only if you use this tier.
3. **Propose a diff** over Wizard's own source, in two model turns: a file-selection turn picks the relevant files from the repository listing (with a keyword-matching fallback when it fails), then the diff-authoring turn sees those files' actual contents (up to 8 files under a ~96 kB budget) so its hunks match the real source and survive `git apply --check`.
4. **Clear the gate**, three rungs in order: `cargo build --release --locked`, then `cargo test --release --locked`, then a `--version` **smoke test** on the fresh binary. Any rung failing reverts the diff, records the failure with its output in `~/.wizard/evolution.jsonl`, and leaves the running binary alone.
5. **Install** the new binary over the running executable (keeping the prior one as `<name>.prev` in the same directory). This step does not escalate with `sudo`: if the install path is not writable the deep evolve fails here, and the error names both the built binary under `~/.wizard/src/target/release/wizard` and the exact `sudo install` command that would finish the job by hand. The running binary is untouched either way. Failing to *locate* the running executable (it was unlinked, or its path cannot be canonicalised) fails the evolve at this same step, with the same message: there is no fallback to "just use the build output", because that is what reported a rebuild as landed while `wizard` on `PATH` stayed the old binary. The one case that returns the build path is the case where nothing needed installing, meaning Wizard is already running from it.
6. **Restart into the new binary** when the surface supports it: the CLI path (`wizard --evolve --deep`) `exec`-replaces immediately; continuous mode writes an `evolve-reexec` marker and re-execs at a safe boundary; the interactive TUI/tool path reports the install and expects a restart (or continuous's own re-exec).

If there's no toolchain or source and one can't be provisioned (offline, no `rustup`), deep evolve falls back to Tier 1 and says so, rather than failing.

### The gate

| Rung | Command | Why |
|------|---------|-----|
| Build | `cargo build --release --locked` | The patch has to compile. `--locked` is on this rung, not only on the tests, because the build is the first cargo invocation to touch `Cargo.lock`: it is the one that either rejects a patch that invented a dependency or quietly resolves it (and runs its build script) for everything downstream |
| Tests | `cargo test --release --locked` | "It compiles and prints a version string" is a bar any plausible-looking patch clears while quietly breaking the agent loop. `--release` reuses the artifacts the build just produced, and it carries `--locked` too so the lockfile rule holds for the whole gate |
| Smoke test | the built binary, `--version` | It has to actually run on this machine before it replaces a binary that does |

Both cargo rungs carry the features the running binary was built with, so a `wizard-native` install (`--features native`) is rebuilt and tested as a native build rather than quietly becoming one whose `wizard gui` opens no window — the same rule `wizard update` follows when it picks a release asset.

**The test rung is why a deep evolve now takes as long as it does.** It runs Wizard's whole suite, which on a laptop is minutes rather than seconds, on top of a release build that is already slow. That is the cost of the change being checked by something other than the compiler; budget for it before starting a deep evolve, especially from the TUI where the session waits on it.

The run is bounded: if the suite does not finish within **45 minutes** it is killed and the patch is rejected exactly as a failing test would be. `WIZARD_EVOLVE_TEST_TIMEOUT_SECS` overrides that bound with a number of seconds, for a slow or heavily loaded machine:

```bash
WIZARD_EVOLVE_TEST_TIMEOUT_SECS=5400 wizard --evolve --deep -p "…"
```

A value that is zero, negative, or not a number is ignored and the 45-minute default applies; there is no way to disable the bound. On failure or timeout the error carries the tail of the test output, which is what the model reads when it tries again.

To install the toolchain eagerly at setup time (air-gapped or offline-first machines), set `WIZARD_WITH_TOOLCHAIN=1` on the installer:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
  | WIZARD_WITH_TOOLCHAIN=1 WIZARD_BUILD_FROM_SOURCE=1 bash
```

`WIZARD_BUILD_FROM_SOURCE=1` builds from the release tag rather than downloading its assets, which is the path that needs a Rust toolchain. See [Getting started](getting-started.md#install).

---

## Picking a tier

| You want to… | Tier | Recompile? |
|--------------|------|------------|
| Add knowledge or a workflow | Skill (1) | No |
| Add an external capability (computer use, browser, DB) | MCP (1) | No |
| Add small glue/automation | Scripted tool (LuaJIT) | No |
| Add a specialized sub-worker | Subagent (1) | No |
| Change Wizard's own built-in behavior or UI | Deep (2) | Yes |

If an MCP server or script can do it, stay in Tier 1: it's instant, reversible, and works on every install. Use `--deep` only when the capability has to live inside the binary.

---

## Logging and rollback

Every evolution, tier 1 or 2, is appended to `~/.wizard/evolution.jsonl` with a timestamp, the change, and (for deep evolve) the diff and build result. Deep evolves that are *rejected* are recorded too: the entry names the gate that stopped it (`build`, `tests`, `smoke test`, or `install`) and carries the failing output.

For the three gate rungs (`build`, `tests`, `smoke test`) the patch is already reverted by the time the entry is written, so `~/.wizard/src` is back at its pre-evolve state. **`install` is the exception.** That stage is past the gate: the patch compiled, passed the suite, and ran, so it has already been committed to the checkout at `~/.wizard/src` before the install is attempted. A failure there (an install directory that needs `sudo`, a full disk) leaves that commit in place and the new binary sitting at `~/.wizard/src/target/release/wizard`; the error names that path, and the *running* binary is still untouched. If you do not want the change, undo the commit in `~/.wizard/src` with git before the next deep evolve builds on top of it.

Inspect and roll back from the CLI:

```bash
# Numbered history, most recent first (#1 is the newest):
wizard evolve list

# Undo entry #N from the list:
wizard evolve undo 2
```

`undo` reverts what the entry recorded: a skill, scripted tool, or subagent undo deletes the created files (`/reload` to apply); an MCP-server undo removes its entry from `~/.wizard/mcp.toml`; a deep-evolve undo restores the `<binary>.prev` rollback copy over the installed binary (keeping the undone build beside it as `<binary>.undone`). Restart Wizard to run it. Undo is conservative: when the recorded artifacts are already gone it refuses with a clear message rather than guessing.

Everything is also plain files under `~/.wizard/`, so manual cleanup keeps working: delete the file and `/reload` to revert a tier-1 change; deep evolve keeps the prior binary as `<binary>.prev`.

---

## Safety

`/evolve` widens what the agent can do to your machine, so review what it adds. MCP servers and scripted tools run with your privileges and can make their own network and system calls. **Both modes apply `/evolve` changes directly: there is no approval gate.** Only run unattended evolution on machines and tasks where that's acceptable. See the [security model](architecture.md#security-model).
