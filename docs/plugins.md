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
`src/llm/{mod,provider,compat,registry}.rs` (the `LlmProvider` trait, the shared
streaming machinery and the registry that resolves a `kind`, not the providers),
`src/ui/`, `src/app/`, `src/skin/`,
`src/event.rs`, `src/dispatch.rs`, `src/tools/{mod,registry}.rs`,
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
| `ctx:command(spec)` | register a slash command |
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

**One file still names the providers that are not plugins yet.**
`src/llm/builtin.rs` is the provider half of the `src/plugins/mod.rs` this
document describes. Eight of the nine shipped providers are still in it. The
ninth went through the door; see below.

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
default. It registers through `Ctx::provider` at kernel boot and `builtin.rs`
does not name it. `kind = "anthropic"` in an existing `config.toml` resolves to
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
`a_plugin_cannot_shadow_a_built_in_provider_kind` depend on test ordering. The
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

### Still open

- **The onboarding menu and the TUI provider picker are still hand-written.**
  On a build without `provider-anthropic` they still offer Anthropic, and
  picking it produces a config that fails at `build()` with a clear message
  rather than an entry that was never offered. Building both from
  `registry::kinds()` is the fix and is the same change `src/llm/registry.rs`
  has been asking for.
- **The host bridge is still `UnwiredHost`.** A Lua plugin that calls
  `wizard.http` or `wizard.model` gets an error naming the reason. Attaching a
  real bridge is its own piece of work.
- **Eight providers to go**, plus everything that is not a provider.
