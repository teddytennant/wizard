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
`src/llm/{mod,provider,compat}.rs` (the `LlmProvider` trait and the shared
streaming machinery, not the providers), `src/ui/`, `src/app/`, `src/skin/`,
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
| `ctx:provider(spec)` | register an `LlmProvider` |
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
