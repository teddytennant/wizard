//! Lua plugins: one long-lived VM each, on a task of its own.
//!
//! This is the change from `src/tools/lua.rs`, where every tool call gets a
//! fresh throwaway state and can therefore hold no state between calls. Here a
//! plugin's VM is created once at load and dropped once at unload, so the
//! `local store = {}` in the spec's example is a real store, `ctx:on` handlers
//! close over it, and `ctx:effect` has something to tear down.
//!
//! # The task, and why it is a task
//!
//! `mlua`'s `Lua` is `Send` but not `Sync` under the `send` feature, so it
//! cannot simply live behind an `Arc` and be called from three turns at once.
//! It lives on one tokio task instead, and everything that wants to reach it —
//! a tool the model called, an event the bus dispatched, a slash command —
//! sends a [`VmRequest`] down a channel and awaits a oneshot. The handle is
//! [`VmHandle`], it is cheap to clone, and when the task is gone every call
//! through it fails with a message saying so rather than hanging.
//!
//! The loop drives requests through a `FuturesUnordered` rather than one at a
//! time, and that is load-bearing rather than an optimisation: a Lua tool that
//! calls `ctx:emit` reaches a Lua handler *in its own VM*, and a strictly
//! sequential loop would be parked on the first request waiting for a second it
//! will never pick up. Concurrency here is cooperative and single-threaded —
//! each resume of a Lua coroutine runs to its next yield without interleaving —
//! so it buys re-entrancy without buying a data race.
//!
//! # The bound is the existing one, pushed forward
//!
//! `docs/plugins.md` records the spike: `disable_jit` + `install_hook` from
//! `src/tools/lua.rs` bound an `exec_async` chunk exactly as they bound a sync
//! one, including a spin placed after an await point, and the three details
//! that make them work (`jit.flush()` after `jit.off()`, `set_global_hook`
//! rather than `set_hook`, and `install_stop_guard`) are all inside
//! [`install_bounds`], which is why this module calls that and reimplements
//! none of it.
//!
//! One thing does have to change for a VM that lives for hours.
//! [`BoundsHandle::fixed`] arms a deadline once, which for a long-lived plugin
//! would mean it dies thirty seconds after it loads. The deadline is behind a
//! mutex precisely so it can be pushed (code mode already does this), so
//! [`Bound::arm`] resets it at the start of every call and [`Bound::relax`]
//! parks it while the VM is idle. The memory ceiling is not pushed and applies
//! continuously, which is what "bound a plugin's whole lifetime" should mean:
//! a plugin may take as long as it likes across many calls and may not hold a
//! gigabyte at any point in any of them.
//!
//! [`BoundsHandle::stop`] is also reset once a VM goes idle. It latches when
//! the hook fires, and the stop guard re-raises through `pcall` for as long as
//! it is latched — correct for one bounded chunk, and permanently fatal for a
//! VM that is supposed to survive its first timeout. Clearing it when nothing
//! is in flight is what makes "a VM that had one call bounded is still usable
//! for the next one" true here.

pub mod host;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::tools::lua::{BoundsHandle, StopReason, install_bounds};

use super::lifecycle::PluginId;
use super::manifest::{PluginManifest, PluginSource};
use super::{Kernel, KernelError, PluginKind, VmShutdown};

/// A Lua function the host holds a handle on, by number.
///
/// Numbers rather than `mlua::RegistryKey`s because the handle that calls back
/// in — a `Tool` sitting in the agent's registry — must be able to name a
/// function without holding anything that borrows the VM.
pub type FnId = u64;

/// Compute a bounded plugin may spend inside one call before the hook stops it.
///
/// Per call and not per lifetime; see the module docs. Generous for policy and
/// orchestration, which is all a Lua plugin is supposed to be doing, and far
/// below the point where a user thinks the agent has hung.
pub const DEFAULT_CALL_BUDGET: Duration = Duration::from_secs(30);

/// Memory a bounded plugin's VM may hold. Same figure the sandboxed scripted
/// tools use, and for the same reason: generous for text munging, far below
/// what it takes to disturb the host.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// How far out the wall ceiling is set for a plugin VM.
///
/// [`BoundsHandle::wall`] is deliberately not extendable — for a scripted tool
/// it is what stops a program pushing its own deadline forever. A plugin is
/// meant to live for the session, so the ceiling is a session-length figure
/// rather than a call-length one. It is still a ceiling: a VM that somehow
/// stays alive for a year stops.
const VM_LIFETIME: Duration = Duration::from_secs(365 * 24 * 3600);

/// How long an unload waits for a VM to run its teardowns before abandoning it.
///
/// A bound rather than an await, because the VM being wedged is exactly the
/// state an unload is most likely to be called in, and an unload that can hang
/// is worse than a teardown that does not run.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// What a caller asks the VM task to do.
enum VmRequest {
    Invoke {
        func: FnId,
        args: Vec<Value>,
        reply: oneshot::Sender<anyhow::Result<Value>>,
    },
    Shutdown {
        reply: oneshot::Sender<VmShutdown>,
    },
}

/// A cheap, clonable way to call into one plugin's VM.
#[derive(Clone)]
pub struct VmHandle {
    plugin: Arc<str>,
    tx: mpsc::Sender<VmRequest>,
}

impl VmHandle {
    /// Call a registered Lua function and convert the result to JSON.
    ///
    /// Every failure mode is an `Err` and none of them is a hang: a dead task
    /// closes the channel, a dropped reply closes the oneshot, and a Lua error
    /// comes back as itself.
    pub async fn call(&self, func: FnId, args: Vec<Value>) -> anyhow::Result<Value> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(VmRequest::Invoke { func, args, reply })
            .await
            .map_err(|_| anyhow::anyhow!("the Lua VM for plugin '{}' is gone", self.plugin))?;
        answer.await.map_err(|_| {
            anyhow::anyhow!(
                "the Lua VM for plugin '{}' dropped a call without answering",
                self.plugin
            )
        })?
    }

    pub fn plugin(&self) -> &str {
        &self.plugin
    }
}

/// A loaded Lua plugin, from the kernel's side. Dropping it stops the VM.
pub struct LuaPlugin {
    handle: VmHandle,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl LuaPlugin {
    pub fn handle(&self) -> &VmHandle {
        &self.handle
    }

    /// Run the plugin's Lua teardowns and stop the VM.
    ///
    /// Consumes the handle because there is nothing to call afterwards. A VM
    /// that does not answer within [`SHUTDOWN_GRACE`] is abandoned and
    /// reported, rather than held onto: `Drop` aborts the task either way, so
    /// the worst case is a leaked socket in a plugin that was already wedged.
    pub async fn shutdown(self) -> VmShutdown {
        let (reply, answer) = oneshot::channel();
        if self
            .handle
            .tx
            .send(VmRequest::Shutdown { reply })
            .await
            .is_err()
        {
            return VmShutdown::default();
        }
        match tokio::time::timeout(SHUTDOWN_GRACE, answer).await {
            Ok(Ok(shutdown)) => shutdown,
            Ok(Err(_)) => VmShutdown::default(),
            Err(_) => {
                tracing::warn!(
                    plugin = %self.handle.plugin,
                    "a Lua plugin did not finish its teardowns in time; abandoning its VM"
                );
                VmShutdown {
                    effects: 0,
                    failures: vec![format!(
                        "{}: teardowns did not finish within {}s",
                        self.handle.plugin,
                        SHUTDOWN_GRACE.as_secs()
                    )],
                }
            }
        }
    }
}

impl Drop for LuaPlugin {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Aborting rather than waiting: `Drop` cannot await, and a VM
            // spinning inside Lua would never reach a yield point for the task
            // to be cancelled at anyway. The abort is what guarantees the VM is
            // gone once the plugin record is dropped.
            task.abort();
        }
    }
}

/// The per-call bound, and the two operations a long-lived VM needs that a
/// one-shot chunk does not.
pub(crate) struct Bound {
    handle: BoundsHandle,
    budget: Duration,
}

impl Bound {
    fn new(budget: Duration) -> Self {
        let now = Instant::now();
        Bound {
            budget,
            handle: BoundsHandle {
                deadline: Arc::new(Mutex::new(now + budget)),
                wall: now + VM_LIFETIME,
                memory_limit: MEMORY_LIMIT,
                stop: Arc::new(std::sync::atomic::AtomicU8::new(StopReason::None.as_u8())),
                cancel: None,
            },
        }
    }

    /// Start a call's clock.
    fn arm(&self) {
        *self
            .handle
            .deadline
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now() + self.budget;
    }

    /// Park the clock while nothing is running, and un-latch the stop flag so
    /// the next call is not re-raised out of by the stop guard. See the module
    /// docs.
    fn relax(&self) {
        *self
            .handle
            .deadline
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now() + VM_LIFETIME;
        self.handle
            .stop
            .store(StopReason::None.as_u8(), Ordering::SeqCst);
    }
}

/// Load a Lua plugin from a directory holding `manifest.toml` and `plugin.lua`.
pub async fn load_dir(
    kernel: &Kernel,
    dir: &Path,
    source: PluginSource,
    parent: Option<PluginId>,
    config: Option<Value>,
) -> Result<PluginId, KernelError> {
    let manifest_path = dir.join("manifest.toml");
    let raw = std::fs::read_to_string(&manifest_path).map_err(|err| KernelError::Apply {
        plugin: dir.display().to_string(),
        source: anyhow::anyhow!("reading {}: {err}", manifest_path.display()),
    })?;
    let manifest = PluginManifest::parse(&raw)?;

    let script_path = dir.join("plugin.lua");
    let script = std::fs::read_to_string(&script_path).map_err(|err| KernelError::Apply {
        plugin: manifest.name.clone(),
        source: anyhow::anyhow!("reading {}: {err}", script_path.display()),
    })?;

    load_source(
        kernel,
        manifest,
        source,
        &script,
        &format!("@{}", script_path.display()),
        parent,
        config,
    )
    .await
}

/// Load a Lua plugin from source already in hand.
///
/// `chunk_name` is what appears in a Lua traceback, so it should be a real path
/// when there is one — mlua's `@` prefix is what makes it print as a file name
/// rather than as the source text.
pub async fn load_source(
    kernel: &Kernel,
    manifest: PluginManifest,
    source: PluginSource,
    script: &str,
    chunk_name: &str,
    parent: Option<PluginId>,
    config: Option<Value>,
) -> Result<PluginId, KernelError> {
    manifest.validate()?;
    let id = PluginId::new(&manifest.name);
    kernel.reserve(&id)?;

    let manifest = Arc::new(manifest);
    let ctx = kernel.context(&id, Arc::clone(&manifest), config);
    let (tx, rx) = mpsc::channel(32);
    let handle = VmHandle {
        plugin: Arc::from(id.as_str()),
        tx,
    };

    let (ready, started) = oneshot::channel();
    let task = tokio::spawn(vm_task(VmSetup {
        ctx,
        handle: handle.clone(),
        script: script.to_string(),
        chunk_name: chunk_name.to_string(),
        source,
        rx,
        ready,
    }));

    let plugin = LuaPlugin {
        handle,
        task: Some(task),
    };

    match started.await {
        Ok((ledger, Ok(()))) => {
            kernel.finish_load(super::LoadedPlugin {
                id: id.clone(),
                manifest,
                source,
                parent,
                kind: PluginKind::Lua(plugin),
                ledger,
            });
            Ok(id)
        }
        Ok((ledger, Err(err))) => {
            // The VM took itself down; whatever it managed to register before
            // failing is disposed here so a failed load leaves nothing. Any
            // child it had already loaded goes first, because that child is a
            // plugin in its own right and its own unload is the only thing
            // that stops its VM.
            drop(plugin);
            for child in ledger.children() {
                let _ = kernel.unload(child).await;
            }
            super::lifecycle::dispose(kernel.slots(), &id, ledger, |_| None);
            kernel.release(&id);
            Err(KernelError::Apply {
                plugin: id.to_string(),
                source: err,
            })
        }
        Err(_) => {
            drop(plugin);
            kernel.release(&id);
            Err(KernelError::Apply {
                plugin: id.to_string(),
                source: anyhow::anyhow!("the plugin's Lua VM panicked while starting"),
            })
        }
    }
}

/// Everything the VM task needs, in one struct so the spawn is readable.
struct VmSetup {
    ctx: super::Ctx,
    handle: VmHandle,
    script: String,
    chunk_name: String,
    source: PluginSource,
    rx: mpsc::Receiver<VmRequest>,
    ready: oneshot::Sender<(super::Ledger, anyhow::Result<()>)>,
}

/// The VM's whole life: build, apply, serve, tear down.
async fn vm_task(setup: VmSetup) {
    let VmSetup {
        ctx,
        handle,
        script,
        chunk_name,
        source,
        mut rx,
        ready,
    } = setup;

    let plugin = ctx.name().to_string();
    let state = match host::build(&ctx, &handle, source) {
        Ok(state) => state,
        Err(err) => {
            let _ = ready.send((ctx.into_ledger(), Err(err)));
            return;
        }
    };

    if let Some(bound) = &state.bound {
        bound.arm();
    }
    let applied = apply(&state, &script, &chunk_name).await;
    if let Some(bound) = &state.bound {
        bound.relax();
    }

    // The ledger travels back whether `apply` succeeded or not. A plugin that
    // registered two tools and then errored on the third registered two tools,
    // and the caller needs the record to take them out again — sweeping by
    // plugin id alone would miss anything the sweep does not cover, which is
    // every registry except the bus and the services.
    let ledger = ctx.into_ledger();
    if let Err(err) = applied {
        let _ = ready.send((ledger, Err(err)));
        return;
    }

    // Handed over now rather than at the end: everything the plugin registers,
    // it registered during `apply`, and the kernel needs the record before it
    // will answer a call that reaches back in here.
    if ready.send((ledger, Ok(()))).is_err() {
        return;
    }

    let mut inflight = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            Some(()) = inflight.next(), if !inflight.is_empty() => {
                if inflight.is_empty() && let Some(bound) = &state.bound {
                    bound.relax();
                }
            }
            request = rx.recv() => match request {
                Some(VmRequest::Invoke { func, args, reply }) => {
                    if let Some(bound) = &state.bound {
                        bound.arm();
                    }
                    inflight.push(invoke(&state, func, args, reply));
                }
                Some(VmRequest::Shutdown { reply }) => {
                    // Anything still in flight is abandoned: its caller gets a
                    // closed oneshot, which reads as "the VM is gone", which is
                    // true.
                    if let Some(bound) = &state.bound {
                        bound.arm();
                    }
                    let _ = reply.send(host::run_effects(&state).await);
                    break;
                }
                None => break,
            },
        }
    }

    tracing::debug!(plugin = %plugin, "a plugin's Lua VM stopped");
    // Drop the function table before the state, so no `mlua::Function` outlives
    // the `Lua` it points into.
    state
        .functions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

/// Run the chunk and call its `apply`.
async fn apply(state: &host::VmState, script: &str, chunk_name: &str) -> anyhow::Result<()> {
    let table: mlua::Table = state
        .lua
        .load(script)
        .set_name(chunk_name)
        .eval_async()
        .await
        .map_err(|err| anyhow::anyhow!("{chunk_name} did not return a plugin table: {err}"))?;

    let apply: mlua::Function = table
        .get("apply")
        .map_err(|_| anyhow::anyhow!("{chunk_name} returned a table with no `apply` function"))?;

    apply
        .call_async::<()>(state.ctx_table.clone())
        .await
        .map_err(|err| anyhow::anyhow!("{chunk_name}: apply() failed: {err}"))
}

/// One `Invoke`, as a future the loop can hold alongside others.
///
/// Returns `()` and answers through the oneshot rather than returning the
/// result, because `FuturesUnordered` wants one type and the loop has nothing
/// to do with the answer.
fn invoke(
    state: &host::VmState,
    func: FnId,
    args: Vec<Value>,
    reply: oneshot::Sender<anyhow::Result<Value>>,
) -> impl std::future::Future<Output = ()> + use<> {
    let function = state
        .functions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&func)
        .cloned();
    let lua = state.lua.clone();
    let bound_reason = state
        .bound
        .as_ref()
        .map(|bound| (Arc::clone(&bound.handle.stop), bound.budget));

    async move {
        let answer = match function {
            None => Err(anyhow::anyhow!(
                "the plugin has no function #{func}; it was unregistered or never existed"
            )),
            Some(function) => {
                let converted: mlua::Result<Vec<mlua::Value>> = args
                    .iter()
                    .map(|arg| crate::tools::lua::json_to_lua(&lua, arg))
                    .collect();
                match converted {
                    Err(err) => Err(anyhow::anyhow!("converting arguments to Lua: {err}")),
                    Ok(values) => {
                        let multi = mlua::MultiValue::from_iter(values);
                        match function.call_async::<mlua::Value>(multi).await {
                            Ok(value) => crate::tools::lua::lua_to_json(&lua, value)
                                .map_err(|err| anyhow::anyhow!("converting the result: {err}")),
                            Err(err) => {
                                let (reason, budget) = bound_reason
                                    .map(|(flag, budget)| {
                                        (StopReason::from_u8(flag.load(Ordering::SeqCst)), budget)
                                    })
                                    .unwrap_or((StopReason::None, DEFAULT_CALL_BUDGET));
                                Err(describe_failure(reason, budget, err))
                            }
                        }
                    }
                }
            }
        };
        let _ = reply.send(answer);
    }
}

/// Turn a Lua error into one a reader can act on.
///
/// The flag, never the message text: `mlua::Error::runtime` looks identical for
/// a timeout, a memory cap and an ordinary `error()` call, and a plugin can
/// `error("exceeded its time budget")` on purpose. This is the same reasoning
/// [`BoundsHandle::stop`] exists for, applied one layer up.
fn describe_failure(reason: StopReason, budget: Duration, err: mlua::Error) -> anyhow::Error {
    match reason {
        StopReason::Time => anyhow::anyhow!(
            "the plugin exceeded its {}ms compute budget for one call",
            budget.as_millis()
        ),
        StopReason::Memory => anyhow::anyhow!(
            "the plugin exceeded its {} MB memory budget",
            MEMORY_LIMIT / (1024 * 1024)
        ),
        StopReason::Interrupted => anyhow::anyhow!("the plugin was interrupted"),
        StopReason::Calls | StopReason::None => anyhow::anyhow!("{err}"),
    }
}

/// Install the existing bounds machinery on a VM that is going to be bounded.
///
/// One line, and a whole doc comment, because the temptation this exists to
/// resist is writing the four lines it wraps by hand: `install_bounds` is
/// `disable_jit` (which is also `jit.flush()`, and which takes the `jit` table
/// away so the chunk cannot switch the compiler back on) followed by
/// `install_hook` (which is also `install_stop_guard`, and which uses
/// `set_global_hook` rather than `set_hook` so a coroutine does not uninstall
/// it for the whole VM). `docs/plugins.md` records all three being
/// rediscovered the hard way by reimplementing them wrongly first.
pub(crate) fn bind(
    lua: &mlua::Lua,
    source: PluginSource,
    budget: Duration,
) -> mlua::Result<Option<Bound>> {
    match source {
        PluginSource::FirstParty => Ok(None),
        PluginSource::Registry => {
            let bound = Bound::new(budget);
            install_bounds(lua, &bound.handle)?;
            Ok(Some(bound))
        }
    }
}

#[cfg(test)]
mod tests;
