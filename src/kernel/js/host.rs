//! The globals a JavaScript plugin sees: `ctx` and `wizard`.
//!
//! `src/kernel/lua/host.rs` with a different engine under it, and that is the
//! whole design brief. `ctx.tool`, `ctx.command`, `ctx.provider`, `ctx.on`,
//! `ctx.emit`, `ctx.provide`, `ctx.inject`, `ctx.plugin`, `ctx.effect`,
//! `ctx.config`, `ctx.name` mean what they mean from Lua and from Rust, take
//! the same shapes, and go through the same [`Ctx`]. `wizard.*` reaches the
//! same [`HostBridge`] — there is one `WizardHost` in the process and both
//! backends call it. Nothing here is a second implementation of anything.
//!
//! The one spelling difference is punctuation: Lua's `ctx:tool{...}` passes
//! the table as `self`, JavaScript's `ctx.tool({...})` does not, so every
//! method here takes its spec as the first argument where the Lua side takes
//! it as the second.
//!
//! # Gating by absence, not by refusal
//!
//! A plugin without `network` does not get a `wizard.http` that throws; it
//! gets `wizard.http === undefined`. Same rule as Lua's `nil`, same reason:
//! "can I fetch?" has to be a question a plugin can answer without trying, and
//! a namespace that exists and refuses makes it a question you can only answer
//! by catching.
//!
//! # What is blocked, and why
//!
//! `narrow_stdlib` in the Lua host removes `package` and `require` from every
//! plugin because `package.loadlib` maps a `.so` into this process and calls
//! it — native execution behind a grant that never mentioned it. The
//! JavaScript equivalents, in the order they matter:
//!
//! **The module loader, which is the real one.** `import` and `import()` are
//! how a JavaScript program reaches code outside itself, and QuickJS resolves
//! them through a *loader* the embedder installs. rquickjs ships two:
//! `loader` (filesystem modules) and `dyn-load` (native `.so` modules, which
//! is `package.loadlib` with different spelling). Neither cargo feature is
//! enabled and [`crate::kernel::js::runtime_for`] never calls `set_loader`, so
//! both forms of `import` fail with nothing to resolve against. This is
//! stronger than blanking a global: there is no loader to reach rather than a
//! loader nobody named.
//!
//! **`Atomics` and `SharedArrayBuffer`.** Removed. `Atomics.wait` is the one
//! JavaScript primitive that blocks a thread *without executing bytecode*, and
//! the interrupt handler that bounds a plugin only fires from the interpreter
//! loop — so a plugin that parked there would sit past its deadline with the
//! bound looking on. QuickJS happens to refuse to block on the main agent
//! today ("cannot block in this thread"), which means this is defence in
//! depth rather than a live hole; it is removed anyway, because a capability
//! model that depends on one engine's implementation detail is not a promise.
//! Nothing else about them is useful to a plugin that has no worker threads.
//!
//! **`FinalizationRegistry`.** Removed. It is the only way to get plugin code
//! to run when nobody called it: the callback fires at garbage collection,
//! which is not inside any call, which is exactly where no deadline is armed.
//! `WeakRef` stays — it has no callback and cannot schedule anything.
//!
//! **`eval` and the `Function` constructor stay**, deliberately, and this
//! mirrors Lua rather than diverging from it: `blank_globals` keeps `load`
//! and `loadstring` and only refuses *bytecode* chunks, because compiling text
//! is not an escape — the result runs in the same VM, under the same globals,
//! behind the same bound. QuickJS exposes no bytecode reader to JavaScript at
//! all, so the hole Lua had to patch does not exist here. Removing `eval`
//! would break ordinary libraries and buy nothing.
//!
//! One honest divergence, in the other direction. Lua's sandboxed profile has
//! no `os`, so a plugin that declared nothing cannot read the clock.
//! JavaScript's `Date` is not removable without breaking the language, so
//! every JS plugin can tell the time whatever it declared. `performance` is
//! left for the same reason `Date` is: with `Date.now` present, removing the
//! other timer would be theatre.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rquickjs::prelude::{Async, Opt, Rest};
use rquickjs::{AsyncContext, AsyncRuntime, Ctx as JsCtx, Function, Module, Object, Value as JsValue};
use serde_json::Value;

use crate::commands::surface::Surface;
use crate::commands::{CommandFuture, CommandHandler, PluginCommand};
use crate::tools::lua::{Stdlib, resolve_plugin_path};
use crate::tools::{
    MAX_OUTPUT_BYTES, Tool, ToolAccess, ToolContext, ToolError, ToolKind, ToolOutput,
    truncate_output,
};

use super::super::Ctx;
use super::super::bus::{Event, EventHandler, HandlerFuture, Verdict};
use super::super::manifest::{Capability, CapabilitySet, PluginSource};
use super::super::services::Service;
use super::convert::{js_to_json, json_to_js};
use super::{Bound, FnId, VmHandle, VmShutdown, runtime_for};

/// Where a plugin's registered callbacks live, inside its own VM.
///
/// The Lua backend keeps them Rust-side in a `HashMap<FnId, mlua::Function>`.
/// That is not available here: `rquickjs::Persistent` holds raw pointers into
/// the runtime and is deliberately not `Send`, so a table of them could not
/// live in a struct held across an `await` on a `tokio::spawn`ed task — which
/// is where a plugin's VM lives, in both backends, by design.
///
/// So the table is a JavaScript array on this VM's own globals, and an `FnId`
/// is an index into it. It is non-enumerable and non-writable so an
/// `Object.keys(globalThis)` in a plugin does not trip over it, and a plugin
/// that reaches in and corrupts it anyway breaks only itself: there is one VM
/// per plugin and nothing else is in it.
const REGISTRY: &str = "__wizard_callbacks__";

/// Longest a plugin may park in `wizard.sleep`.
///
/// [`crate::kernel::lua::host`]'s figure and its reason: a sleeping plugin
/// costs no CPU so the interrupt handler will never stop it, but it holds one
/// of the VM's in-flight slots, and a plugin that slept for a day would look
/// exactly like one that had wedged.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// Budget a `wizard.process.exec` gets when it names none.
const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything one plugin's VM owns.
///
/// The runtime is held rather than merely created because dropping it stops
/// the engine, and the context is a child of it: the field order is the drop
/// order, and a context outliving its runtime aborts the process.
pub(crate) struct VmState {
    _runtime: AsyncRuntime,
    context: AsyncContext,
    /// Teardowns, in registration order. Run in reverse at shutdown.
    effects: Arc<Mutex<Vec<(String, FnId)>>>,
    pub(crate) bound: Option<Bound>,
}

/// The counter behind [`REGISTRY`] indices, shared by every closure that holds
/// a callback.
#[derive(Clone)]
struct Registry {
    next: Arc<AtomicU64>,
    effects: Arc<Mutex<Vec<(String, FnId)>>>,
}

impl Registry {
    fn new() -> Self {
        Registry {
            next: Arc::new(AtomicU64::new(0)),
            effects: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Put `function` in this VM's callback array and hand back its index.
    fn hold(&self, ctx: &JsCtx<'_>, function: Function<'_>) -> rquickjs::Result<FnId> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let table: rquickjs::Array = ctx.globals().get(REGISTRY)?;
        table.set(id as usize, function)?;
        Ok(id)
    }
}

/// Build a plugin's VM: runtime, bound, sandbox, host object, `ctx` object.
pub(crate) async fn build(
    ctx: &Ctx,
    handle: &VmHandle,
    source: PluginSource,
) -> anyhow::Result<VmState> {
    let (runtime, context, bound) = runtime_for(source, ctx.kernel().call_budget()).await?;

    let caps = ctx.capabilities().clone();
    let registry = Registry::new();
    let effects = Arc::clone(&registry.effects);

    let ctx_for_build = ctx.clone();
    let handle_for_build = handle.clone();
    let built: anyhow::Result<()> = context
        .async_with(async |js| {
            narrow_globals(&js).map_err(|err| anyhow::anyhow!("narrowing the globals: {err}"))?;
            install_callback_table(&js)
                .map_err(|err| anyhow::anyhow!("installing the callback table: {err}"))?;
            install_console(&js, ctx_for_build.name())
                .map_err(|err| anyhow::anyhow!("installing console: {err}"))?;
            install_wizard(&js, &ctx_for_build, &caps)
                .map_err(|err| anyhow::anyhow!("installing wizard.*: {err}"))?;
            install_ctx(&js, &ctx_for_build, &handle_for_build, &registry)
                .map_err(|err| anyhow::anyhow!("building the ctx object: {err}"))?;
            Ok(())
        })
        .await;
    built?;

    Ok(VmState {
        _runtime: runtime,
        context,
        effects,
        bound,
    })
}

/// Run the plugin module and call its default export's `apply`.
///
/// A *module*, not a script, and that is the TypeScript decision showing up in
/// the loader. `tsc` and `esbuild` both emit `export default {...}`, so a
/// plugin written in TypeScript compiles to something this can load with no
/// wrapper; a script would have had to end in a bare object expression, which
/// is a shape no TypeScript toolchain produces. Modules are also strict mode
/// by default, which is one fewer footgun in a file nobody is going to lint.
pub(crate) async fn apply(
    state: &VmState,
    script: &str,
    module_name: &str,
) -> anyhow::Result<()> {
    let script = script.to_string();
    let module_name = module_name.to_string();
    state
        .context
        .async_with(async |js| {
            let declared = Module::declare(js.clone(), module_name.as_str(), script.as_str())
                .map_err(|err| {
                    anyhow::anyhow!("{module_name} did not parse: {}", thrown(&js, err))
                })?;
            let (module, promise) = declared.eval().map_err(|err| {
                anyhow::anyhow!("{module_name} failed to evaluate: {}", thrown(&js, err))
            })?;
            promise.into_future::<()>().await.map_err(|err| {
                anyhow::anyhow!("{module_name} failed to evaluate: {}", thrown(&js, err))
            })?;

            let default: Object = module.get("default").map_err(|_| {
                anyhow::anyhow!("{module_name} has no default export; a plugin is `export default {{ name, apply }}`")
            })?;
            let apply: Function = default.get("apply").map_err(|_| {
                anyhow::anyhow!("{module_name}'s default export has no `apply` function")
            })?;
            let ctx_object: Object = js.globals().get("ctx").map_err(|err| {
                anyhow::anyhow!("the ctx object went missing before apply(): {err}")
            })?;

            let answer: JsValue = apply
                .call((ctx_object,))
                .map_err(|err| anyhow::anyhow!("{module_name}: apply() failed: {}", thrown(&js, err)))?;
            // `apply` may be `async`. Awaiting the promise is what makes a
            // plugin that fetches its config at load time possible, and what
            // makes a rejection during load a load failure rather than an
            // unhandled rejection nobody sees.
            if let Some(promise) = answer.into_promise() {
                promise.into_future::<()>().await.map_err(|err| {
                    anyhow::anyhow!("{module_name}: apply() failed: {}", thrown(&js, err))
                })?;
            }
            Ok(())
        })
        .await
}

/// Call one registered callback with JSON arguments and read its answer back
/// as JSON.
///
/// Returns an owned future so [`super::invoke`] can hold several at once in a
/// `FuturesUnordered`, which is what gives a JS tool that calls `ctx.emit` a
/// handler in its own VM to reach. The re-entrancy is real: `AsyncContext`'s
/// lock is released every time the outer call parks on a host future, so the
/// inner call gets in.
pub(crate) fn call_function(
    state: &VmState,
    func: FnId,
    args: Vec<Value>,
) -> impl std::future::Future<Output = anyhow::Result<Value>> + use<> {
    let context = state.context.clone();
    async move {
        context
            .async_with(async |js| {
                let table: rquickjs::Array = js.globals().get(REGISTRY)?;
                let function: Function = table.get(func as usize).map_err(|_| {
                    rquickjs::Error::new_from_js_message(
                        "undefined",
                        "function",
                        "the plugin has no callback with that id; it was never registered",
                    )
                })?;

                let mut converted = Vec::with_capacity(args.len());
                for arg in &args {
                    converted.push(json_to_js(&js, arg)?);
                }
                let answer: JsValue = function.call((Rest(converted),))?;
                let answer = match answer.into_promise() {
                    Some(promise) => promise.into_future::<JsValue>().await?,
                    None => answer,
                };
                js_to_json(&answer)
            })
            .await
            .map_err(|err| {
                // The message is read out of the context rather than off the
                // `Error`, which prints "Exception generated by QuickJS" and
                // nothing else.
                anyhow::anyhow!("{}", describe(&err))
            })
    }
}

/// Run a plugin's teardowns, newest first.
///
/// An error is recorded and the next one runs, matching
/// [`crate::kernel::lifecycle`] and the Lua backend: a plugin gets to leak its
/// own socket, it does not get to leave the rest of the unload undone.
pub(crate) async fn run_effects(state: &VmState) -> VmShutdown {
    let effects = {
        let mut held = state.effects.lock().unwrap_or_else(PoisonError::into_inner);
        let mut taken = std::mem::take(&mut *held);
        taken.reverse();
        taken
    };

    let mut shutdown = VmShutdown::default();
    for (label, id) in effects {
        match call_function(state, id, Vec::new()).await {
            Ok(_) => shutdown.effects += 1,
            Err(err) => {
                tracing::error!(effect = %label, error = %err, "a JS teardown failed");
                shutdown.failures.push(format!("{label}: {err:#}"));
            }
        }
    }
    shutdown
}

/// Empty this VM's callback array before the context is dropped.
///
/// The Lua backend clears its `HashMap<FnId, mlua::Function>` for the same
/// reason: no handle on a function may outlive the engine it points into.
/// Here the handles are already inside the engine, so this is belt and
/// braces — it drops the closures a beat earlier and makes a call that arrives
/// during teardown fail with "no callback with that id" rather than running.
pub(crate) fn clear_functions(state: &VmState) {
    // Best effort: the runtime may already be gone, and there is nothing
    // useful to do about it if it is.
    let context = state.context.clone();
    let _ = futures_util::future::FutureExt::now_or_never(context.async_with(async |js| {
        if let Ok(table) = js.globals().get::<_, rquickjs::Array>(REGISTRY) {
            let _ = table.as_object().set("length", 0);
        }
    }));
}

// ---------------------------------------------------------------------------
// The sandbox
// ---------------------------------------------------------------------------

/// Remove the globals no capability grants and nothing honest needs.
///
/// The module docs carry the argument for each. This function is the whole of
/// the list: everything else a plugin can reach is either pure computation
/// (`Math`, `JSON`, `RegExp`, the typed arrays) or something the host put
/// there behind a capability.
///
/// Setting an absent global to `undefined` is already a no-op, so over-listing
/// costs nothing and under-listing is the only mistake available — the same
/// property [`crate::kernel::lua::host::narrow_stdlib`] relies on.
fn narrow_globals(js: &JsCtx<'_>) -> rquickjs::Result<()> {
    let globals = js.globals();
    for name in ["Atomics", "SharedArrayBuffer", "FinalizationRegistry"] {
        globals.remove(name)?;
    }
    Ok(())
}

/// Create this VM's callback array, hidden from `Object.keys`.
fn install_callback_table(js: &JsCtx<'_>) -> rquickjs::Result<()> {
    let table = rquickjs::Array::new(js.clone())?;
    // `prop` with an explicit descriptor rather than `set`, so a plugin
    // enumerating its own globals does not see the host's bookkeeping and a
    // plugin assigning to the name cannot replace the array wholesale.
    js.globals().prop(
        REGISTRY,
        // `Property::from` sets only HAS_VALUE, so the property is born
        // non-writable, non-enumerable and non-configurable. Naming the three
        // flags would *grant* them.
        rquickjs::object::Property::from(table),
    )
}

/// `console.log` / `.warn` / `.error` for a plugin, which go to the log.
///
/// JavaScript authors reach for `console` before they read any documentation,
/// so a VM without one produces a `TypeError` on the first debugging attempt.
/// It writes to `tracing` for [`crate::kernel::lua::host::install_print`]'s
/// reason: a plugin's VM never ends, so a buffer the host reads afterwards
/// would be a leak with a `console.log` in front of it. A plugin with
/// something to tell the *user* asks for `ui` and calls `wizard.ui.notify`.
fn install_console(js: &JsCtx<'_>, plugin: &str) -> rquickjs::Result<()> {
    let console = Object::new(js.clone())?;
    for (name, level) in [("log", 0u8), ("info", 0), ("debug", 0), ("warn", 1), ("error", 2)] {
        let plugin = plugin.to_string();
        let function = Function::new(js.clone(), move |args: Rest<JsValue>| {
            let line = args
                .0
                .iter()
                .map(display_value)
                .collect::<Vec<_>>()
                .join(" ");
            match level {
                1 => tracing::warn!(plugin = %plugin, "{line}"),
                2 => tracing::error!(plugin = %plugin, "{line}"),
                _ => tracing::debug!(plugin = %plugin, "{line}"),
            }
        })?;
        console.set(name, function)?;
    }
    js.globals().set("console", console)
}

/// One `console.log` argument, rendered the way a JavaScript author expects.
///
/// A bare string prints as itself rather than as `"itself"`, which is what
/// every `console` does and what makes the difference between a readable log
/// line and a quoted one.
fn display_value(value: &JsValue<'_>) -> String {
    if let Some(text) = value.as_string() {
        return text.to_string().unwrap_or_default();
    }
    match js_to_json(value) {
        Ok(Value::String(text)) => text,
        Ok(json) => json.to_string(),
        Err(_) => "<unreadable>".to_string(),
    }
}

// ---------------------------------------------------------------------------
// `wizard.*`
// ---------------------------------------------------------------------------

/// Build the `wizard` global: the ungated helpers, then one table per
/// capability the manifest declared.
fn install_wizard(js: &JsCtx<'_>, ctx: &Ctx, caps: &CapabilitySet) -> rquickjs::Result<()> {
    let wizard = Object::new(js.clone())?;
    wizard.set("plugin", ctx.name())?;
    // Identity marker, the sibling of `wizard.runtime == "luajit"`. A plugin
    // that has to know which engine it is in reads this rather than sniffing
    // for a global.
    wizard.set("runtime", "quickjs")?;

    install_fs(js, &wizard, ctx, caps)?;
    install_output_budget(js, &wizard)?;

    let sleep = Function::new(
        js.clone(),
        Async(|millis: u64| async move {
            tokio::time::sleep(Duration::from_millis(millis).min(MAX_SLEEP)).await;
            Ok::<(), rquickjs::Error>(())
        }),
    )?;
    wizard.set("sleep", sleep)?;

    let plugin = ctx.name().to_string();
    let log = Function::new(js.clone(), move |message: String| {
        tracing::info!(plugin = %plugin, "{message}");
    })?;
    wizard.set("log", log)?;

    if caps.contains(Capability::Filesystem) {
        install_paths(js, &wizard, ctx)?;
    }

    if caps.contains(Capability::Network) {
        let http = Object::new(js.clone())?;
        for (name, method) in [("get", "GET"), ("post", "POST"), ("put", "PUT")] {
            let host = ctx.host();
            let function = Function::new(
                js.clone(),
                Async(move |url: String, body: Opt<String>| {
                    let host = Arc::clone(&host);
                    async move { host.http(method, &url, body.0).await.map_err(external) }
                }),
            )?;
            http.set(name, function)?;
        }
        wizard.set("http", http)?;
    }

    if caps.contains(Capability::Model) {
        let model = Object::new(js.clone())?;
        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let complete = Function::new(
            js.clone(),
            Async(move |prompt: String| {
                let host = Arc::clone(&host);
                let plugin = plugin.clone();
                async move { host.model(&plugin, &prompt).await.map_err(external) }
            }),
        )?;
        model.set("complete", complete)?;
        wizard.set("model", model)?;
    }

    if caps.contains(Capability::Ui) {
        let ui = Object::new(js.clone())?;
        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let notify = Function::new(
            js.clone(),
            Async(move |text: String| {
                let host = Arc::clone(&host);
                let plugin = plugin.clone();
                async move { host.notify(&plugin, &text).await.map_err(external) }
            }),
        )?;
        ui.set("notify", notify)?;
        wizard.set("ui", ui)?;
    }

    if caps.contains(Capability::Agent) {
        let agent = Object::new(js.clone())?;
        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let spawn = Function::new(
            js.clone(),
            Async(move |task: String| {
                let host = Arc::clone(&host);
                let plugin = plugin.clone();
                async move { host.spawn_agent(&plugin, &task).await.map_err(external) }
            }),
        )?;
        agent.set("spawn", spawn)?;
        wizard.set("agent", agent)?;
    }

    if caps.contains(Capability::Process) {
        let process = Object::new(js.clone())?;
        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let run = Function::new(
            js.clone(),
            Async(move |command: String| {
                let host = Arc::clone(&host);
                let plugin = plugin.clone();
                async move { host.run(&plugin, &command).await.map_err(external) }
            }),
        )?;
        process.set("run", run)?;

        let host = ctx.host();
        let plugin = ctx.name().to_string();
        let exec = Function::new(
            js.clone(),
            Async(move |js: JsCtx<'_>, spec: Object<'_>| {
                let host = Arc::clone(&host);
                let plugin = plugin.clone();
                let request = exec_request(&spec);
                let js = js.clone();
                async move {
                    let outcome = host.exec(&plugin, request?).await.map_err(external)?;
                    let result = Object::new(js.clone())?;
                    result.set("stdout", outcome.stdout)?;
                    result.set("stderr", outcome.stderr)?;
                    // `null` rather than a sentinel for both, and the Lua
                    // backend's reason: `code = -1` for "signalled" is a
                    // number a plugin will compare against zero and get
                    // wrong. `null` is falsy and prints as nothing.
                    match outcome.code {
                        Some(code) => result.set("code", code)?,
                        None => result.set("code", JsValue::new_null(js.clone()))?,
                    }
                    match outcome.timed_out {
                        Some(secs) => result.set("timed_out", secs)?,
                        None => result.set("timed_out", JsValue::new_null(js.clone()))?,
                    }
                    Ok::<Object<'_>, rquickjs::Error>(result)
                }
            }),
        )?;
        process.set("exec", exec)?;
        wizard.set("process", process)?;
    }

    js.globals().set("wizard", wizard)
}

/// `wizard.fs.read` / `.write`, confined to the project root without
/// [`Capability::Filesystem`].
///
/// The confinement decision itself is [`resolve_plugin_path`], which is the
/// function the Lua host's `wizard.read_file` goes through. Two copies of that
/// walk would be two places for a `..` to stop being caught, which is the
/// argument `src/tools/http.rs` was split out of the web tools to make.
fn install_fs(
    js: &JsCtx<'_>,
    wizard: &Object<'_>,
    ctx: &Ctx,
    caps: &CapabilitySet,
) -> rquickjs::Result<()> {
    // Follows `filesystem` and not the standard-library profile, exactly as the
    // Lua host does: there is no library profile here to follow.
    let profile = if caps.contains(Capability::Filesystem) {
        Stdlib::Full
    } else {
        Stdlib::Sandboxed
    };
    let root = ctx.kernel().project_root().to_path_buf();

    let fs = Object::new(js.clone())?;
    let read_root = root.clone();
    let read = Function::new(js.clone(), move |path: String| {
        let resolved = resolve_plugin_path(&read_root, &path, profile)
            .map_err(|reason| external(anyhow::anyhow!(reason)))?;
        std::fs::read_to_string(&resolved).map_err(|err| external(anyhow::anyhow!("{err}")))
    })?;
    fs.set("read", read)?;

    let write = Function::new(js.clone(), move |path: String, contents: String| {
        let resolved = resolve_plugin_path(&root, &path, profile)
            .map_err(|reason| external(anyhow::anyhow!(reason)))?;
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(|err| external(anyhow::anyhow!("{err}")))?;
        }
        std::fs::write(&resolved, contents).map_err(|err| external(anyhow::anyhow!("{err}")))
    })?;
    fs.set("write", write)?;

    wizard.set("fs", fs)
}

/// `wizard.paths`: where Wizard keeps its own state, as strings.
///
/// Gated on [`Capability::Filesystem`] and populated from [`Config`]'s own
/// accessors, both for the reasons the Lua host's `install_paths` gives at
/// length — the short version being that a plugin deriving `~/.wizard` from
/// the environment sails straight past the temp-directory redirect
/// `cargo test` installs, and writes to a developer's real config.
fn install_paths(js: &JsCtx<'_>, wizard: &Object<'_>, ctx: &Ctx) -> rquickjs::Result<()> {
    use crate::config::Config;

    let paths = Object::new(js.clone())?;
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

/// `wizard.limits` and `wizard.truncate`: the context-window budgets and the
/// one function that applies them.
///
/// Ungated, and the same numbers the Lua host publishes, because they are
/// facts about what a *tool answer* may cost the model rather than about
/// either engine. A plugin that invented its own would drift from the native
/// ones the first time either moved.
fn install_output_budget(js: &JsCtx<'_>, wizard: &Object<'_>) -> rquickjs::Result<()> {
    let limits = Object::new(js.clone())?;
    limits.set("output", MAX_OUTPUT_BYTES)?;
    limits.set("diff", crate::tools::MAX_DIFF_BYTES)?;
    limits.set("search", crate::tools::MAX_SEARCH_BYTES)?;
    limits.set("listing", crate::tools::MAX_LISTING_BYTES)?;
    limits.set("error", crate::tools::MAX_ERROR_BYTES)?;
    wizard.set("limits", limits)?;

    let truncate = Function::new(js.clone(), |text: String, max_bytes: Opt<usize>| {
        truncate_output(text, max_bytes.0.unwrap_or(MAX_OUTPUT_BYTES).max(1))
    })?;
    wizard.set("truncate", truncate)
}

/// One `wizard.process.exec({ argv, cwd, timeout_ms })`.
///
/// Every failure is a refusal rather than a default, for the Lua host's
/// reason: both available mistakes are silent. An `argv` given as a string
/// runs a program whose name contains spaces, and an empty one becomes a
/// request the host has to reject with less context than this has.
fn exec_request(spec: &Object<'_>) -> rquickjs::Result<crate::kernel::ExecRequest> {
    let argv: rquickjs::Array = spec.get("argv").map_err(|_| {
        external(anyhow::anyhow!(
            "wizard.process.exec needs argv: ['program', 'arg', ...]; a shell line goes to \
             wizard.process.run"
        ))
    })?;
    let argv: Vec<String> = argv.iter::<String>().collect::<rquickjs::Result<_>>()?;
    if argv.is_empty() {
        return Err(external(anyhow::anyhow!(
            "wizard.process.exec was given an empty argv"
        )));
    }
    let cwd: Option<String> = spec.get("cwd").ok().filter(|s: &String| !s.is_empty());
    let millis: Option<u64> = spec.get("timeout_ms").ok();
    Ok(crate::kernel::ExecRequest {
        argv,
        cwd: cwd.map(std::path::PathBuf::from),
        timeout: millis.map_or(DEFAULT_EXEC_TIMEOUT, Duration::from_millis),
    })
}

/// Carry a host error into JavaScript as a plain thrown message.
///
/// Flattened with `{:#}` rather than `to_string()`, which prints only the
/// outermost layer. A host call's reason is almost always underneath one, and
/// a plugin author handed the top half alone has nothing to act on.
fn external(err: anyhow::Error) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host", "value", format!("{err:#}"))
}

/// What a QuickJS failure actually said.
///
/// `rquickjs::Error::Exception` prints as "Exception generated by QuickJS" and
/// carries nothing, because the thrown value is left in the context for the
/// embedder to pick up. Everything user-facing has to go through here or
/// through [`thrown`], or a plugin author sees that sentence and nothing else.
fn describe(err: &rquickjs::Error) -> String {
    match err {
        rquickjs::Error::Exception => "an uncaught exception".to_string(),
        other => other.to_string(),
    }
}

/// [`describe`] with the context in hand, so the thrown value can be read.
fn thrown(js: &JsCtx<'_>, err: rquickjs::Error) -> String {
    if !matches!(err, rquickjs::Error::Exception) {
        return err.to_string();
    }
    let caught = js.catch();
    if let Some(exception) = caught.as_exception() {
        let message = exception.message().unwrap_or_default();
        return match exception.stack() {
            Some(stack) if !stack.trim().is_empty() => format!("{message}\n{}", stack.trim_end()),
            _ if message.is_empty() => "an uncaught exception".to_string(),
            _ => message,
        };
    }
    match js_to_json(&caught) {
        Ok(Value::String(text)) => text,
        Ok(json) => json.to_string(),
        Err(_) => "an uncaught exception".to_string(),
    }
}

// ---------------------------------------------------------------------------
// `ctx`
// ---------------------------------------------------------------------------

/// Build the `ctx` object the plugin's `apply` is handed.
///
/// A global rather than a value threaded through the module, because a module
/// is evaluated before anything can be passed to it. `apply(ctx)` is still how
/// a plugin receives it — [`apply`] reads the global and passes it — so a
/// plugin that only ever touches its argument never sees the global, and the
/// two are the same object.
fn install_ctx<'js>(
    js: &JsCtx<'js>,
    ctx: &Ctx,
    handle: &VmHandle,
    registry: &Registry,
) -> rquickjs::Result<()> {
    let object = Object::new(js.clone())?;

    object.set("tool", tool_fn(js, ctx, handle, registry)?)?;
    object.set("command", command_fn(js, ctx, handle, registry)?)?;
    object.set("provider", provider_fn(js, ctx)?)?;
    object.set("on", on_fn(js, ctx, handle, registry)?)?;
    object.set("emit", emit_fn(js, ctx)?)?;
    object.set("provide", provide_fn(js, ctx)?)?;
    object.set("inject", inject_fn(js, ctx)?)?;
    object.set("plugin", plugin_fn(js, ctx)?)?;
    object.set("effect", effect_fn(js, registry)?)?;
    object.set("config", config_fn(js, ctx)?)?;

    let name = ctx.name().to_string();
    object.set("name", Function::new(js.clone(), move || name.clone())?)?;

    js.globals().set("ctx", object)
}

fn tool_fn<'js>(
    js: &JsCtx<'js>,
    ctx: &Ctx,
    handle: &VmHandle,
    registry: &Registry,
) -> rquickjs::Result<Function<'js>> {
    let ctx = ctx.clone();
    let handle = handle.clone();
    let registry = registry.clone();
    Function::new(js.clone(), move |js: JsCtx<'_>, spec: Object<'_>| {
        let name: String = spec.get("name")?;
        let description: String = spec.get("description").unwrap_or_default();
        let parameters = match spec.get::<_, JsValue>("parameters") {
            Ok(value) if !value.is_undefined() && !value.is_null() => js_to_json(&value)?,
            _ => empty_schema(),
        };
        let access = match spec.get::<_, String>("access").as_deref() {
            Ok("read_only") => ToolAccess::ReadOnly,
            Ok("edit") => ToolAccess::Edit,
            // Anything else, including nothing, is the conservative answer:
            // `ToolAccess` drives the plan-mode read-only gate, and guessing
            // wrong in the other direction lets a plugin write in plan mode.
            _ => ToolAccess::Execute,
        };
        let execute: Function = spec.get("execute").map_err(|_| {
            external(anyhow::anyhow!(
                "ctx.tool({{ name: '{name}' }}) has no execute function"
            ))
        })?;
        let func = registry.hold(&js, execute)?;

        ctx.tool(Arc::new(JsTool {
            name,
            description,
            parameters,
            access,
            handle: handle.clone(),
            func,
        }))
        .map_err(|err| external(anyhow::anyhow!("{err}")))
    })
}

fn command_fn<'js>(
    js: &JsCtx<'js>,
    ctx: &Ctx,
    handle: &VmHandle,
    registry: &Registry,
) -> rquickjs::Result<Function<'js>> {
    let ctx = ctx.clone();
    let handle = handle.clone();
    let registry = registry.clone();
    Function::new(js.clone(), move |js: JsCtx<'_>, spec: Object<'_>| {
        let name: String = spec.get("name")?;
        let description: String = spec.get("description").unwrap_or_default();
        let args: String = spec.get("args").unwrap_or_default();
        let run: Function = spec.get("run").map_err(|_| {
            external(anyhow::anyhow!(
                "ctx.command({{ name: '{name}' }}) has no run function"
            ))
        })?;
        let func = registry.hold(&js, run)?;
        let mut command = PluginCommand::new(
            name.clone(),
            description,
            Arc::new(JsCommand {
                handle: handle.clone(),
                func,
            }),
        )
        .args(args);
        // Absent means every surface, matching the Rust and Lua defaults.
        // Present and empty is a plugin saying "nowhere", which is a plugin
        // bug rather than a shorthand for "everywhere" — reading it as the
        // latter would make a typo silently do the opposite of what it says.
        if let Ok(surfaces) = spec.get::<_, rquickjs::Array>("surfaces") {
            let mut named = Vec::new();
            for value in surfaces.iter::<String>() {
                named.push(surface_named(&name, &value?)?);
            }
            command = command.only(&named);
        }
        ctx.command(command)
            .map_err(|err| external(anyhow::anyhow!("{err}")))
    })
}

/// One entry of `ctx.command({ surfaces: [...] })`.
///
/// Refuses an unknown name rather than skipping it: the failure mode of
/// skipping one is a command silently missing from exactly the surface the
/// author meant to name.
fn surface_named(command: &str, value: &str) -> rquickjs::Result<Surface> {
    match value {
        "tui" => Ok(Surface::Tui),
        "gui" => Ok(Surface::Gui),
        "gateway" => Ok(Surface::Gateway),
        other => Err(external(anyhow::anyhow!(
            "ctx.command({{ name: '{command}' }}) names surface '{other}' (tui|gui|gateway)"
        ))),
    }
}

/// `ctx.provider` exists so the shape is the same in all three languages, and
/// refuses so the refusal is at least honest.
///
/// The Lua host's argument, unchanged and for the same reason: an
/// `LlmProvider` is TLS, SSE framing and nine providers' worth of wire quirks,
/// which is the half `docs/plugins.md` puts in Rust. A JavaScript
/// implementation would reach all of it through a host API wide enough to be
/// Rust with a slower calling convention.
fn provider_fn<'js>(js: &JsCtx<'js>, ctx: &Ctx) -> rquickjs::Result<Function<'js>> {
    let plugin = ctx.name().to_string();
    Function::new(js.clone(), move |_spec: Opt<JsValue>| {
        Err::<(), _>(external(anyhow::anyhow!(
            "plugin '{plugin}': a provider cannot be registered from JavaScript. \
             Providers are transport code and stay in Rust; see docs/plugins.md."
        )))
    })
}

fn on_fn<'js>(
    js: &JsCtx<'js>,
    ctx: &Ctx,
    handle: &VmHandle,
    registry: &Registry,
) -> rquickjs::Result<Function<'js>> {
    let ctx = ctx.clone();
    let handle = handle.clone();
    let registry = registry.clone();
    Function::new(
        js.clone(),
        move |js: JsCtx<'_>, event: String, callback: Function<'_>, priority: Opt<i32>| {
            let parsed = Event::parse(&event).ok_or_else(|| {
                external(anyhow::anyhow!(
                    "'{event}' is not an event; a subscription to a name nothing emits \
                     would silently never fire"
                ))
            })?;
            let func = registry.hold(&js, callback)?;
            ctx.on(
                parsed,
                priority.0.unwrap_or(super::super::bus::DEFAULT_PRIORITY),
                Arc::new(JsEventHandler {
                    handle: handle.clone(),
                    func,
                }),
            );
            Ok(())
        },
    )
}

fn emit_fn<'js>(js: &JsCtx<'js>, ctx: &Ctx) -> rquickjs::Result<Function<'js>> {
    let ctx = ctx.clone();
    Function::new(
        js.clone(),
        Async(move |js: JsCtx<'_>, event: String, payload: Opt<JsValue<'_>>| {
            let ctx = ctx.clone();
            let parsed = Event::parse(&event);
            let payload = payload
                .0
                .map(|value| js_to_json(&value))
                .transpose()
                .map(|value| value.unwrap_or(Value::Null));
            let js = js.clone();
            async move {
                let event = parsed
                    .ok_or_else(|| external(anyhow::anyhow!("'{event}' is not an event")))?;
                let dispatch = ctx.emit(event, payload?).await;
                let result = Object::new(js.clone())?;
                result.set("payload", json_to_js(&js, &dispatch.payload)?)?;
                result.set("vetoed", dispatch.is_vetoed())?;
                match dispatch.veto {
                    Some(veto) => {
                        result.set("veto", veto.reason)?;
                        result.set("veto_by", veto.plugin)?;
                    }
                    None => result.set("veto", JsValue::new_null(js.clone()))?,
                }
                result.set("ran", dispatch.ran)?;
                result.set("failures", dispatch.failures.len())?;
                Ok::<Object<'_>, rquickjs::Error>(result)
            }
        }),
    )
}

fn provide_fn<'js>(js: &JsCtx<'js>, ctx: &Ctx) -> rquickjs::Result<Function<'js>> {
    let ctx = ctx.clone();
    Function::new(js.clone(), move |name: String, value: JsValue<'_>| {
        ctx.provide(name, Service::data(js_to_json(&value)?));
        Ok::<(), rquickjs::Error>(())
    })
}

fn inject_fn<'js>(js: &JsCtx<'js>, ctx: &Ctx) -> rquickjs::Result<Function<'js>> {
    let ctx = ctx.clone();
    Function::new(js.clone(), move |js: JsCtx<'_>, name: String| {
        // A native service is `undefined` here, which is the same `undefined`
        // an absent one gives — see the `Ctx` module docs. JavaScript cannot
        // call a Rust trait object, and pretending otherwise would only move
        // the failure later.
        match ctx.inject(&name).as_ref().and_then(Service::as_data) {
            Some(value) => json_to_js(&js, value),
            None => Ok(JsValue::new_undefined(js.clone())),
        }
    })
}

/// `ctx.plugin(name, config)` — load a child JavaScript plugin from the plugin
/// root.
///
/// Only JavaScript children, mirroring the Lua host's rule that a Lua parent
/// loads Lua children: a scripted plugin has no way to name a Rust one, since
/// those are values behind a cargo feature rather than directories. A plugin
/// that wants a child in the *other* scripting language is a case nobody has
/// asked for and would need a way to say which, so the directory's
/// `plugin.js` is what a JavaScript parent looks for.
fn plugin_fn<'js>(js: &JsCtx<'js>, ctx: &Ctx) -> rquickjs::Result<Function<'js>> {
    let ctx = ctx.clone();
    Function::new(
        js.clone(),
        Async(move |name: String, config: Opt<JsValue<'_>>| {
            let ctx = ctx.clone();
            let config = config
                .0
                .filter(|value| !value.is_undefined() && !value.is_null())
                .map(|value| js_to_json(&value))
                .transpose();
            async move {
                let config = config?;
                if name.contains(['/', '\\']) || name.contains("..") {
                    return Err(external(anyhow::anyhow!(
                        "'{name}' is not a plugin name; ctx.plugin takes a name under the \
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
                .map_err(|err| external(anyhow::anyhow!("{err}")))?;
                ctx.record_child(id.clone());
                Ok(id.to_string())
            }
        }),
    )
}

/// `ctx.effect(dispose, label)` — a teardown that lives in the VM.
///
/// A JavaScript closure cannot become a Rust `FnOnce`, so it is held in the
/// callback table and run by [`run_effects`] during shutdown. The observable
/// ordering matches both other backends: after every registry entry is gone,
/// newest first.
fn effect_fn<'js>(js: &JsCtx<'js>, registry: &Registry) -> rquickjs::Result<Function<'js>> {
    let registry = registry.clone();
    Function::new(
        js.clone(),
        move |js: JsCtx<'_>, dispose: Function<'_>, label: Opt<String>| {
            let func = registry.hold(&js, dispose)?;
            let label = label.0.unwrap_or_else(|| format!("js effect #{func}"));
            registry
                .effects
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((label, func));
            Ok::<(), rquickjs::Error>(())
        },
    )
}

fn config_fn<'js>(js: &JsCtx<'js>, ctx: &Ctx) -> rquickjs::Result<Function<'js>> {
    let config = ctx.config().clone();
    Function::new(js.clone(), move |js: JsCtx<'_>| json_to_js(&js, &config))
}

/// A JSON Schema for a tool that declared no parameters.
///
/// An empty object rather than `null`: every provider's tool-calling wire
/// format wants a schema, and the one that means "no arguments" is an object
/// with no properties.
fn empty_schema() -> Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

/// The second argument a tool body is called with: as much of the
/// [`ToolContext`] as can cross into JavaScript.
///
/// Which is one field, and the Lua host's write-up says why at length:
/// thirteen of `ToolContext`'s sixteen are Rust handles a JavaScript value
/// cannot be, and they reach a plugin through `wizard.*` if they reach it at
/// all. `cwd` is different — it is a path, it is what every path-taking tool
/// resolves against, and a tool that does not get it operates on the wrong
/// directory without failing.
fn tool_context(ctx: &ToolContext) -> Value {
    serde_json::json!({ "cwd": ctx.cwd.to_string_lossy() })
}

/// A tool whose body is a JavaScript function in a plugin's VM.
struct JsTool {
    name: String,
    description: String,
    parameters: Value,
    access: ToolAccess,
    handle: VmHandle,
    func: FnId,
}

#[async_trait]
impl Tool for JsTool {
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
/// The Lua backend's three shapes, unchanged, so a tool ported between the two
/// languages keeps its contract: a string is the content, `null`/`undefined`
/// is empty content, and `{ content, is_error }` is a [`ToolOutput`] spelled
/// out. Anything else is JSON, so a plugin returning structured data is
/// unaffected unless it happens to have a string `content` key.
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

/// A slash command whose body is a JavaScript function.
struct JsCommand {
    handle: VmHandle,
    func: FnId,
}

impl CommandHandler for JsCommand {
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

/// An event handler whose body is a JavaScript function.
///
/// The verdict protocol is the Lua one: return nothing to observe,
/// `{ payload }` to rewrite, `{ veto: "reason" }` to refuse. A returned object
/// with neither key is an observation rather than a rewrite to that object,
/// because `return {}` from a handler that meant nothing by it should not
/// blank the payload for everything downstream.
struct JsEventHandler {
    handle: VmHandle,
    func: FnId,
}

impl EventHandler for JsEventHandler {
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

