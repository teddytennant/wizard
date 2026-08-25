# Plugin architecture

Wizard is a plugin host. The Rust binary keeps four things — the agent loop, the
provider transport, the terminal UI, and the kernel that wires plugins together
— and everything else is a plugin that registers itself through one API.

This is the "everything is a plugin" model, adapted to a compiled language. The
adaptation matters: a plugin here is **either** an in-tree Rust module compiled
behind a cargo feature, **or** a LuaJIT script loaded at runtime from
`~/.wizard/plugins/`. The kernel cannot tell the two apart, and no core module
names a plugin.

## Why not "all of it in Lua"

The obvious reading of "everything is a plugin" is "rewrite everything above the
agent loop in Lua". That does not survive contact with the tree. TLS and SSE
streaming for nine providers, QUIC and x509 for the mesh, an iced window, PTY
handling, release-signature verification and image decoding are ~60k lines that
would have to either be reimplemented in Lua (they will not be) or reached
through a host API so wide that it is Rust with a slower calling convention.

So the split is drawn by **what a plugin does**, not by what it is written in:

- Work that is *policy and orchestration* — deciding what to run, in what order,
  under which budget, and what to tell the model — is Lua. It is the part that
  benefits from hot reload, from being disabled on a small machine, and from
  being written by somebody who is not us.
- Work that is *bytes and syscalls* — a TLS handshake, a QUIC stream, a
  framebuffer — is Rust, compiled in, and still a plugin in every sense that
  matters: it registers through the same `Ctx`, it is feature-gated, it can be
  left out of a build, and core does not name it.

The result is that ~30% of the tree moves to Lua and ~55% becomes Rust plugins,
leaving a ~35k-line core.

## The boundary

**Core (never a plugin).** `src/kernel/`, `src/agent/{mod,turn,context,session,event,retry,breaker}.rs`,
`src/llm/{mod,provider,compat,registry,wire,oauth_callback,xai_oauth}.rs` (the
`LlmProvider` trait, the shared streaming machinery, the registry that resolves
a `kind`, the OpenAI-protocol client five backends build on, the loopback
redirect both sign-ins come back on, and the xAI token store two core *tools*
authenticate with — not the providers),
`src/ui/`, `src/app/` (including `src/app/tee.rs`, which is now the
`SessionTee` trait and the lookup — not a tee), `src/skin/`,
`src/event.rs`, `src/dispatch.rs`, `src/tools/{mod,registry}.rs`,
`src/entrypoint.rs` (the two lookups a CLI subcommand whose body ships in a
plugin goes through — `Entrypoint` for `wizard gui` and `Subcommand` for
`wizard peers` — not the surfaces themselves),
`src/event.rs`, `src/dispatch.rs`, `src/tools/{mod,registry,http}.rs` (the tool
trait, the one lookup, and the HTTP client/SSRF guard/redirect walk/body cap
that the web tools, the image downloader and a Lua plugin's `wizard.http` all
go through), `src/text.rs`,
`src/config.rs`, `src/logging.rs`, `src/trust.rs`, `src/cli.rs`, `src/main.rs`.

Two rules keep the boundary honest, and CI enforces both:

1. **No core module may `use crate::<plugin>`.** Core reaches plugins only
   through the registries and the event bus.
2. **Deleting any one plugin must leave a tree that compiles and passes tests.**
   A plugin whose removal breaks the build is not a plugin.

## The kernel

```
src/kernel/
  mod.rs         Kernel: owns the registries, the bus, and the plugin graph
  ctx.rs         Ctx — the whole plugin-facing API
  bus.rs         async event bus: ordered handlers, veto, payload rewriting
  services.rs    provide/inject, typed by name
  lifecycle.rs   load, unload, reload, and exact disposal
  manifest.rs    plugin manifest + capability declaration
  lua/
    mod.rs       long-lived VM per plugin, tokio bridge
    host.rs      the `wizard.*` table exposed to Lua
    sandbox.rs   stdlib profiles, deadline hook, memory ceiling
```

### Ctx

Every plugin — Rust or Lua — is handed a `Ctx` and registers against it. The
shape is identical in both languages so a plugin can be ported between them
without redesigning it.

| Call | Effect |
| --- | --- |
| `ctx:tool(spec)` | register a tool the model can call |
| `ctx:command(spec)` | register a slash command (name, description, `args` hint, `surfaces`) |
| `ctx:provider(spec)` | register a backend `config.toml` can select |
| `ctx:on(event, handler, priority)` | subscribe to a lifecycle event |
| `ctx:emit(event, payload)` | publish one |
| `ctx:provide(name, service)` | expose a service to other plugins |
| `ctx:inject(name)` | take a service, or `nil` if absent |
| `ctx:plugin(child, config)` | load a child plugin under this one |
| `ctx:effect(dispose)` | register a teardown |
| `ctx:config()` | this plugin's slice of `config.toml` |

`ctx:inject` returning `nil` is the composability rule: a plugin that wants the
web tool asks for it and degrades when it is missing, rather than failing to
load. This is what makes the `pi` profile possible without a build matrix.

### Disposal is the point

The reason to have a kernel at all is that unload has to be exact. Every
registration a plugin makes is recorded against that plugin, and unloading it
drops all of them in one step: tools deregister, commands vanish from the
palette, event handlers detach, provided services are withdrawn from anyone who
injected them, spawned tasks are cancelled. `ctx:effect` is the escape hatch for
state the kernel cannot see — an open socket, a temp directory, a child
process.

Without this, "reload" is a leak with good intentions, and the third reload of
a plugin during a long session is a different program from the first.

### The event bus

Handlers run in priority order and may do three things: observe, rewrite the
payload, or veto. This subsumes `src/hooks/` — a shell hook becomes a plugin
that subscribes to the same events — and gives Lua plugins the interception
points that today only `hooks.toml` has.

Events: `session_start`, `session_end`, `user_prompt`, `turn_start`,
`turn_end`, `pre_tool_use`, `post_tool_use`, `pre_model_call`,
`post_model_call`, `compaction`, `checkpoint`, `plugin_loaded`,
`plugin_unloaded`, `config_reload`.

A handler that panics or errors is logged and skipped. A broken plugin cannot
wedge a turn — the same guarantee `src/hooks/` gives today, extended to
everything.

## Plugin kinds

### Rust

```rust
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;
    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()>;
}
```

Compiled in behind a cargo feature named after the plugin. `src/plugins/mod.rs`
holds the one table mapping feature to constructor; it is the only file that
names every Rust plugin, and it is generated from the manifests rather than
hand-maintained.

### Lua

A plugin is a directory under `~/.wizard/plugins/<name>/` holding `plugin.lua`
and `manifest.toml`. `plugin.lua` returns a table:

```lua
return {
  name = "todo",
  apply = function(ctx)
    local store = {}
    ctx:tool { name = "todo", description = "...", parameters = {...},
               execute = function(args) return render(store) end }
    ctx:on("session_end", function() store = {} end)
    ctx:effect(function() store = nil end)
  end,
}
```

The VM is long-lived — one per plugin, created at load and dropped at unload —
which is the change from today's scripted tools, where each call gets a fresh
throwaway VM and can therefore hold no state.

## Manifest and capabilities

```toml
name = "web"
version = "1.0.0"
description = "Fetch and search the web"
capabilities = ["network"]
optional_deps = ["credentials"]
profiles = ["full", "server"]
```

Capabilities extend the two that already gate registry tools
(`crate::registry_client::Capability`):

| Capability | Grants |
| --- | --- |
| `filesystem` | `io.open`, `os.remove`, unconfined `wizard.fs.*` |
| `process` | `os.execute`, `io.popen`, `os.getenv`, `wizard.process.*` |
| `network` | `wizard.http.*` |
| `model` | `wizard.model.*` — spend tokens on the user's account |
| `ui` | `wizard.ui.*` — write to the transcript, open a picker |
| `agent` | `wizard.agent.spawn` — start subagents |

A plugin that declares none runs under `Stdlib::Sandboxed`: no `os`, no `io`,
no `package`, host file helpers confined to the project directory. Plugins that
ship with Wizard declare what they need and are granted it at build time.
Plugins installed from the registry prompt, exactly as tools do today, naming
the author and the grant.

`model` and `network` are new and are the two worth arguing about, because they
are the ones that cost money and leak data. Both are metered: a plugin's model
calls are attributed to it in `/cost`, and a plugin's HTTP goes through the same
allowlist `[web]` already applies.

Two corrections to that last sentence, made when the bridge was built.
`[web]` has no allowlist — it has an SSRF guard (`check_url`, which resolves
the host and refuses every private range) plus `allow_local` and
`fetch_max_bytes`. That is what a plugin's HTTP goes through, and it is a
tighter check than an allowlist would be, but it is not the one this paragraph
named. And "attributed to it in `/cost`" is half true: the spend is counted,
and `UsageTracker` has no dimension to say whose it was. See "Still open".

## The async problem

Today Lua runs one throwaway VM per tool call on `spawn_blocking`
(`src/tools/lua.rs:588`) and every host function is synchronous. A plugin that
has to await a model call or an HTTP fetch cannot be written against that.

The kernel enables `mlua`'s `async` feature and exposes host functions as
`create_async_function`, so Lua code awaits through coroutines and reads as
straight-line code. The VM stays on a dedicated task; the deadline hook and
memory ceiling from `sandbox.rs` still apply, and now bound a plugin's whole
lifetime rather than one call.

This is the single highest-risk piece of the design. It is built and proven
first, alone, before anything is ported.

## Profiles

A profile is a named plugin set. `install.sh` picks one; `~/.wizard/plugins.toml`
records it and can be edited afterwards.

| Profile | Contents | For |
| --- | --- | --- |
| `full` | every plugin | the default install |
| `server` | full minus GUI, minus TUI extras, plus gateway and ACP | headless boxes |
| `minimal` | core plus file, shell, git, todo | CI containers, a second machine |
| `pi` | minimal plus a local provider, JIT tuned, no mesh, no GUI, no web | Raspberry Pi and other small ARM |
| `custom` | `WIZARD_PLUGINS="a,b,c"` | anything else |

Rust plugins map to cargo features, so a profile is also a build: the `pi`
release asset does not link iced, quinn or the image stack at all. Lua plugins
are files, so a profile is also a copy — which is why `pi` can be narrowed after
install without a rebuild.

`WIZARD_MINIMAL` keeps working and means `WIZARD_PROFILE=minimal`.

## The async model, as proven

The design above was spiked before any of it was built, because a long-lived Lua
plugin that can `await` is the load-bearing assumption and LuaJIT is exactly the
runtime where it might not hold. Findings, all reproduced against
`mlua 0.12` with `luajit,vendored,send,serialize,async`:

**It works.** `create_async_function` yields from straight-line LuaJIT without
"attempt to yield across C-call boundary". A plugin can await in a loop, take a
table back from an async host call, hold state across await points, and keep
that state across separate `exec_async` calls on the same VM.

**The existing sandbox already covers the async case, and must be reused rather
than reimplemented.** `disable_jit` + `install_hook` (`src/tools/lua.rs`) bound
an `exec_async` chunk exactly as they bound a sync one: a bare `while true do
end` and a spin placed *after* an await point both stop on the deadline, to the
millisecond. An honest plugin that computes and awaits is not touched. A VM that
had one call bounded is still usable for the next one, which is what makes a
long-lived per-plugin VM safe.

Three details in that code are load-bearing and were each rediscovered the hard
way by reimplementing them wrongly first:

- `jit.flush()` after `jit.off()`. Without it, traces recorded before the switch
  survive, and `while true do end` runs in a compiled trace with the hook
  silent — forever.
- `set_global_hook`, not `set_hook`. mlua drives async on a coroutine, and a
  per-thread hook is not merely skipped there, it is *uninstalled for the whole
  VM* by mlua's own trampoline.
- `install_stop_guard`. A bound is signalled as an ordinary Lua error, so
  `coroutine.resume` turns it into a `false, msg` return value and the program
  continues. Reproduced: a spin inside `coroutine.create` burned the full
  deadline and then reported success.

**A bound costs the JIT.** `jit.off()` is what makes the instruction hook fire,
so a bounded plugin is interpreted. This is the existing trade and it maps onto
trust: first-party plugins in a profile run unbounded and keep the compiler,
registry plugins run bounded and lose it.

**A spinning plugin cannot be rescued by `tokio::time::timeout`.** Blocking Lua
never yields, so the timeout future is never polled. The in-VM hook is the only
real bound, which is why the above matters.

## As built: where the kernel departs from the design above

The kernel is implemented and the design above is what it was built from, so the
places it could not be followed are corrections, not notes. Each is pinned by a
test.

**`ctx:provider` is Rust-only.** The design says the `Ctx` shape is identical in
both languages. It is not, and cannot be: an `LlmProvider` is TLS and SSE
framing, which is the half this document itself puts in Rust. The call exists on
the Lua table and refuses, naming the reason
(`a_provider_cannot_be_registered_from_lua`).

**`ctx:provider` takes a descriptor, not a provider.** It took an
`Arc<dyn LlmProvider>` first, which made the call unusable for what it is for: a
provider instance is bound to one base URL, one model and one key, and all three
come out of the user's config, so no `kind = "..."` could ever name an instance
somebody had already constructed. It now takes a `ProviderDescriptor` — an id,
a display name, a credential policy, a `build(&ProviderConfig)`, and an optional
readiness hook — which is the thing the config side needs. `ProviderKind` stopped
being a nine-variant enum in the same change; `src/llm/registry.rs` has the
argument.

**A provider is registered in two places at once.** Every other registration has
the kernel as its consumer: a tool is copied out into the agent's registry, a
command into the palette. A provider's consumer is `ProviderConfig::build`,
which runs where no kernel handle exists — a unit test, `wizard doctor`, the
settings sheet's probe. So `insert_provider` writes the kernel's slot *and* the
process-wide registry in one step, and `remove_providers` sweeps both. Doing it
as a separate publish step, the way `install_tools_into` works, would leave a
window in which an unloaded plugin's provider was still selectable, and exact
unload is the reason there is a kernel
(`a_plugin_registered_provider_is_selectable_from_config`).

**`src/llm/builtin.rs` was the provider half of `src/plugins/mod.rs`, and it
is deleted.** It held the eight providers that were not plugins yet and seeded
the process registry from them. All eight went through the door; the registry
now starts empty and every kind in it was put there by a plugin. See the last
section.

**`SlashCommand` stayed a closed enum, and gained one open variant.** The
provider kind became a string plus a lookup because a closed enum meant no
provider could be a plugin. The same argument applies to slash commands and the
same fix does not: `SlashCommand` has 260 use sites, and unlike `ProviderKind`
its variants carry *parsed arguments* (`Mode`, `ReasoningEffort`, `UltraConfig`,
`ImportSelection`) that the one dispatcher matches on exhaustively. Turning it
into a string would push that parsing back out to the surfaces, which is the
drift `src/commands/` exists to prevent.

So the enum is the built-in spelling and `SlashCommand::Plugin { name, args }`
is the escape hatch, carrying the registered name and the raw rest of the line.
A plugin command is a `PluginCommand` in a runtime registry
(`src/commands/plugin.rs`) rather than a variant, and the two are merged by
`commands::listing(surface)` — the one list every surface completes, helps and
advertises from. First-class means all four of those: a plugin's `/name`
completes in the TUI popup and the window's palette, appears in `/help` and in
Telegram's `setMyCommands`, parses through `SlashCommand::parse`, and runs
through `commands::surface::dispatch` with no second path
(`a_plugin_command_runs_through_the_one_dispatcher`).

**Surface gating for a plugin command is availability, not a column.** A plugin
declares which surfaces it runs on (`PluginCommand::only`, or `surfaces = {...}`
from Lua) and the registry answers `Execution::Agent` there and
`Execution::Unavailable` everywhere else. The `Agent`/`Ui` split answers "which
half of a two-halved surface owns this command's semantics", and a plugin
command's semantics are in neither half — they are in the plugin. What the split
decides in practice is where the dispatch runs, and the agent-holding half is
the honest answer: it has a runtime, it is the only half the gateway has at all,
and it puts the output in the transcript in typed order. So `only(&[Surface::Tui])`
is a genuine "TUI only", enforced by the same line of `dispatch` that enforces
`/vim`'s (`a_plugin_command_can_be_tui_only_and_is_refused_elsewhere`).

**A command is registered in two places at once, like a provider.** For the same
reason: `SlashCommand::parse` runs in `App::submit`, in the window's `route` and
in the gateway's `apply_command`, none of which hold a kernel handle. So
`insert_command` writes the kernel's slot *and* the process-wide registry in one
step, and `remove_commands` sweeps both
(`a_plugin_registered_command_reaches_the_palette_and_leaves_with_the_plugin`).

**Conflict policy: the built-in keeps the name, and the first plugin keeps it
after that.** A claim on a name a built-in owns — including `/q`, which is a
parser alias with no table row — is refused, logged with both sides, and leaves
nothing behind in either registry
(`a_plugin_cannot_shadow_a_built_in_slash_command`). Shadowing was the
alternative and is wrong here specifically because a slash command is muscle
memory: `/clear` is typed without reading, and a plugin that quietly took it
would be discovered by losing a conversation, whereas a plugin's `/todo` failing
to appear is discovered by reading `/help`. The refusal is a `Result`, so a
plugin with a fallback name can catch it and carry on.

**A plugin command is not on the agent's `run_command` allowlist.** Every entry
of that allowlist is an argument about one command's blast radius — read-only?
needs a human at a picker? reaches outside the session? — made in
`SlashCommand::agent_runnable`. A plugin cannot make that argument about itself,
and an `agent_runnable = true` field would be a plugin grading its own homework.
A plugin that wants to be model-callable registers a *tool*, which is the API
that already carries a capability grant. This is the one place a plugin command
is deliberately not equal to a built-in.

**No command is a plugin yet.** The thirteen the migration earmarks — `/evolve`,
`/publish`, `/fusion`, `/ultra`, `/server`, `/login`, `/resume-claude`, the
`ImportClaude` half of `/settings`, `/memory`, `/doctor`, `/todos`, `/cost`,
`/compact` — are still built-ins in `COMMANDS`, still compiled in, still
registered eagerly, and none is behind a cargo feature. As with the providers:
the door is open and nothing has gone through it.


**Capabilities are finer-grained than `Stdlib` is.** `Stdlib::Sandboxed` drops
`os` and `io` wholesale, so `filesystem` and `process` would both have to open
the full standard library and would each imply the other. `narrow_stdlib` closes
the gap by blanking the *other* capability's names: `filesystem` alone gets
`io.open` without `os.execute`, `process` alone the reverse. Confinement of
`wizard.fs.*` follows the `filesystem` capability specifically rather than the
library profile, so a `process`-only plugin is still pinned to the project
directory.

**A service cannot be taken back from whoever injected it.** "Provided services
are withdrawn from anyone who injected them" is not implementable as written,
because `inject` hands out an `Arc` and an `Arc` cannot be revoked. Plain
`inject` therefore returns a snapshot that stays alive; `ServiceRef` re-resolves
by name on each use and starts answering `None` the instant its provider
unloads. Use `ServiceRef` for anything held across a possible unload.

**A bound is per call, not per lifetime.** The memory ceiling applies
continuously, but the compute deadline is pushed on each call and the latched
stop flag is cleared when the VM goes idle. Read literally, a lifetime deadline
would kill a plugin loaded at 09:00 thirty seconds later.

**There is no `lua/sandbox.rs`.** The file list above names one; the spike
section says to reuse `src/tools/lua.rs` rather than reimplement it. The latter
won, so `lua/` has two files. `sandboxed_libs` and `blank_globals` widened to
`pub(crate)` — the alternative was a second copy of the one allowlist whose
accidental widening is a supply-chain hole.

**Handler priority: lower runs first.** `DEFAULT_PRIORITY` is 0 and the type is
signed, so a plugin can order itself ahead of everything without knowing how many
others exist.

## As built: the kernel is live, and anthropic is a plugin

The section above described a kernel nothing called. It is called now, and one
provider has come out of `src/llm/builtin.rs` and into it. What follows is what
that took and what it cost.

### `src/plugins/mod.rs` is the table, and the process kernel

The file this document predicted exists. It holds `compiled_in()` — one line
per Rust plugin, each behind its cargo feature, and the only place in the tree
that names one — and the `OnceLock<Kernel>` they load into. There is one kernel
per process for the same reason `llm::registry`'s `INSTALLED` is a global:
there is one set of installed plugins per process, by construction. `Kernel`
itself stays instantiable more than once, because every kernel test makes its
own.

### Where startup calls it, and why there

`crate::run` — the top of it, above the dispatch chain rather than inside any
arm. `src/lib.rs` has seventeen entrypoints and every surface is one of them:
the TUI, `wizard -p`, `--gateway`, `acp`, `mcp serve`, `fleet run`, `doctor`,
the scheduler, `evolve`, `publish`, `sync`, `update`, `skills`, `peers`,
`harness`, `desktop-setup`, `agents`. Subagents and `run_code` programs are not
separate entrypoints: they compose from the same registry the agent got. One
call above the chain gives all of them the same plugin set; a call per surface
would be seventeen places to forget, and the ones that get forgotten are the
headless surfaces nobody watches start up.

`--cwd` is passed to `boot` rather than applied by it, because each arm does
its own chdir further down and the kernel needs the project root *now* — it is
what confines a sandboxed plugin's file helpers, and a confinement computed
from the wrong directory is worse than none because it looks like it is
working.

### Loading is in two halves, at two different times

Rust plugins load **lazily, synchronously, inside the `OnceLock`**. Their
`apply` is a handful of map inserts, so there is nothing to defer, and being
synchronous is what lets `llm::registry` reach them from `ProviderConfig::build`
— which runs in unit tests, in `wizard doctor`, and in the settings sheet's
probe, none of which hold a kernel handle and some of which have no tokio
runtime.

Lua plugins load **once, from `boot`, asynchronously**. They are files: a
`read_dir` of `~/.wizard/plugins`, then a VM and a script per plugin. On a
machine with no plugins installed the whole of startup's plugin cost is one
`read_dir` that returns `ENOENT`. With plugins installed it is one LuaJIT VM
each, which is the cost the user asked for by installing them. Nothing is
deferred beyond that, and nothing needed to be.

A user plugin loads as `PluginSource::Registry`, i.e. bounded and interpreted.
First-party status is a property of shipping *in the binary*, and the binary is
`compiled_in()`.

### Failure is a warning, at every step

A plugin that will not load costs its own registrations and nothing else. Every
load site logs and continues, and the Rust half additionally wraps `apply` in
`catch_unwind`: a compiled-in plugin is still third-party code from the
kernel's point of view, and "wizard will not start" is not an acceptable
outcome for a broken one. The `AssertUnwindSafe` is sound because every kernel
registry recovers from lock poisoning already, so an interrupted `apply` leaves
a partially-filled map that reads normally rather than a torn one.

### `install_tools_into` is used, in exactly two places

`crate::agent::build_tool_registry` is the funnel every agent-bearing surface
goes through, and plugin tools go into its `base` registry — the one subagents
are scoped from and the one a `run_code` program reaches — after the scripted
and MCP tools and before the harness overrides. That ordering is the
precedence: plugin beats MCP beats scripted beats native, which is what lets a
plugin deliberately replace a builtin, and being ahead of the overrides means a
harness bundle rewrites a plugin's tool descriptions exactly as it rewrites
everyone else's.

`mcp serve` is the one surface that composes its own registry instead, so it
has its own line. Without it, an MCP client would see a different tool set than
the agent does from the same install.

### Anthropic is a plugin

`src/plugins/anthropic.rs`, behind `--features provider-anthropic`, on by
default. It registers through `Ctx::provider` at kernel boot and no core
module names it. `kind = "anthropic"` in an existing `config.toml` resolves to
the same descriptor, builds the same client and puts the same bytes on the
wire; the only thing that changed is who registered it.

It was chosen because a dependency audit found it the only truly free split:
nothing in `src/llm/` reaches into it, and it reaches back only for the
streaming helpers every adapter shares. Everything Anthropic-shaped — the block
translation, the SSE decoder, the cache-breakpoint arithmetic — was already in
that one file.

**`--no-default-features` builds, tests and runs**, with no Anthropic transport
linked at all. `kind = "anthropic"` then resolves to nothing and the error says
so and lists what is installed, which is the degrade-when-missing rule this
document already required of an absent plugin's kind.
`contrib/check-plugin-work.sh` has a leg that builds and tests that
configuration, and `plugins::anthropic_is_present_exactly_when_its_feature_is`
asserts both sides of the feature.

### Two corrections this half forced

**The registry ensures on read, not at startup.** `llm::registry::installed`
and `kinds` call `plugins::ensure_providers()` before answering. They have to:
a provider plugin's registration must be visible to `ProviderConfig::build` no
matter who calls it, and in a test binary nobody calls `run`. `install` ensures
too, so a plugin loaded into some *other* kernel cannot take a kind merely
because nothing had looked one up yet — which would otherwise make
`a_plugin_cannot_take_a_provider_kind_another_plugin_holds` depend on test
ordering. The
re-entrancy that creates (a loading plugin's own `install` calling back into
the `OnceLock` it is inside) is closed by a thread-local `LOADING` flag rather
than by rule, because a rule is a thing the next provider conversion forgets.

**`ProviderKind::ANTHROPIC` stays in core.** A `kind` is a string a user writes
in a file, and core is allowed to hold the string — to offer it in the
onboarding menu, to compare against one somebody typed — as long as it never
names the type behind it or constructs one. Every use is already guarded by a
registry lookup that returns `None` when the plugin is absent. Gating the
constant would have pushed `#[cfg]` into onboarding's numbered menu, the TUI's
provider picker and the settings presets, which is the hand-written-menu
problem `src/llm/registry.rs` already flags as its own change.

## As built: every provider is a plugin, and `builtin.rs` is gone

The section above described one provider going through the door and eight
still to go. The eight have gone, and `src/llm/builtin.rs` — the one file that
still named a concrete provider type — is deleted. `llm::registry`'s process registry now starts **empty**: what a
build answers to is exactly the set of plugins it was compiled with, and
nothing else can put a kind in it.

### Seven features over nine kinds

| Feature | Registers | Lives in |
| --- | --- | --- |
| `provider-anthropic` | `anthropic` | `src/plugins/anthropic.rs` |
| `provider-openai` | `openai`, `openrouter` | `src/plugins/openai/` |
| `provider-ollama` | `ollama` | `src/plugins/ollama.rs` |
| `provider-llamacpp` | `llamacpp` | `src/plugins/llamacpp.rs` |
| `provider-cloudflare` | `cloudflare` | `src/plugins/cloudflare.rs` |
| `provider-xai` | `xai`, `xaioauth` | `src/plugins/xai.rs` |
| `provider-chatgpt` | `chatgptoauth` | `src/plugins/chatgpt/` |

All on by default; a stock build behaves exactly as before.

The grouping is one feature per *backend*, not per kind, because the two
multi-kind features would otherwise ship a cargo flag whose entire content is a
credential variant. `xai` and `xaioauth` are one endpoint, one wire shape and
one vendor label differing only in where the bearer token comes from — forty
lines between them. `openrouter` is the `openai` kind with a fixed base URL and
two attribution headers, and splitting it would also permit a build that had
`openrouter` but not the `openai` kind that vLLM, LM Studio, DeepSeek and every
`compat.rs` preset are configured as: a combination nobody wants and everybody
would have to test.

### What stayed in core, and why

Three modules that look like providers are not.

**`src/llm/wire.rs`** is the OpenAI-protocol machinery — request shape, SSE
decoding, the bearer-token seam, retry classification. Five of these backends
build on it. A shared transport that lived inside one plugin would be a
dependency edge between plugins, and deleting that plugin would break four
others.

**`src/llm/oauth_callback.rs`** is the loopback redirect both sign-ins come
back on, and it gained `generate_pkce`, `pkce_challenge` and `jwt_exp` in this
change. Those were in `xai_oauth.rs` and `chatgpt_oauth.rs` imported them from
there — an edge between two backends with nothing to do with each other, and
once each is a plugin, an edge that makes deleting one break the other. RFC
7636 is not xAI's.

**`src/llm/xai_oauth.rs`** — the token store, the sign-in flow and
`XaiTokenSource` — is the interesting one, because it *is* xAI's and it is
still core. The split is by consumer rather than by subject: five of the six
things that read it are not the chat provider. `plugins/web.rs` authenticates
xAI's server-side **search** API with those tokens and `tools/image.rs` its
**image** API — both core tools, both reaching for xAI whatever chat backend is
configured — `sync.rs` backs the token file up, and onboarding and
`app/prompts.rs` ask whether a session exists. Moving the store into the plugin
would mean a build without `provider-xai` lost web search and image generation,
which have nothing to do with which model answers a turn. So `src/plugins/xai.rs`
is forty lines: the two descriptors saying which credential goes with which
kind, which is the whole of what is provider-shaped about xAI.

ChatGPT's sign-in is *not* core, by the same test: nothing outside that plugin
reads its token store, so the store has exactly one consumer and ships with it.

### Provider default strings moved to core, provider types did not

`llm::registry::defaults` holds OpenRouter's and Cloudflare's base URLs, model
tags and key env vars. Onboarding's numbered menu, the TUI provider picker, the
settings sheet's preset table and `wizard doctor` all print them, and they have
to keep printing them on a build compiled without those plugins. This is the
`ProviderKind::ANTHROPIC` argument applied one level down: core may hold the
*text* a user would otherwise type, as long as it never names the type behind
it or constructs one. The alternative was `#[cfg]` inside a numbered menu,
which is the hand-written-menu problem `src/llm/registry.rs` has flagged from
the start and which gating the strings would have made harder to fix, not
easier.

### The edges that had to be cut

Four core modules reached into a provider for something that was not a string.

- **`agent::error_is_transient` downcast `OllamaError`.** That arm was already
  dead: Ollama's `typed()` puts a `ProviderError` at the *head* of the same
  `anyhow` chain and the two classifications are the same predicate over the
  same statuses, so the `ProviderError` branch above it always won. Deleted.
  One downcast, not one per backend, is the shape a plugin boundary needs.
- **`kernel/tests.rs` built an `OllamaClient`** as a convenient provider that
  needs no key and no reachable endpoint. It builds a `wire::OpenAiProvider`
  now. A kernel test that names a plugin stops compiling the moment that
  feature is left out, which is the failure the kernel exists to prevent.
- **`--login chatgpt` and the GUI's sign-in sheet** are `#[cfg]`-gated on
  `provider-chatgpt`. `--login xai` is not, because its store is core. The
  sheet's `SUPPORTED` table is gated in step with its `begin` match, and the
  `debug_assert` already tying those two together is what keeps them honest.
- **Onboarding's "model already pulled" note** calls the Ollama plugin's tag
  canonicalizer (`ollama list` prints `llama3:latest` where a config says
  `llama3`), so that one branch is gated too. Without the plugin there is no
  `kind = "ollama"` to advise about.

### Proving it, one plugin at a time

`--no-default-features` proves the floor and the default build proves the
ceiling, and neither catches the case in between: a core module that reached
into `provider-ollama` compiles with everything off (the module it reached into
is gone too) and compiles with everything on. It only fails with that one
feature missing and the rest present.

So `contrib/check-provider-plugins.sh` builds and tests each leave-one-out set
plus the all-off floor — eight feature sets. It is the slow gate; run it when
the plugin set or the boundary moves.
`plugins::a_kind_is_installed_exactly_when_its_plugin_is_compiled_in` is the
in-tree half: one row per feature, both directions asserted, plus a sweep that
fails if a kind reached the process kernel that no compiled-in plugin claims —
which is what would catch `builtin.rs` coming back. That sweep reads the
*kernel's* slot rather than `registry::kinds()`, because the process registry is
shared with every other test in the binary and the kernel tests that exercise
`Ctx::provider` leave their own kinds in it.

`a_stock_build_still_answers_to_all_nine_shipped_kinds` is the other side: the
nine ids as literal strings, which is what a user's `config.toml` actually
holds. `builtin.rs` used to make that assertion over its own table, so it could
only ever agree with itself.

**The `--no-default-features` test count drops, and that is arithmetic rather
than a regression.** It was 2521 with eight providers still compiled in
unconditionally; it is 2431 now that all of them are behind features, because
that leg no longer compiles their test modules. The default leg is the one the
ratchet in `contrib/check-plugin-work.sh` guards, and it went 2536 → 2557.

### Still open

- **The onboarding menu and the TUI provider picker are still hand-written.**
  On a build without `provider-anthropic` they still offer Anthropic, and
  picking it produces a config that fails at `build()` with a clear message
  rather than an entry that was never offered. Building both from
  `registry::kinds()` is the fix and is the same change `src/llm/registry.rs`
  has been asking for.
- **Eight providers to go**, plus everything that is not a provider.
- **A plugin's spend is in `/cost`'s total and nowhere else.** `UsageTracker`
  is nine bare atomics with no keyed dimension in it, so `wizard.model` bills
  through `record_delegated` exactly as a subagent does and is then
  indistinguishable from the turn's own tokens. `docs/plugins.md` promised "a
  plugin's model calls are attributed to it in `/cost`"; half of that is true
  (the money is counted) and half is not (it does not say whose). The honest
  fix is a keyed bucket on the tracker and a `source` on `UsageRecord`, and it
  is a usage change rather than a plugin one.

## As built: the host bridge

`src/plugins/host.rs` is the `HostBridge` the section above left open, and
every namespace on it resolves to code that already existed:

| Namespace | Reached through |
| --- | --- |
| `wizard.fs` | `install_wizard_lib`, confined to the project root without `filesystem` |
| `wizard.http` | `web_client` + `check_url` + `get_following_redirects` + `read_capped` |
| `wizard.process` | `shell::run_command_cancellable` |
| `wizard.model` | the agent's live `LlmProvider`, drained through `collect_text_billed` |
| `wizard.ui` | `AgentEvent::Notice` on the turn's channel |
| `wizard.agent` | the registered `spawn_subagent` tool |

Nothing here is a second implementation. That is the whole design, and the two
places it was tempting to write one are worth naming: a second HTTP client is a
second place to forget that reqwest's redirect policy is synchronous and
therefore cannot re-resolve a hop — which is the entire SSRF guard bypassed —
and a second subagent spawner is a second place to get the pane events, the
read-only gate, the shared breaker and the foreground/background cancellation
split wrong. Three functions in what was then `src/tools/web.rs` widened to `pub(crate)` and
one new entry point beside `run_command`; that was the whole cost.

**The live agent arrives through a slot, and that is a real limitation.** Four
of the six namespaces need something only a running agent has — a provider, a
token tracker, a cancel handle, an event channel, a tool registry — and the
kernel is built long before any of them exist, from `llm::registry`, from
`wizard doctor`, from a unit test. So `WizardHost` holds a slot an agent fills
through `host::bind`, called from `Agent::new`, from `set_model`, from
`set_client` and from the top of every turn (which is when the event channel is
known). Binding from the agent rather than from each surface is the same
argument `boot` makes about `crate::run`: every agent-bearing surface builds an
`Agent`, and the surfaces that get forgotten are the headless ones. **Last
binder wins**, so two agents in one process — a fleet run, a gateway serving two
sessions — share the slot and a plugin bills whichever bound most recently.

**Unbound, four namespaces still answer and two refuse.** `wizard.http` has the
`[web]` defaults and `wizard.process` has the kernel's project root, which is
exactly right for a plugin-only process. `wizard.ui.notify` writes to the log
and returns `Ok`: a notice's failure mode is nobody hearing it, and the log is
somewhere it can be heard. `wizard.model` and `wizard.agent` **refuse**, and
the alternative — building a provider from `Config::active()` on the side — is
specifically wrong, because that provider has no tracker behind it and the
spend would never reach `/cost`. Unmetered spend on the user's key is worse
than a clear error.

**Everything that can block observes the turn's cancel handle.** HTTP and the
model call are a `tokio::select!` against `agent::cancelled`, because dropping
a reqwest future or a `ChatStream` is a clean abort. A child process is not —
dropping it reaps the shell and orphans whatever it forked — so
`run_command_cancellable` was added beside `run_command`, selecting on the
handle *inside* the runner where `kill_group` is. The existing capture callers
kept their signatures and pass no handle. The subagent path hands the handle
down as `SpawnOptions::cancel`, which is what a foreground `spawn_subagent`
already did.

**A plugin's HTTP body comes back as text, not as markdown.** `web_fetch`
converts HTML because it is feeding a model prose; a plugin calling an endpoint
wants the endpoint's answer. Everything else is the web tool's: `allow_local`
decides whether loopback is reachable, `fetch_max_bytes` caps the body while it
streams, and the result is defanged. Redirects are followed for `GET` and
refused for `POST`/`PUT`, because following one with a body means replaying
that body — very possibly a credential — to a host the plugin never named.

**Host errors reach Lua flattened.** `external` formatted with `to_string()`,
which prints only the outermost layer, and a host call's reason is almost
always underneath one. `{:#}` now, so `wizard.process.run` fails as "tool '...'
failed: exited 3" rather than as "tool '...' failed".
- **The onboarding menu and the TUI provider picker are still hand-written**,
  and now they are hand-written menus of *seven* removable plugins rather than
  one. A stripped build still offers a backend it does not have; picking it
  writes a config that fails at `build()` with the named error rather than an
  entry that was never offered. Building both from `registry::kinds()` is the
  fix, and it is a bigger change now than when it was one provider.
- **`tools/image.rs` still branches on four provider kind ids.** The question
  it asks — "does this backend serve an image API, and under which
  credential" — is a *capability*, and the descriptor does not carry one.
  Adding an image field to a chat-shaped type to satisfy one tool is the wrong
  fix; the right one is a service the provider plugin provides and the tool
  injects, which is what `Ctx::provide` / `Ctx::inject` are for and which is
  its own change.
- **The host bridge is still `UnwiredHost`.** A Lua plugin that calls
  `wizard.http` or `wizard.model` gets an error naming the reason.
- **The thirteen earmarked commands are still built-ins.** (The window has
  since gone through the door; see the section below. Nothing that is a
  *command* has.)

## As built: the window is a plugin, and it is the first one that is not a provider

`src/native/` (the iced window, ~15.7k lines) and `src/gui/` (the agent core
under it — sessions, the config store, git, OAuth, ~5.1k) are now
`src/plugins/native/` and `src/plugins/gui/`, behind the existing `native`
feature, registered through the kernel. Seven provider features became eight
plugin features, and the eighth is a surface.

It was picked for the same reason anthropic was: a dependency audit found it
the cleanest split in the tree — 33 outgoing edges and **zero incoming**.
Nothing in core referenced either directory except one line, and that one line
is the whole of what this change is about.

### The one edge, and how it was inverted

`src/lib.rs`'s dispatch chain had this:

```rust
#[cfg(feature = "native")]
{
    let config = config::Config::load()?;
    return native::run(config).await.map(|()| 0);
}
```

That is rule 1 broken in the open — a core module naming a plugin — and it
compiled either way, which is why it survived a year. It is now:

```rust
if let Some(window) = entrypoint::installed(entrypoint::GUI) {
    let config = config::Config::load()?;
    return window.run(config).await.map(|()| 0);
}
```

with the `#[cfg(not(feature = "native"))]` bail underneath it becoming an
ordinary `else`. There is no `#[cfg]` left in that arm. The window
`provide`s an `Entrypoint` under the name `"gui"` in its `apply`, and core
injects one.

### Why an entrypoint service rather than `ctx:command`

`Ctx::command` exists, it registers something a plugin owns, and it is the
wrong hook. Three reasons, and the third is the one that decides it:

- A `PluginCommand` is a `String -> String` body. `wizard gui` takes no
  arguments and returns nothing; what it does is *not return* until the window
  closes.
- A slash command runs inside a session, on a surface that is already up.
  `wizard gui` runs before there is a session — the window builds its own
  `TaskManager` and its agents lazily, per chat.
- `src/commands/plugin.rs` deliberately refuses a plugin command the
  `CommandSurface` verbs, because handing a plugin `&mut App` makes unload
  unsafe in a way the ledger cannot fix. A window is `&mut App` and then some.

Registering it as a slash command would have produced a `/gui` in the TUI
palette that opens a second surface out from under the first. `wizard gui` is
a **CLI subcommand**, parsed by clap in `src/cli.rs`, and the thing that had to
become pluggable is its *body*.

So `src/entrypoint.rs` is a new core module holding one concrete type and one
lookup — an `Entrypoint` is a boxed `Fn(Config) -> Future<Output = Result<()>>`
plus its name, and `installed(name)` injects one out of the process kernel.
This is the `ProviderDescriptor` shape at one remove: the consumer defines it
(the consumer here is the dispatch chain), the plugin supplies "how to start
one", and an absent plugin is a `None` that becomes a sentence rather than a
link error.

A concrete struct rather than a trait for a mechanical reason: `inject_as` is
an `Arc<dyn Any>` downcast and `Arc::downcast` needs a `Sized` target, so
publishing an `Arc<dyn Trait>` means the injector has to name
`Arc<Arc<dyn Trait>>`. One closure in a struct is the same expressiveness with
none of that.

### Why the `#[cfg]`-gated arm was not simply kept

It works, and that is the trap. The cost of keeping it is not this plugin, it
is the next one: core pays one `#[cfg]` per plugin that owns a surface, and
the gateway, ACP and `mcp serve` are all the same shape. A name in a registry
costs core one lookup, once, forever.

### What core still holds

The string `"gui"` and the paragraph printed when nothing answers to it — the
one telling the reader to run `install.sh` with `WIZARD_NATIVE=1` or to build
with `--features native`. Same rule as `ProviderKind::ANTHROPIC`: core may
hold the text a user types and the prose explaining how to get the thing
behind it, as long as it never names the type or constructs one.

### Two directories, one plugin

`gui` stays a sibling of `native` under `src/plugins/` rather than becoming a
module inside it. It registers nothing and it draws nothing — it is the half
of the GUI that would survive another front end being written against it, and
`src/plugins/native/mod.rs` is explicit that the window is a *client* of it.
Nesting it would say the window owns it. `compiled_in()` therefore has one
`native` line covering two directories, which is the same thing
`provider-openai` does across `openai/` and its `openrouter.rs`.

### The feature name did not change

`native`, exactly as before. `install.sh` reads `WIZARD_NATIVE=1`, the
`native` job in `.github/workflows/release.yml` publishes
`wizard-native-<target>.tar.gz`, and `docs/native-gui.md` spells it
throughout. Renaming it to `plugin-native` for symmetry with
`provider-anthropic` would break the release pipeline to make a table look
tidier.

### What did not change

The transport, so to speak: `--features native` builds the same window, opens
the same first chat, draws the same frame. Two mechanical fixes were needed
for the move and nothing else — `include_bytes!("../../assets/fonts/…")` in
`font.rs` gained a `../`, and the source-scanning test in `tests.rs` that
reads `src/native/{pane,rail}.rs` off disk follows the new path. A default
build is byte-identical in behaviour: it never compiled these modules before
and does not now.

### Still open, specific to this

- **`graph/` is still deferred and still unreachable**, exactly as it was.
  Moving the directory did not wire it in.
- **The window's plugin declares every capability and none of them is
  enforced.** `Capability` gates the Lua host bridge, and a compiled-in Rust
  plugin reaches past it into the crate directly. The declaration is honest
  documentation — it is what `wizard doctor`'s plugin listing shows — and it
  is not a sandbox. Making a compiled-in plugin's capabilities mean something
  is a kernel change, not this one.
- **The other three surfaces are still core.** `wizard acp`, `wizard gateway`
  and `mcp serve` are the same shape as `wizard gui` and would each be one
  `Entrypoint` registration, but none of them is behind a feature yet, so
  there is nothing to remove and the door being open is the whole of the
  progress.

- **No *command* has gone through the door yet.** The thirteen earmarked ones
  are still built-ins.

## As built: two subsystems that are not providers

`graph` and `tool-web` are the first plugins that are not a backend, and they
were picked by the same dependency audit that picked anthropic: the two
cheapest splits left. What they cost was not the move.

### `tool-web`, and where the line through `web.rs` is

`src/tools/web.rs` was 3.3k lines with **zero** core references — no module
outside `src/tools/registry.rs` named `WebFetchTool`, `WebSearchTool` or
`XSearchTool`, and the registry names every tool. On the audit's numbers it was
a lift-and-shift.

It is not, and the reason is the half of that file that is not a tool. Three
callers share it and only one of them is the web tool:

| caller | what it needs | why it is not the web tool's business |
| --- | --- | --- |
| `plugins/host.rs` | `web_client`, `check_url`, `get_following_redirects`, `read_capped` | `Capability::Network` is granted on builds with no web tool; the promise that grant makes lives here |
| `tools/image.rs` | the same walk, with `HopScheme::HttpsOnly` | `generate_image` downloads a provider-named URL to the user's disk |
| `plugins/web.rs` | all of it | the tools |

So the file split in two. `src/tools/http.rs` is core and holds the client, the
SSRF guard, the hand-walked redirect chain and the body cap; `src/plugins/web.rs`
holds the three tools, the HTML reader and the five search backends. This is
`src/llm/wire.rs` against `src/plugins/openai/` again — shared protocol
machinery in core, the vendor-facing thing in the plugin — and the argument is
the same one this document already makes about a shared transport inside one
plugin being an edge between plugins.

Putting the plumbing in the plugin would have been worse than untidy. A build
without `tool-web` would have kept `wizard.http` and `generate_image` and lost
their SSRF guard, which is a security property disappearing as a side effect of
a cargo flag: exactly the failure the boundary exists to make impossible rather
than merely unlikely. There is also a specific reason not to have two copies —
reqwest's redirect policy is a *synchronous* callback and therefore cannot
re-resolve a hop, so any client that keeps the default follow-10 policy has
bypassed the whole guard. One place gets that right and everybody starts from
it.

`[web]` in `config.toml` stays core for the same reason: `allow_local` and
`fetch_max_bytes` are promises about what this *process* does on the network,
not settings for one tool, and a build without the plugin still reads and obeys
them.

**A missing tool degrades differently from a missing provider, and that is the
whole point.** An absent `kind` still has a string a user can type, so
`registry::unknown` names it and lists what is installed. An absent tool has no
such affordance: the only correct behaviour is to be *absent from the roster*,
because the roster is what the model is told it can call, and a tool advertised
but unrunnable costs a turn to discover in the middle of somebody's work.
`plugins::a_tool_is_registered_exactly_when_its_plugin_is_compiled_in` and
`plugin_tools_reach_the_agents_registry_and_only_when_compiled_in` assert both
halves.

Two consumers had to stop assuming "native" meant "all". `harness export` now
composes native + plugin tools, so a bundle describes what its binary can do
(a build without `tool-web` exports no `web_fetch.md`, and `tests/cli.rs`
expects that). `mcp`'s `RESERVED_TOOL_NAMES` went the other way and keeps the
three web names *unconditionally*: the list is about names, and a name Wizard
can register must not be claimable by an MCP server on a stripped build, or it
would work until somebody rebuilt with the feature on. Core holding the string
while never naming the type is the `ProviderKind::ANTHROPIC` rule.

### `graph`, and the first plugin that registers nothing

`src/graph/` is 2.6k lines with one outgoing edge (to `mesh`) and one consumer
(`src/native/`). It moved to `src/plugins/graph/` behind `--features graph`,
on by default.

Its `apply` is empty, and that is a decision rather than an omission. `Ctx`
registers the four things a plugin hands the *kernel* — a tool, a command, a
provider, an event handler — and what this plugin produces is a `MeshGraph` and
a `Layout` over it, which one screen constructs by name. There is no
registration for "a type another module builds", and providing a service nobody
injects in order to have a line in that function would be decoration.
`the_graph_plugin_loads_and_registers_nothing` pins it, so the day it grows a
tool is a deliberate day.

It is a plugin in the two senses this document says are load-bearing: it is
behind a cargo feature and can be left out, and no core module names it. Its
consumer, `src/native/graph/`, is gated on the same feature — not on `native`
alone — because a plugin whose removal breaks the build is not a plugin. That
costs nothing today: `src/native/mod.rs` records the explorer screen as
"deferred, not reachable", so `--features native` without `graph` is the window
that already ships. `tests/graph_explorer.rs` is
`#![cfg(all(feature = "native", feature = "graph"))]` and compiles to nothing
without either.

### `crate::mesh::is_invisible` became `crate::text::is_invisible`

`defang` reached into `mesh` for the "what does a renderer draw as nothing"
table, and `memory.rs` did too. With the web tools becoming a plugin and `mesh`
on its way out of core, that was a plugin-to-plugin edge waiting to happen. The
table moved down into `src/text.rs` and all three callers ask core; nothing
about it changed, because what is invisible is a property of Unicode rather
than of the mesh. The bidi-table assertion moved with it, which is where a test
of a table belongs.

### Two bugs in the old `web.rs`, fixed on the way past

Both were found while wiring the host bridge, both predate this change, and
both are in the code the split was already rewriting.

**The search path had no size cap at all.** The fetch path has honoured
`fetch_max_bytes` since it was written; `send_following_redirects` handed its
response to `.text()` or `.json()`, which read to EOF. Three of the five
backends point at an operator-supplied `base_url` and the DuckDuckGo one parses
whatever HTML comes back, so "it is a reply to a request we made" was never a
bound. `SEARCH_MAX_BYTES` is 2 MB and the read *refuses* rather than truncates,
for the reason that function's own doc comment gives about silence: a truncated
search page parses to fewer results, or none, and reports success.

**`FETCH_TIMEOUT` was per-`send()`, not per chain.** A reqwest client timeout is
per request and a chain is `MAX_REDIRECTS + 1` requests, so a server that
answered each hop just inside thirty seconds could run for five minutes under a
budget every caller and every doc comment called thirty — and a hostile server
picks both the hop count and the delay, which makes it the cheapest way there
is to pin an agent turn. Both walkers now take an explicit `budget` and wrap the
loop in it; `generate_image` passes its own, longer one. The tests assert the
clock and not only the message, because running out of *redirects* also returns
an error and would satisfy a message-only test while taking the full unbudgeted
time.

### Proving it

`contrib/check-tool-plugins.sh` is `check-provider-plugins.sh` for these two,
and it exists because the combinations that matter here are ones that script
never builds: `graph` left out **with the GUI present**, which is the only way
to catch `src/native/graph/` reaching for an absent plugin, and `tool-web` left
out with everything else present. Four legs.


## As built: the mesh is a plugin, and it took two seams to get it out

`src/mesh/` (~11.7k lines) is `src/plugins/mesh/` behind `--features mesh`, on
by default. It was the hardest split left and the audit said so: **thirty**
core-to-mesh references against anthropic's zero and the window's one, and no
amount of moving files was going to reduce that number on its own. What it
actually took was two new seams and one trait, and the count is now zero.

### Where the thirty went

Most of them were doc comments, and a doc comment that names a plugin is a
broken intra-doc link on a build without it rather than an architectural
problem, so those became plain code spans. Four were real, and each needed a
different answer.

**`src/app/tee.rs` was a core file that was entirely mesh glue.** 685 lines
importing ten mesh symbols, holding a `Mesh`, a `QuicTransport` and a
`Discovery`, hung off `App::handle_agent_event`. `App` held
`pub mesh: Option<MeshTee>` and `app::runtime::run_tui` called `MeshTee::join`
by name.

The file moved to `src/plugins/mesh/tee.rs` and what stayed behind under the
same path is the *shape*: a `SessionTee` trait with three methods, and a
`TeeFactory` a plugin `provide`s under the name `"session-tee"`. `App::mesh` is
an `Option<Box<dyn SessionTee>>` now, `app::tee::join` is the lookup, and a
build without the mesh has a `None` there that nothing can fill.

A trait rather than the `Entrypoint`-style struct because a tee is not one
closure: it is a live object with a bound socket, a running mDNS advertisement
and a `leave` that has to say goodbye over the wire, which is why `leave` takes
`self: Box<Self>` and returns a boxed future. The `Arc<dyn Any>` downcast
problem `entrypoint.rs` documents does not arise, because what is *injected* is
the factory — a struct, like `Entrypoint` — and the trait object is what the
factory returns.

**Every word the user reads about the mesh listening now comes from the
plugin.** `src/app/runtime.rs` prints `tee.joined_notice()` on success and
`{err:#}` on failure, and nothing else. The failure sentence ("mesh: not
listening — … this session runs normally; no peer can watch it") is written in
`plugins::mesh::tee::factory`, because core saying "mesh" about the thing on
the other end of a lookup is core knowing what registered there.

**`wizard peers` is a whole clap subcommand tree whose `trust` argument is
`mesh::Trust`.** This is the one that could not be solved the way `wizard gui`
was. `Command::Gui` carries no arguments, so core's clap variant names no
plugin type; `PeersCmd::Trust { state: Trust }` names one in a `#[derive]`.

Mirroring `Trust` into core was the obvious fix and is specifically wrong:
`Trust` derives `clap::ValueEnum` on the peer store's own type precisely so a
second spelling on the argument-parsing side cannot drift into a fourth state,
and its doc comment has said so since it was written. A CLI able to express a
decision `peers.json` cannot record is worse than a slightly clumsier `--help`.

So the argument list crosses **unparsed**. Core's variant is
`Peers { args: Vec<String> }` with `trailing_var_arg`, `allow_hyphen_values`
and — the load-bearing one — `disable_help_flag`, without which clap answers
`wizard peers --help` in core with a usage line reading `wizard peers [ARGS]...`
and no mention of the eight subcommands. `entrypoint::Subcommand` is
`Entrypoint`'s sibling for this shape: a name, a `Vec<String>`, and an exit
code. The mesh's `PeersCli` is a `clap::Parser` with `no_binary_name`, and
clap's own `err.exit()` keeps help at 0 and a bad argument at 2.

The cost is real and small: `wizard --help` shows `peers` with core's
description rather than its subcommand list, and a misspelled subcommand is
caught one frame later, by the plugin's parser, against the right usage line.

**`src/app/transcript.rs` took a `NodeId`.** The peer-attribution machinery —
the marker stamped on every physical line of a watched session — is core, and it
was building that marker from the mesh's `NodeId`. The two things a marker may
be derived from are a short form and a full address, both strings, so the trait
*is* the whole dependency: `PeerAddress` has two methods, core owns it, and
`impl PeerAddress for NodeId` is four lines in `plugins::mesh::node`.

Two strings passed in directly would have been smaller and is the wrong trade —
it lets a caller pass a *label* where an address goes, which is exactly the
confusion `PeerOrigin`'s private fields exist to prevent. The trait keeps
"derived from the key, never from the name" a property of the type instead of
of every call site.

### `graph` depends on `mesh`, and Cargo is where that is written down

`graph = ["mesh"]`. A `MeshGraph` is a `PeerStore` turned into something
drawable, so the explorer cannot exist without the store, and the feature edge
is the honest place to say so — the alternative is a comment and a build that
fails at link time for somebody who reads neither. It is the first
plugin-to-plugin dependency in the tree, and it makes `without` in
`contrib/check-tool-plugins.sh` insufficient by itself: dropping `mesh` from the
default list leaves `graph` to turn it back on, hence `without_many`.

### What stayed in core

**`[mesh]` in `config.toml`.** Same argument as `[web]`: `listen`, `mdns`,
`listen_addr` and `[mesh.routes]` are promises about what this *process* does on
the network, and a build without the plugin still parses and still ignores them,
rather than failing to load a config file that was valid yesterday.

**`crate::text::is_invisible`.** It came out of the mesh in the `tool-web`
change and stays out. What is invisible is a property of Unicode, not of the
mesh, and three callers need the same answer.

**`AgentEvent::is_request`.** The exhaustive match deciding what may cross a
socket sits next to the variants it matches on, which is the only place it can
be kept honest. `PeerTurn::sanitize` consults it; it does not own it.

### Two crates left the default build with it

`quinn` and `mdns-sd` are `optional = true` and pulled by `dep:` from the `mesh`
feature, so a build without it links neither. `rustls` went with them: the mesh
was the only direct caller, and reqwest brings its own copy either way, so what
is removed there is the edge rather than the crate.

This is the first plugin whose removal measurably shrinks the binary, which is
the whole argument for the `pi` profile that `docs/plugins.md` has been
describing since before any of this was built.

### Proving it

`contrib/check-tool-plugins.sh` grew two legs: `mesh` (and therefore `graph`)
left out headless, and left out **with the window present** — the combination
that catches a GUI reaching for peers outside the `graph` gate, which neither
`--no-default-features` nor a default build can see. Six legs now.
`plugins::the_meshs_two_seams_are_present_exactly_when_its_plugin_is` is the
in-tree half: both registrations, both directions, plus a sweep asserting the
mesh registers no tool and no command — because a `mesh_*` tool would be a model
deciding who watches this session, and that is a trust decision and therefore a
person's. `tests/cli.rs` runs the real binary against both sides of the flag.

### Still open, specific to this

- **A live session still does not re-read `peers.json`.** `wizard peers trust
  <peer> known` in a second terminal binds every process started afterwards and
  not the one already running. Named in `tee.rs` since the tee landed and not
  changed by the move.
- **`wizard --help` describes `peers` in one paragraph** rather than listing its
  eight subcommands, which is the price of the argument list crossing unparsed.
  Building the top-level listing from what plugins registered is the fix and is
  the same change the onboarding menu and the provider picker have been waiting
  for.
- **The other three surfaces are still core.** `wizard acp`, `wizard gateway`
  and `mcp serve` are each one `Entrypoint` registration, and none of them is
  behind a feature yet.
