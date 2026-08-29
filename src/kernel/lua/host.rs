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
use super::super::manifest::{Capability, CapabilitySet, PluginSource};
use super::super::services::Service;
use super::{Bound, FnId, VmHandle, VmShutdown, bind};
use crate::commands::surface::Surface;
use crate::commands::{CommandFuture, CommandHandler, PluginCommand};

/// Longest a plugin may park in `wizard.sleep`.
///
/// A sleeping plugin costs no CPU, so the hook will never stop it — but it does
/// hold one of the VM's in-flight slots and one of the channel's, and a plugin
/// that slept for a day would look exactly like one that had wedged.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// Budget a `wizard.process.exec` gets when it names none.
///
/// The same figure `[shell]`'s foreground default is, which is also the
/// ceiling the host clamps to, so a plugin that says nothing gets exactly what
/// the shell tool's callers get.
const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

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

    // `package` goes for everyone, whatever they declared.
    //
    // It is not gated because there is no grant it could hang off: none of the
    // six capabilities means "load and execute arbitrary native code", and
    // `package.loadlib` maps a `.so` into this process and calls it, which is
    // `ffi` under another name. `require` reaches the same loader through
    // `package.cpath`.
    //
    // Leaving it alone is what `Stdlib::Full` has always done, and for a
    // locally authored scripted tool that is right: its author is the user, and
    // `src/tools/lua.rs` is unaffected by this function. A plugin is different.
    // It can arrive from the registry, its capabilities are what the install
    // prompt shows the user, and `filesystem` -- the mildest thing a
    // text-munging plugin asks for -- would otherwise carry native execution in
    // with it, making that prompt a description of nothing.
    globals.set("require", LuaValue::Nil)?;
    globals.set("package", LuaValue::Nil)?;

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

    install_output_budget(lua, &wizard)?;

    if caps.contains(Capability::Filesystem) {
        install_paths(lua, &wizard, ctx)?;
    }

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

        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let exec = lua.create_async_function(move |lua, spec: Table| {
            let host = Arc::clone(&host);
            let plugin = plugin.clone();
            let request = exec_request(&spec);
            async move {
                let outcome = host.exec(&plugin, request?).await.map_err(external)?;
                let table = lua.create_table()?;
                table.set("stdout", outcome.stdout)?;
                table.set("stderr", outcome.stderr)?;
                // Nil rather than a sentinel for both of these. `code = -1`
                // for "signalled" is a number a plugin will compare against
                // zero and get wrong; `nil` is a value Lua's own `if` already
                // reads as "no answer".
                match outcome.code {
                    Some(code) => table.set("code", code)?,
                    None => table.set("code", LuaValue::Nil)?,
                }
                match outcome.timed_out {
                    Some(secs) => table.set("timed_out", secs)?,
                    None => table.set("timed_out", LuaValue::Nil)?,
                }
                Ok(table)
            }
        })?;
        process.set("exec", exec)?;
        wizard.set("process", process)?;
    }

    Ok(())
}

/// One `wizard.process.exec{ argv = {...}, cwd = ..., timeout_ms = ... }`.
///
/// Every failure here is a refusal rather than a default, because the two
/// mistakes available are both silent: an `argv` given as a string runs a
/// program whose name contains spaces and fails as "no such file", and an
/// empty `argv` would otherwise become a request the host has to reject with
/// less context than this has.
fn exec_request(spec: &Table) -> mlua::Result<crate::kernel::ExecRequest> {
    let argv: Table = spec.get("argv").map_err(|_| {
        mlua::Error::external(
            "wizard.process.exec needs argv = { 'program', 'arg', ... }; a shell line goes to \
             wizard.process.run",
        )
    })?;
    let argv: Vec<String> = argv
        .sequence_values::<String>()
        .collect::<mlua::Result<_>>()?;
    if argv.is_empty() {
        return Err(mlua::Error::external(
            "wizard.process.exec was given an empty argv",
        ));
    }
    let cwd: Option<String> = spec.get("cwd").ok().filter(|s: &String| !s.is_empty());
    let millis: Option<u64> = spec.get("timeout_ms").ok();
    Ok(crate::kernel::ExecRequest {
        argv,
        cwd: cwd.map(std::path::PathBuf::from),
        timeout: millis.map_or(DEFAULT_EXEC_TIMEOUT, Duration::from_millis),
    })
}

/// `wizard.paths`: the directories Wizard keeps its own state in, as strings.
///
/// Gated on [`Capability::Filesystem`], which is the grant that makes a path
/// useful. Without it `wizard.fs` is pinned to the project root and `io` is
/// gone, so the table would be a list of places the plugin cannot go — and a
/// path is still a fact about the machine, so handing one to a plugin that
/// declared nothing is a leak with no upside.
///
/// It exists because the alternative is worse in a specific way. A ported
/// subsystem that keeps state under `~/.wizard` — the evolution log, the
/// source checkout, the skills tree — would otherwise rebuild those paths from
/// `os.getenv("HOME")`, and that answer is *wrong under `cargo test`*:
/// [`Config::wizard_dir`] redirects to a temp directory there, deliberately,
/// so the suite cannot overwrite a developer's real config. A plugin deriving
/// the path itself would sail past the redirect and write to the real one. So
/// every entry here is [`Config`]'s own accessor, evaluated once, and the
/// redirect holds for a plugin exactly as it holds for Rust.
///
/// Named entries rather than a `home` a plugin joins onto, for the reason
/// `docs/plugins.md` gives about `memory`: the moment a plugin writes
/// `home .. "/src"` there are two definitions of where the checkout is. `home`
/// is here anyway, because a plugin that needs a path this table does not
/// carry has to start somewhere and a missing key is a worse failure than a
/// join somebody can see.
///
/// A `Config` accessor that fails (no home directory) leaves its key absent
/// rather than erroring the whole VM: a plugin that needs it gets a `nil` it
/// can report on, and one that does not is unaffected.
fn install_paths(lua: &Lua, wizard: &Table, ctx: &Ctx) -> mlua::Result<()> {
    use crate::config::Config;

    let paths = lua.create_table()?;
    paths.set("project", ctx.kernel().project_root().to_string_lossy())?;
    for (key, path) in [
        ("home", Config::wizard_dir()),
        ("source", Config::source_dir()),
        ("evolution_log", Config::evolution_log_path()),
    ] {
        if let Ok(path) = path {
            paths.set(key, path.to_string_lossy())?;
        }
    }
    wizard.set("paths", paths)
}

/// `wizard.truncate` and `wizard.limits`: the context-window budgets, and the
/// one function that applies them.
///
/// Ungated. `truncate_output` can spill to the session's scratch file, which
/// is the one thing here that touches a disk, and it is not a way around
/// `filesystem`: the plugin picks neither the path nor the filename, the bytes
/// written are its own return value on its way to the model, and it cannot
/// read any of it back. Gating it would mean a `filesystem` grant on every
/// plugin that wanted its output framed the way a native tool's is.
///
/// It is here at all because a ported tool cannot preserve its behaviour
/// without it. A native tool picks a budget per *answer* — `git_diff` caps a
/// diff at [`MAX_DIFF_BYTES`] and its stderr at [`MAX_ERROR_BYTES`] — and the
/// only cap a Lua tool had was the blanket [`MAX_OUTPUT_BYTES`] that
/// [`LuaTool::execute`] applies on the way out. A plugin that had to invent
/// its own numbers would drift from the native ones the first time either
/// moved, and a plugin that wrote its own `string.sub` would lose the head/tail
/// framing and the spill file with it.
fn install_output_budget(lua: &Lua, wizard: &Table) -> mlua::Result<()> {
    let limits = lua.create_table()?;
    limits.set("output", MAX_OUTPUT_BYTES)?;
    limits.set("diff", crate::tools::MAX_DIFF_BYTES)?;
    limits.set("search", crate::tools::MAX_SEARCH_BYTES)?;
    limits.set("listing", crate::tools::MAX_LISTING_BYTES)?;
    limits.set("error", crate::tools::MAX_ERROR_BYTES)?;
    wizard.set("limits", limits)?;

    let truncate = lua.create_function(|_, (text, max_bytes): (String, Option<usize>)| {
        Ok(truncate_output(
            text,
            max_bytes.unwrap_or(MAX_OUTPUT_BYTES).max(1),
        ))
    })?;
    wizard.set("truncate", truncate)
}

/// Carry a host error into Lua as a plain string.
///
/// Flattened with `{:#}` rather than `to_string()`, which prints only the
/// outermost layer. A host call's *reason* is almost always underneath one:
/// `wizard.process.run` fails as "tool '...' failed" with "interrupted" or
/// "exited 3" beneath it, and a plugin author handed the top half alone has
/// nothing to act on.
fn external(err: anyhow::Error) -> mlua::Error {
    mlua::Error::external(Box::<dyn std::error::Error + Send + Sync>::from(format!(
        "{err:#}"
    )))
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
            Ok(value) => object_schema(lua_to_json(lua, value)?),
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
        let args: String = spec.get("args").unwrap_or_default();
        let run: mlua::Function = spec.get("run").map_err(|_| {
            mlua::Error::external(format!("ctx:command{{name='{name}'}} has no run function"))
        })?;
        let func = registry.hold(run);
        let mut command = PluginCommand::new(
            name.clone(),
            description,
            Arc::new(LuaCommand {
                handle: handle.clone(),
                func,
            }),
        )
        .args(args);
        // `surfaces` absent means every surface, matching the Rust default.
        // Present and empty is a plugin saying "nowhere", which is a plugin bug
        // rather than a shorthand for "everywhere" — reading it as the latter
        // would make a typo silently do the opposite of what it says.
        if let Ok(surfaces) = spec.get::<Table>("surfaces") {
            let mut named = Vec::new();
            for value in surfaces.sequence_values::<String>() {
                named.push(surface_named(&name, &value?)?);
            }
            command = command.only(&named);
        }
        ctx.command(command).map_err(mlua::Error::external)
    })
}

/// One entry of `ctx:command{ surfaces = {...} }`.
///
/// Refuses an unknown name rather than skipping it: the failure mode of
/// skipping one is a command silently missing from exactly the surface the
/// author meant to name.
fn surface_named(command: &str, value: &str) -> mlua::Result<Surface> {
    match value {
        "tui" => Ok(Surface::Tui),
        "gui" => Ok(Surface::Gui),
        "gateway" => Ok(Surface::Gateway),
        other => Err(mlua::Error::external(format!(
            "ctx:command{{name='{command}'}} names surface '{other}' (tui|gui|gateway)"
        ))),
    }
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
pub(crate) async fn run_effects(state: &VmState) -> VmShutdown {
    let effects = {
        let mut held = state.effects.lock().unwrap_or_else(PoisonError::into_inner);
        let mut taken = std::mem::take(&mut *held);
        taken.reverse();
        taken
    };

    let mut shutdown = VmShutdown::default();
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

/// Repair the one place Lua's single table type loses information on the way
/// to a JSON Schema.
///
/// Lua has no empty *object*: `{}` is a table with no entries, and a serializer
/// has to guess. mlua guesses array, which is right far more often than not and
/// is exactly wrong here — `properties = {}` becomes `"properties": []`, and a
/// tool-calling API handed an array where the schema says object either rejects
/// the whole request or, worse, accepts it and tells the model the tool takes
/// unspecified arguments.
///
/// Fixed rather than documented as a plugin author's problem, because the
/// spelling that triggers it is the natural one: a tool with no arguments
/// writes `parameters = { type = "object", properties = {} }`, and the failure
/// arrives from a provider, mid-turn, in somebody else's error message.
fn object_schema(mut schema: Value) -> Value {
    if schema.as_array().is_some_and(Vec::is_empty) {
        return empty_schema();
    }
    if let Some(properties) = schema.get_mut("properties")
        && properties.as_array().is_some_and(Vec::is_empty)
    {
        *properties = Value::Object(serde_json::Map::new());
    }
    schema
}

/// The second argument a tool body is called with: as much of the
/// [`ToolContext`] as can cross into Lua.
///
/// Which is one field, and saying why matters more than the field does.
/// `ToolContext` is sixteen, and thirteen of them are Rust handles a Lua value
/// cannot be: a `Sender<AgentEvent>`, a `CancelHandle`, two task registries,
/// the checkpoint and image stores, the todo list the TUI renders. Those reach
/// a plugin — when they reach one at all — through `wizard.*`, where the host
/// holds the handle and Lua holds only the call. `cwd` is different: it is a
/// path, it is what every path-taking tool resolves against, and a tool that
/// does not get it silently operates on the wrong directory rather than
/// failing.
///
/// A table rather than a bare string so the day a second field is portable it
/// is one line here and no change to any plugin's signature.
fn tool_context(ctx: &ToolContext) -> Value {
    serde_json::json!({ "cwd": ctx.cwd.to_string_lossy() })
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let value = self
            .handle
            .call(self.func, vec![args, tool_context(ctx)])
            .await
            .map_err(|source| ToolError::Execution {
                tool: self.name.clone(),
                source,
            })?;

        let (content, declared) = tool_result(value);
        // The `error:` convention scripted tools already use, so a plugin tool
        // and a scripted tool report a soft failure the same way. It is the
        // *fallback*: a plugin that said which it was gets what it said.
        let is_error = declared.unwrap_or_else(|| {
            let trimmed = content.trim_start();
            trimmed.starts_with("error:") || trimmed.starts_with("Error:")
        });
        let content = truncate_output(content, MAX_OUTPUT_BYTES);
        Ok(if is_error {
            ToolOutput::error(content)
        } else {
            ToolOutput::ok(content)
        })
    }
}

/// What a tool body returned: the text, and whether it said it was a failure.
///
/// Three shapes, and the third is the reason this is a function.
///
/// - a string is the content;
/// - `nil` is empty content;
/// - `{ content = "...", is_error = true }` is a [`ToolOutput`] spelled out.
///
/// Without the third, the only way for a Lua tool to report a failure is the
/// `error:` prefix, which puts a marker word into the text the model reads.
/// That is fine for a scripted tool somebody wrote this morning and wrong for
/// a ported one: `git_status` outside a repository answers with git's own
/// `fatal: not a git repository`, marked as an error and otherwise verbatim,
/// and there is no prefix that could be added to it without changing what the
/// model is told. `error()` from Lua is not the same thing either — that is a
/// tool that *broke*, and this is a tool that worked and has bad news.
///
/// Any other table is still JSON, as it was, so a plugin returning structured
/// data is unaffected unless it happens to have a string `content` key — which
/// is the one collision, and the one spelling this protocol could not avoid
/// without inventing a marker key nobody would guess.
fn tool_result(value: Value) -> (String, Option<bool>) {
    match value {
        Value::Null => (String::new(), None),
        Value::String(text) => (text, None),
        Value::Object(map) if map.get("content").is_some_and(Value::is_string) => {
            let content = map
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            (content, map.get("is_error").and_then(Value::as_bool))
        }
        other => (other.to_string(), None),
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
