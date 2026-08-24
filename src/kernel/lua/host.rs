//! The tables a Lua plugin sees: `ctx` and `wizard`.
//!
//! Two halves. `ctx` is the plugin API from `docs/plugins.md`, translated call
//! for call — `ctx:tool`, `ctx:command`, `ctx:on`, `ctx:emit`, `ctx:provide`,
//! `ctx:inject`, `ctx:plugin`, `ctx:effect`, `ctx:config` — so a plugin written
//! in Lua and the same plugin written in Rust register through the same shapes.
//! `wizard` is the host surface, and every table on it that costs money, leaks
//! data or touches the machine is installed only when the manifest declared the
//! capability that names it.
//!
//! # Gating by absence, not by refusal
//!
//! A plugin without `network` does not get a `wizard.http` that errors; it gets
//! no `wizard.http` at all. `wizard.http == nil` is a thing a plugin can branch
//! on, which is the same composability rule `ctx:inject` follows: ask, and
//! degrade when the answer is nothing. A table that exists and refuses would
//! make "can I fetch?" a question you can only answer by trying.
//!
//! # The two capabilities that live inside the standard library
//!
//! `filesystem` and `process` name functions in `os` and `io`, and
//! [`Stdlib`] is binary: [`Stdlib::Sandboxed`] leaves both tables out of the
//! state entirely, so declaring either capability has to open both. That is
//! coarser than the spec's table, and [`narrow_stdlib`] is what closes the gap
//! — it blanks the names belonging to whichever of the two was *not* declared.
//! The result is that a plugin with `filesystem` alone can `io.open` and cannot
//! `os.execute`, which is what the table promises.
//!
//! The confinement of `wizard.read_file` / `wizard.write_file` follows
//! `filesystem` specifically and not the standard-library profile, which is why
//! [`build`] passes its own answer to
//! [`install_wizard_lib`](crate::tools::lua::install_wizard_lib) rather than
//! the one [`CapabilitySet::stdlib`] gave: a `process`-only plugin runs under
//! `Stdlib::Full` and still has its host file helpers pinned to the project
//! directory.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use mlua::{Lua, LuaOptions, StdLib, Table, Value as LuaValue};
use serde_json::Value;

use crate::tools::lua::{
    Stdlib, blank_globals, install_wizard_lib, json_to_lua, lua_to_json, sandboxed_libs,
};
use crate::tools::{
    MAX_OUTPUT_BYTES, Tool, ToolAccess, ToolContext, ToolError, ToolKind, ToolOutput,
    truncate_output,
};

use super::super::Ctx;
use super::super::bus::{Event, EventHandler, HandlerFuture, Verdict};
use super::super::ctx::{Command, CommandFuture, CommandHandler};
use super::super::manifest::{Capability, CapabilitySet, PluginSource};
use super::super::services::Service;
use super::{Bound, FnId, LuaShutdown, VmHandle, bind};

/// Longest a plugin may park in `wizard.sleep`.
///
/// A sleeping plugin costs no CPU, so the hook will never stop it — but it does
/// hold one of the VM's in-flight slots and one of the channel's, and a plugin
/// that slept for a day would look exactly like one that had wedged.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// Everything one plugin's VM owns.
pub(crate) struct VmState {
    pub(crate) lua: Lua,
    pub(crate) ctx_table: Table,
    /// Lua functions the host holds by number: tool bodies, command bodies,
    /// event handlers, teardowns.
    pub(crate) functions: Arc<Mutex<HashMap<FnId, mlua::Function>>>,
    /// Teardowns, in registration order. Run in reverse at shutdown.
    effects: Arc<Mutex<Vec<(String, FnId)>>>,
    pub(crate) bound: Option<Bound>,
}

/// Shared state the host closures write into.
#[derive(Clone)]
struct Registry {
    functions: Arc<Mutex<HashMap<FnId, mlua::Function>>>,
    effects: Arc<Mutex<Vec<(String, FnId)>>>,
    next: Arc<AtomicU64>,
}

impl Registry {
    fn new() -> Self {
        Self {
            functions: Arc::new(Mutex::new(HashMap::new())),
            effects: Arc::new(Mutex::new(Vec::new())),
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    fn hold(&self, function: mlua::Function) -> FnId {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.functions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, function);
        id
    }
}

/// Build a plugin's VM: sandbox, host table, `ctx` table.
///
/// The order matters and matches `run_lua_blocking` in `src/tools/lua.rs`: the
/// JIT goes off (inside [`bind`]) before anything else, because a hook on a
/// compiled state is a hard crash rather than a slow path and because
/// `disable_jit` is what takes the `jit` switch away from the chunk.
pub(crate) fn build(ctx: &Ctx, handle: &VmHandle, source: PluginSource) -> anyhow::Result<VmState> {
    let caps = ctx.capabilities().clone();
    let stdlib = caps.stdlib();
    let libs = match stdlib {
        Stdlib::Full => StdLib::ALL_SAFE,
        Stdlib::Sandboxed => sandboxed_libs(),
    };
    let lua = Lua::new_with(libs, LuaOptions::default())
        .map_err(|err| anyhow::anyhow!("creating the LuaJIT state: {err}"))?;

    let bound = bind(&lua, source, ctx.kernel().call_budget())
        .map_err(|err| anyhow::anyhow!("installing the plugin's bounds: {err}"))?;

    match stdlib {
        Stdlib::Sandboxed => blank_globals(&lua)
            .map_err(|err| anyhow::anyhow!("sandboxing the LuaJIT state: {err}"))?,
        Stdlib::Full => narrow_stdlib(&lua, &caps)
            .map_err(|err| anyhow::anyhow!("narrowing the standard library: {err}"))?,
    }

    install_print(&lua, ctx.name()).map_err(|err| anyhow::anyhow!("installing print(): {err}"))?;

    // Confinement follows `filesystem`, not the library profile. See the
    // module docs.
    let fs_profile = if caps.contains(Capability::Filesystem) {
        Stdlib::Full
    } else {
        Stdlib::Sandboxed
    };
    install_wizard_lib(&lua, ctx.kernel().project_root(), fs_profile)
        .map_err(|err| anyhow::anyhow!("installing wizard.*: {err}"))?;
    install_host(&lua, ctx, &caps)
        .map_err(|err| anyhow::anyhow!("installing the capability tables: {err}"))?;

    let registry = Registry::new();
    let ctx_table = build_ctx_table(&lua, ctx, handle, &registry)
        .map_err(|err| anyhow::anyhow!("building the ctx table: {err}"))?;

    Ok(VmState {
        lua,
        ctx_table,
        functions: registry.functions,
        effects: registry.effects,
        bound,
    })
}

/// `print` for a plugin, which goes to the log rather than to a buffer.
///
/// The scripted-tool `print` accumulates into a string the host reads when the
/// chunk ends. A plugin's VM never ends, so the same buffer would be a leak
/// with a `print` in front of it. Plugins that want to say something to the
/// user ask for `ui` and call `wizard.ui.notify`; `print` is for debugging and
/// lands where debugging output belongs.
fn install_print(lua: &Lua, plugin: &str) -> mlua::Result<()> {
    let plugin = plugin.to_string();
    let print = lua.create_function(move |lua, values: mlua::MultiValue| {
        let mut line = String::new();
        for (i, value) in values.into_iter().enumerate() {
            if i > 0 {
                line.push('\t');
            }
            line.push_str(&crate::tools::lua::lua_value_to_string(lua, value)?);
        }
        tracing::debug!(plugin = %plugin, "{line}");
        Ok(())
    })?;
    lua.globals().set("print", print)
}

/// Blank the standard-library names belonging to a capability the plugin did
/// not declare.
///
/// Only reached under [`Stdlib::Full`], where `os` and `io` exist; the
/// `if let Ok` guards make it a no-op anywhere else. Setting an absent field to
/// `nil` is already a no-op in Lua, so over-listing costs nothing and
/// under-listing is the only mistake available.
///
/// `package` is deliberately not touched. `require` and `package.loadlib` are
/// reachable by every `Stdlib::Full` script in the tree today and always have
/// been; removing them here would be a new confinement wearing a capability's
/// name, and the honest place to argue about it is [`sandboxed_libs`].
pub(crate) fn narrow_stdlib(lua: &Lua, caps: &CapabilitySet) -> mlua::Result<()> {
    let globals = lua.globals();
    let os: Option<Table> = globals.get("os").ok();
    let io: Option<Table> = globals.get("io").ok();

    if !caps.contains(Capability::Process) {
        if let Some(os) = &os {
            for name in ["execute", "getenv", "exit"] {
                os.set(name, LuaValue::Nil)?;
            }
        }
        if let Some(io) = &io {
            io.set("popen", LuaValue::Nil)?;
        }
    }

    if !caps.contains(Capability::Filesystem) {
        if let Some(os) = &os {
            for name in ["remove", "rename", "tmpname"] {
                os.set(name, LuaValue::Nil)?;
            }
        }
        if let Some(io) = &io {
            for name in [
                "open", "lines", "input", "output", "read", "write", "close", "popen", "tmpfile",
            ] {
                io.set(name, LuaValue::Nil)?;
            }
        }
        // `dofile` and `loadfile` open a path and run it, which is `io.open`
        // with an extra step.
        for name in ["dofile", "loadfile"] {
            globals.set(name, LuaValue::Nil)?;
        }
    }

    Ok(())
}

/// Add the capability-gated tables to the `wizard` table
/// [`install_wizard_lib`] already put in place.
fn install_host(lua: &Lua, ctx: &Ctx, caps: &CapabilitySet) -> mlua::Result<()> {
    let wizard: Table = lua.globals().get("wizard")?;
    wizard.set("plugin", ctx.name())?;

    // `wizard.fs` is the spec's spelling of the two helpers that are already
    // there under their older names. Same functions, so the confinement
    // decision is made in exactly one place.
    let fs = lua.create_table()?;
    fs.set("read", wizard.get::<mlua::Function>("read_file")?)?;
    fs.set("write", wizard.get::<mlua::Function>("write_file")?)?;
    wizard.set("fs", fs)?;

    let sleep = lua.create_async_function(|_, millis: u64| async move {
        tokio::time::sleep(Duration::from_millis(millis).min(MAX_SLEEP)).await;
        Ok(())
    })?;
    wizard.set("sleep", sleep)?;

    let plugin = ctx.name().to_string();
    let log = lua.create_function(move |_, message: String| {
        tracing::info!(plugin = %plugin, "{message}");
        Ok(())
    })?;
    wizard.set("log", log)?;

    if caps.contains(Capability::Network) {
        let http = lua.create_table()?;
        for (name, method) in [("get", "GET"), ("post", "POST"), ("put", "PUT")] {
            let host = ctx.host();
            let function =
                lua.create_async_function(move |_, (url, body): (String, Option<String>)| {
                    let host = Arc::clone(&host);
                    async move { host.http(method, &url, body).await.map_err(external) }
                })?;
            http.set(name, function)?;
        }
        wizard.set("http", http)?;
    }

    if caps.contains(Capability::Model) {
        let model = lua.create_table()?;
        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let complete = lua.create_async_function(move |_, prompt: String| {
            let host = Arc::clone(&host);
            let plugin = plugin.clone();
            async move { host.model(&plugin, &prompt).await.map_err(external) }
        })?;
        model.set("complete", complete)?;
        wizard.set("model", model)?;
    }

    if caps.contains(Capability::Ui) {
        let ui = lua.create_table()?;
        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let notify = lua.create_async_function(move |_, text: String| {
            let host = Arc::clone(&host);
            let plugin = plugin.clone();
            async move { host.notify(&plugin, &text).await.map_err(external) }
        })?;
        ui.set("notify", notify)?;
        wizard.set("ui", ui)?;
    }

    if caps.contains(Capability::Agent) {
        let agent = lua.create_table()?;
        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let spawn = lua.create_async_function(move |_, task: String| {
            let host = Arc::clone(&host);
            let plugin = plugin.clone();
            async move { host.spawn_agent(&plugin, &task).await.map_err(external) }
        })?;
        agent.set("spawn", spawn)?;
        wizard.set("agent", agent)?;
    }

    if caps.contains(Capability::Process) {
        let process = lua.create_table()?;
        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let run = lua.create_async_function(move |_, command: String| {
            let host = Arc::clone(&host);
            let plugin = plugin.clone();
            async move { host.run(&plugin, &command).await.map_err(external) }
        })?;
        process.set("run", run)?;
        wizard.set("process", process)?;
    }

    Ok(())
}

fn external(err: anyhow::Error) -> mlua::Error {
    mlua::Error::external(Box::<dyn std::error::Error + Send + Sync>::from(
        err.to_string(),
    ))
}

/// Build the `ctx` table the plugin's `apply` is handed.
///
/// Every method takes `self` as its first argument so `ctx:tool{...}` works,
/// and every one of them ignores it: the table carries no state, the closures
/// carry the [`Ctx`]. That is what stops a plugin reaching another plugin's
/// context by copying the table.
fn build_ctx_table(
    lua: &Lua,
    ctx: &Ctx,
    handle: &VmHandle,
    registry: &Registry,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;

    table.set("tool", tool_fn(lua, ctx, handle, registry)?)?;
    table.set("command", command_fn(lua, ctx, handle, registry)?)?;
    table.set("provider", provider_fn(lua, ctx)?)?;
    table.set("on", on_fn(lua, ctx, handle, registry)?)?;
    table.set("emit", emit_fn(lua, ctx)?)?;
    table.set("provide", provide_fn(lua, ctx)?)?;
    table.set("inject", inject_fn(lua, ctx)?)?;
    table.set("plugin", plugin_fn(lua, ctx)?)?;
    table.set("effect", effect_fn(lua, registry)?)?;
    table.set("config", config_fn(lua, ctx)?)?;

    let name = ctx.name().to_string();
    table.set(
        "name",
        lua.create_function(move |_, _: Table| Ok(name.clone()))?,
    )?;

    Ok(table)
}

fn tool_fn(
    lua: &Lua,
    ctx: &Ctx,
    handle: &VmHandle,
    registry: &Registry,
) -> mlua::Result<mlua::Function> {
    let ctx = ctx.clone();
    let handle = handle.clone();
    let registry = registry.clone();
    lua.create_function(move |lua, (_this, spec): (Table, Table)| {
        let name: String = spec.get("name")?;
        let description: String = spec.get("description").unwrap_or_default();
        let parameters = match spec.get::<LuaValue>("parameters") {
            Ok(LuaValue::Nil) | Err(_) => empty_schema(),
            Ok(value) => lua_to_json(lua, value)?,
        };
        let access = match spec.get::<String>("access").as_deref() {
            Ok("read_only") => ToolAccess::ReadOnly,
            Ok("edit") => ToolAccess::Edit,
            // Anything else, including nothing, is the conservative answer.
            // `ToolAccess` drives the plan-mode read-only gate, so guessing
            // wrong in the other direction lets a plugin write in plan mode.
            _ => ToolAccess::Execute,
        };
        let execute: mlua::Function = spec.get("execute").map_err(|_| {
            mlua::Error::external(format!("ctx:tool{{name='{name}'}} has no execute function"))
        })?;
        let func = registry.hold(execute);

        ctx.tool(Arc::new(LuaTool {
            name,
            description,
            parameters,
            access,
            handle: handle.clone(),
            func,
        }))
        .map_err(mlua::Error::external)
    })
}

fn command_fn(
    lua: &Lua,
    ctx: &Ctx,
    handle: &VmHandle,
    registry: &Registry,
) -> mlua::Result<mlua::Function> {
    let ctx = ctx.clone();
    let handle = handle.clone();
    let registry = registry.clone();
    lua.create_function(move |_, (_this, spec): (Table, Table)| {
        let name: String = spec.get("name")?;
        let description: String = spec.get("description").unwrap_or_default();
        let run: mlua::Function = spec.get("run").map_err(|_| {
            mlua::Error::external(format!("ctx:command{{name='{name}'}} has no run function"))
        })?;
        let func = registry.hold(run);
        ctx.command(Command::new(
            name,
            description,
            Arc::new(LuaCommand {
                handle: handle.clone(),
                func,
            }),
        ))
        .map_err(mlua::Error::external)
    })
}

/// `ctx:provider` exists so the shape is the same in both languages, and
/// refuses so the refusal is at least honest.
///
/// An `LlmProvider` is TLS, SSE framing and nine providers' worth of wire
/// quirks — "bytes and syscalls" in the spec's split, which is the half that
/// stays in Rust. A Lua implementation would have to reach all of it through a
/// host API wide enough to be Rust with a slower calling convention. A plugin
/// that wants to add a provider adds a Rust one.
fn provider_fn(lua: &Lua, ctx: &Ctx) -> mlua::Result<mlua::Function> {
    let plugin = ctx.name().to_string();
    lua.create_function(move |_, (_this, _spec): (Table, LuaValue)| {
        Err::<(), _>(mlua::Error::external(format!(
            "plugin '{plugin}': a provider cannot be registered from Lua. \
             Providers are transport code and stay in Rust; see docs/plugins.md."
        )))
    })
}

fn on_fn(
    lua: &Lua,
    ctx: &Ctx,
    handle: &VmHandle,
    registry: &Registry,
) -> mlua::Result<mlua::Function> {
    let ctx = ctx.clone();
    let handle = handle.clone();
    let registry = registry.clone();
    lua.create_function(
        move |_, (_this, event, callback, priority): (Table, String, mlua::Function, Option<i32>)| {
            let parsed = Event::parse(&event).ok_or_else(|| {
                mlua::Error::external(format!(
                    "'{event}' is not an event; a subscription to a name nothing emits \
                     would silently never fire"
                ))
            })?;
            let func = registry.hold(callback);
            ctx.on(
                parsed,
                priority.unwrap_or(super::super::bus::DEFAULT_PRIORITY),
                Arc::new(LuaEventHandler {
                    handle: handle.clone(),
                    func,
                }),
            );
            Ok(())
        },
    )
}

fn emit_fn(lua: &Lua, ctx: &Ctx) -> mlua::Result<mlua::Function> {
    let ctx = ctx.clone();
    lua.create_async_function(
        move |lua, (_this, event, payload): (Table, String, LuaValue)| {
            let ctx = ctx.clone();
            let parsed = Event::parse(&event);
            let payload = lua_to_json(&lua, payload);
            async move {
                let event = parsed
                    .ok_or_else(|| mlua::Error::external(format!("'{event}' is not an event")))?;
                let dispatch = ctx.emit(event, payload?).await;
                let result = lua.create_table()?;
                result.set("payload", json_to_lua(&lua, &dispatch.payload)?)?;
                result.set("vetoed", dispatch.is_vetoed())?;
                match dispatch.veto {
                    Some(veto) => {
                        result.set("veto", veto.reason)?;
                        result.set("veto_by", veto.plugin)?;
                    }
                    None => result.set("veto", LuaValue::Nil)?,
                }
                result.set("ran", dispatch.ran)?;
                result.set("failures", dispatch.failures.len())?;
                Ok(result)
            }
        },
    )
}

fn provide_fn(lua: &Lua, ctx: &Ctx) -> mlua::Result<mlua::Function> {
    let ctx = ctx.clone();
    lua.create_function(
        move |lua, (_this, name, value): (Table, String, LuaValue)| {
            ctx.provide(name, Service::data(lua_to_json(lua, value)?));
            Ok(())
        },
    )
}

fn inject_fn(lua: &Lua, ctx: &Ctx) -> mlua::Result<mlua::Function> {
    let ctx = ctx.clone();
    lua.create_function(move |lua, (_this, name): (Table, String)| {
        // A native service is `nil` here, which is the same `nil` an absent one
        // gives — see the `Ctx` module docs. Lua cannot call a Rust trait
        // object, and pretending otherwise would only move the failure later.
        match ctx.inject(&name).as_ref().and_then(Service::as_data) {
            Some(value) => json_to_lua(lua, value),
            None => Ok(LuaValue::Nil),
        }
    })
}

/// `ctx:plugin(name, config)` — load a child Lua plugin from the plugin root.
///
/// Only Lua children: a Lua plugin has no way to name a Rust one, since Rust
/// plugins are values behind a cargo feature rather than directories. A Rust
/// parent loading a Lua child goes through the kernel directly.
fn plugin_fn(lua: &Lua, ctx: &Ctx) -> mlua::Result<mlua::Function> {
    let ctx = ctx.clone();
    lua.create_async_function(
        move |lua, (_this, name, config): (Table, String, LuaValue)| {
            let ctx = ctx.clone();
            let config = match config {
                LuaValue::Nil => Ok(None),
                other => lua_to_json(&lua, other).map(Some),
            };
            async move {
                let config = config?;
                if name.contains(['/', '\\']) || name.contains("..") {
                    return Err(mlua::Error::external(format!(
                        "'{name}' is not a plugin name; ctx:plugin takes a name under the \
                     plugin directory, not a path"
                    )));
                }
                let dir = ctx.kernel().plugin_root().join(&name);
                let id = super::load_dir(
                    ctx.kernel(),
                    &dir,
                    PluginSource::Registry,
                    Some(ctx.id().clone()),
                    config,
                )
                .await
                .map_err(mlua::Error::external)?;
                ctx.record_child(id.clone());
                Ok(id.to_string())
            }
        },
    )
}

/// `ctx:effect(dispose)` — a teardown that lives in the VM.
///
/// A Lua closure cannot become a Rust `FnOnce`, so it is held here and run by
/// [`run_effects`] during shutdown. The observable ordering matches the Rust
/// side: after every registry entry is gone, newest first.
fn effect_fn(lua: &Lua, registry: &Registry) -> mlua::Result<mlua::Function> {
    let registry = registry.clone();
    lua.create_function(
        move |_, (_this, dispose, label): (Table, mlua::Function, Option<String>)| {
            let func = registry.hold(dispose);
            let label = label.unwrap_or_else(|| format!("lua effect #{func}"));
            registry
                .effects
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((label, func));
            Ok(())
        },
    )
}

fn config_fn(lua: &Lua, ctx: &Ctx) -> mlua::Result<mlua::Function> {
    let config = ctx.config().clone();
    lua.create_function(move |lua, _: Table| json_to_lua(lua, &config))
}

/// Run a plugin's Lua teardowns, newest first.
///
/// An error is recorded and the next one runs, matching
/// [`crate::kernel::lifecycle`]: a plugin gets to leak its own socket, it does
/// not get to leave the rest of the unload undone.
pub(crate) async fn run_effects(state: &VmState) -> LuaShutdown {
    let effects = {
        let mut held = state.effects.lock().unwrap_or_else(PoisonError::into_inner);
        let mut taken = std::mem::take(&mut *held);
        taken.reverse();
        taken
    };

    let mut shutdown = LuaShutdown::default();
    for (label, id) in effects {
        let function = state
            .functions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned();
        let Some(function) = function else {
            shutdown
                .failures
                .push(format!("{label}: gone before it ran"));
            continue;
        };
        match function.call_async::<()>(()).await {
            Ok(()) => shutdown.effects += 1,
            Err(err) => {
                tracing::error!(effect = %label, error = %err, "a Lua teardown failed");
                shutdown.failures.push(format!("{label}: {err}"));
            }
        }
    }
    shutdown
}

/// A JSON Schema for a tool that declared no parameters.
///
/// An empty object rather than `null`: every provider's tool-calling wire
/// format wants a schema, and the one that means "no arguments" is an object
/// with no properties.
fn empty_schema() -> Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

/// A tool whose body is a Lua function in a plugin's VM.
struct LuaTool {
    name: String,
    description: String,
    parameters: Value,
    access: ToolAccess,
    handle: VmHandle,
    func: FnId,
}

#[async_trait]
impl Tool for LuaTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn access(&self) -> ToolAccess {
        self.access
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Scripted
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let value = self
            .handle
            .call(self.func, vec![args])
            .await
            .map_err(|source| ToolError::Execution {
                tool: self.name.clone(),
                source,
            })?;

        let content = match value {
            Value::Null => String::new(),
            Value::String(text) => text,
            other => other.to_string(),
        };
        // The `error:` convention scripted tools already use, so a plugin tool
        // and a scripted tool report a soft failure the same way.
        let trimmed = content.trim_start();
        let is_error = trimmed.starts_with("error:") || trimmed.starts_with("Error:");
        let content = truncate_output(content, MAX_OUTPUT_BYTES);
        Ok(if is_error {
            ToolOutput::error(content)
        } else {
            ToolOutput::ok(content)
        })
    }
}

/// A slash command whose body is a Lua function.
struct LuaCommand {
    handle: VmHandle,
    func: FnId,
}

impl CommandHandler for LuaCommand {
    fn run(&self, args: String) -> CommandFuture {
        let handle = self.handle.clone();
        let func = self.func;
        Box::pin(async move {
            let value = handle.call(func, vec![Value::String(args)]).await?;
            Ok(match value {
                Value::Null => String::new(),
                Value::String(text) => text,
                other => other.to_string(),
            })
        })
    }
}

/// An event handler whose body is a Lua function.
///
/// The verdict protocol, which is the one thing a Lua plugin author has to
/// learn that a Rust one does not:
///
/// - return nothing (or `nil`) to observe;
/// - return `{ payload = ... }` to rewrite;
/// - return `{ veto = "reason" }` to refuse.
///
/// A table with neither key is treated as an observation rather than as a
/// rewrite to that table, because `return {}` from a handler that meant nothing
/// by it should not blank the payload for everything downstream.
struct LuaEventHandler {
    handle: VmHandle,
    func: FnId,
}

impl EventHandler for LuaEventHandler {
    fn handle(&self, event: Event, payload: Value) -> HandlerFuture {
        let handle = self.handle.clone();
        let func = self.func;
        Box::pin(async move {
            let answer = handle
                .call(func, vec![Value::String(event.name().to_string()), payload])
                .await?;
            Ok(verdict_of(answer))
        })
    }
}

fn verdict_of(answer: Value) -> Verdict {
    let Value::Object(map) = &answer else {
        return Verdict::Continue;
    };
    if let Some(Value::String(reason)) = map.get("veto") {
        return Verdict::Veto(reason.clone());
    }
    match map.get("payload") {
        Some(payload) => Verdict::Rewrite(payload.clone()),
        None => Verdict::Continue,
    }
}
