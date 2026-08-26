//! JavaScript plugins: one long-lived QuickJS VM each, on a task of its own.
//!
//! The third plugin backend and a peer of [`super::lua`], not a replacement
//! for it. Everything structural here is that module's, deliberately: the same
//! `VmHandle`/`VmRequest` channel, the same `FuturesUnordered` loop, the same
//! per-call deadline that is armed on entry and parked when the VM goes idle,
//! the same `Drop` that aborts the task. Reading the two side by side should be
//! boring. Where they differ, they differ because the engines differ, and each
//! of those places says so.
//!
//! # Why QuickJS, and not V8
//!
//! The whole argument is size. `--no-default-features` exists because a build
//! that leaves plugins out is supposed to be *smaller*, and a backend that
//! costs 40 MB whether or not anybody writes a plugin in it would make that
//! claim untrue for every stock binary. QuickJS is one C file and an
//! interpreter; a `deno_core`/V8 embedding is a JIT, a snapshot and a garbage
//! collector tuned for a browser tab. The measurement is in `docs/plugins.md`.
//!
//! The second reason is the sandbox. A subprocess `node` would be simpler to
//! wire and would put the capability model on the wrong side of a process
//! boundary: `wizard.fs` confined to the project root means nothing if the
//! plugin is a separate process with the user's own file permissions. An
//! in-process VM with no filesystem, no network and no module loader is the
//! only shape in which "a capability a plugin did not declare is absent" is a
//! statement about what the code *can* do rather than about what it is asked
//! to do.
//!
//! # The bound is an interrupt handler, and it is uncatchable
//!
//! LuaJIT needed three rediscovered details to bound an async chunk (see
//! [`super::lua`]). QuickJS needs one call —
//! [`AsyncRuntime::set_interrupt_handler`] — and gives a stronger guarantee
//! than Lua's for free: the handler makes the interpreter raise an
//! *uncatchable* error, so `try { while(true){} } catch {}` stops on the
//! deadline where the Lua equivalent needed `install_stop_guard` to survive a
//! `pcall`. Verified three ways in [`tests`]: a bare spin, a spin placed after
//! an `await`, and a spin inside a `try`.
//!
//! Two things do carry over unchanged. The deadline is per *call*, armed by
//! [`Bound::arm`] and parked by [`Bound::relax`], because a lifetime deadline
//! on a plugin loaded at 09:00 would kill it at 09:00:30. And the flag latches
//! while a call is being torn down, so it is cleared when the VM goes idle —
//! otherwise the first plugin to time out would be dead for the rest of the
//! session.
//!
//! # There is no JIT to lose
//!
//! `PluginSource::FirstParty` skips the interrupt handler exactly as it skips
//! Lua's instruction hook, and for the same reason: first-party code is code
//! this repository shipped. But the *cost* is different and smaller. In Lua a
//! bound means `jit.off()`, so a bounded plugin is interpreted and gives up
//! the compiler. QuickJS is an interpreter either way, so what a bounded JS
//! plugin pays is one function call every few thousand bytecodes and nothing
//! else. The trade `docs/plugins.md` records for Lua does not exist here.

pub mod convert;
pub mod host;

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use rquickjs::{AsyncContext, AsyncRuntime};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::lifecycle::PluginId;
use super::manifest::{PluginManifest, PluginSource};
use super::{Kernel, KernelError, PluginKind, VmShutdown};

/// A JavaScript function the host holds a handle on, by number.
///
/// Numbers rather than `rquickjs::Persistent` handles at the call site, for
/// the reason [`super::lua::FnId`] gives: the thing that calls back in is a
/// `Tool` sitting in the agent's registry, and it must be able to name a
/// function without holding anything that borrows a VM.
pub type FnId = u64;

/// Memory a bounded plugin's VM may hold.
///
/// The same 64 MB the Lua backend and the sandboxed scripted tools use. The
/// number is not derived from anything about JavaScript; keeping the two
/// backends on one figure is the point, because "how much may a plugin hold"
/// is a question about plugins rather than about engines.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// How long an unload waits for a VM to run its teardowns before abandoning it.
///
/// [`super::lua`]'s figure and its argument: the VM being wedged is exactly
/// the state an unload is most likely to be called in, and an unload that can
/// hang is worse than a teardown that does not run.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Why a call stopped, when it did not stop by returning.
///
/// The flag and never the message text, which is the same reasoning
/// [`crate::tools::lua::StopReason`] exists for: QuickJS renders a deadline
/// stop as `InternalError: interrupted`, and a plugin can `throw new
/// Error("interrupted")` on purpose. Only the host knows which happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    None = 0,
    Time = 1,
}

impl StopReason {
    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => StopReason::Time,
            _ => StopReason::None,
        }
    }
}

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
    /// Call a registered JavaScript function and convert the result to JSON.
    ///
    /// Every failure mode is an `Err` and none of them is a hang: a dead task
    /// closes the channel, a dropped reply closes the oneshot, and a thrown
    /// value comes back as its message.
    pub async fn call(&self, func: FnId, args: Vec<Value>) -> anyhow::Result<Value> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(VmRequest::Invoke { func, args, reply })
            .await
            .map_err(|_| anyhow::anyhow!("the JS VM for plugin '{}' is gone", self.plugin))?;
        answer.await.map_err(|_| {
            anyhow::anyhow!(
                "the JS VM for plugin '{}' dropped a call without answering",
                self.plugin
            )
        })?
    }

    pub fn plugin(&self) -> &str {
        &self.plugin
    }
}

/// A loaded JavaScript plugin, from the kernel's side. Dropping it stops the VM.
pub struct JsPlugin {
    handle: VmHandle,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl JsPlugin {
    pub fn handle(&self) -> &VmHandle {
        &self.handle
    }

    /// Run the plugin's JavaScript teardowns and stop the VM.
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
                    "a JS plugin did not finish its teardowns in time; abandoning its VM"
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

impl Drop for JsPlugin {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Aborting rather than waiting, for [`super::lua::LuaPlugin`]'s
            // reason: `Drop` cannot await, and a VM spinning inside JavaScript
            // would never reach a yield point for the task to be cancelled at.
            // The abort is what guarantees the VM is gone once the plugin
            // record is dropped.
            task.abort();
        }
    }
}

/// The per-call bound: a deadline the interrupt handler reads, and a flag it
/// latches so the host can tell a stop from a `throw`.
pub(crate) struct Bound {
    deadline: Arc<Mutex<Instant>>,
    stop: Arc<AtomicU8>,
    budget: Duration,
}

impl Bound {
    fn new(budget: Duration) -> Self {
        Bound {
            deadline: Arc::new(Mutex::new(Instant::now() + budget)),
            stop: Arc::new(AtomicU8::new(StopReason::None as u8)),
            budget,
        }
    }

    /// Start a call's clock.
    fn arm(&self) {
        *self.deadline.lock().unwrap_or_else(PoisonError::into_inner) =
            Instant::now() + self.budget;
    }

    /// Park the clock while nothing is running, and un-latch the flag.
    ///
    /// The un-latch is what makes "a VM that had one call bounded is still
    /// usable for the next one" true. Without it, the first timeout would be
    /// the plugin's last call for the life of the session — and unlike Lua's
    /// stop guard, which re-raises, QuickJS would simply refuse to run
    /// anything, which reads as a plugin that stopped existing.
    fn relax(&self) {
        // Far enough out that no honest call reaches it, and still a real
        // instant rather than an `Option`, so the interrupt handler stays one
        // comparison. A VM that somehow ran for a year stops.
        *self.deadline.lock().unwrap_or_else(PoisonError::into_inner) =
            Instant::now() + Duration::from_secs(365 * 24 * 3600);
        self.stop.store(StopReason::None as u8, Ordering::SeqCst);
    }

    fn reason(&self) -> StopReason {
        StopReason::from_u8(self.stop.load(Ordering::SeqCst))
    }
}

/// Install the deadline on a VM that is going to be bounded, and the memory
/// ceiling that applies whether or not it is.
///
/// The split matches [`super::lua::bind`]: a first-party plugin is code this
/// repository shipped, so it runs with no interrupt handler at all. The memory
/// ceiling is *not* part of that trade — it is set for every VM, because 64 MB
/// is far more than any honest plugin holds and a first-party plugin that
/// allocated a gigabyte would be a bug rather than a privilege.
async fn bind(runtime: &AsyncRuntime, source: PluginSource, budget: Duration) -> Option<Bound> {
    runtime.set_memory_limit(MEMORY_LIMIT).await;
    match source {
        PluginSource::FirstParty => None,
        PluginSource::Registry => {
            let bound = Bound::new(budget);
            let deadline = Arc::clone(&bound.deadline);
            let stop = Arc::clone(&bound.stop);
            runtime
                .set_interrupt_handler(Some(Box::new(move || {
                    // Latched, so a stop reported by the handler is still
                    // readable once the exception has unwound out to Rust.
                    // Cleared by `relax` when the VM goes idle.
                    if stop.load(Ordering::SeqCst) != StopReason::None as u8 {
                        return true;
                    }
                    let expired = Instant::now()
                        > *deadline.lock().unwrap_or_else(PoisonError::into_inner);
                    if expired {
                        stop.store(StopReason::Time as u8, Ordering::SeqCst);
                    }
                    expired
                })))
                .await;
            Some(bound)
        }
    }
}

/// Load a JavaScript plugin from a directory holding `manifest.toml` and
/// `plugin.js`.
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

    let script_path = dir.join("plugin.js");
    let script = std::fs::read_to_string(&script_path).map_err(|err| KernelError::Apply {
        plugin: manifest.name.clone(),
        source: anyhow::anyhow!("reading {}: {err}", script_path.display()),
    })?;

    load_source(
        kernel,
        manifest,
        source,
        &script,
        &script_path.display().to_string(),
        parent,
        config,
    )
    .await
}

/// Load a JavaScript plugin from source already in hand.
///
/// `module_name` is what appears in a stack trace, so it should be a real path
/// when there is one. Unlike mlua's chunk names it takes no `@` prefix —
/// QuickJS uses the module name verbatim.
pub async fn load_source(
    kernel: &Kernel,
    manifest: PluginManifest,
    source: PluginSource,
    script: &str,
    module_name: &str,
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
        module_name: module_name.to_string(),
        source,
        rx,
        ready,
    }));

    let plugin = JsPlugin {
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
                kind: PluginKind::Js(plugin),
                ledger,
            });
            Ok(id)
        }
        Ok((ledger, Err(err))) => {
            // The VM took itself down; whatever it managed to register before
            // failing is disposed here so a failed load leaves nothing.
            // Children first, because a child is a plugin in its own right and
            // its own unload is the only thing that stops its VM.
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
                source: anyhow::anyhow!("the plugin's JS VM panicked while starting"),
            })
        }
    }
}

/// Everything the VM task needs, in one struct so the spawn is readable.
struct VmSetup {
    ctx: super::Ctx,
    handle: VmHandle,
    script: String,
    module_name: String,
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
        module_name,
        source,
        mut rx,
        ready,
    } = setup;

    let plugin = ctx.name().to_string();
    let state = match host::build(&ctx, &handle, source).await {
        Ok(state) => state,
        Err(err) => {
            let _ = ready.send((ctx.into_ledger(), Err(err)));
            return;
        }
    };

    if let Some(bound) = &state.bound {
        bound.arm();
    }
    let applied = host::apply(&state, &script, &module_name).await;
    if let Some(bound) = &state.bound {
        bound.relax();
    }

    // The ledger travels back whether `apply` succeeded or not, for
    // [`super::lua`]'s reason: a plugin that registered two tools and then
    // threw registered two tools, and the caller needs the record to take them
    // out again.
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

    tracing::debug!(plugin = %plugin, "a plugin's JS VM stopped");
    // Drop the persistent function handles before the context, so no saved
    // `Function` outlives the runtime it points into.
    host::clear_functions(&state);
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
    let call = host::call_function(state, func, args);
    let bound = state
        .bound
        .as_ref()
        .map(|bound| (Arc::clone(&bound.stop), bound.budget));
    async move {
        let answer = call.await.map_err(|err| {
            let (reason, budget) = bound
                .map(|(flag, budget)| (StopReason::from_u8(flag.load(Ordering::SeqCst)), budget))
                .unwrap_or((StopReason::None, Duration::ZERO));
            describe_failure(reason, budget, err)
        });
        let _ = reply.send(answer);
    }
}

/// Turn a failed call into an error a reader can act on.
///
/// The latched flag decides, never the message text. QuickJS renders a
/// deadline stop as `InternalError: interrupted` with a stack trace, which is
/// both unhelpful and forgeable — a plugin may throw exactly that string. The
/// memory ceiling has no arm here because QuickJS reports an allocation
/// failure as an ordinary `OutOfMemory` exception the plugin's own message
/// carries; there is no separate signal to latch and inventing one would mean
/// guessing from the text.
fn describe_failure(reason: StopReason, budget: Duration, err: anyhow::Error) -> anyhow::Error {
    match reason {
        StopReason::Time => anyhow::anyhow!(
            "the plugin exceeded its {}ms compute budget for one call",
            budget.as_millis()
        ),
        StopReason::None => err,
    }
}

/// Build a VM with the deadline and the ceiling on it.
///
/// Here rather than in [`host`] because it is the bound and not the API: what
/// `host` builds is the `wizard` and `ctx` objects, and what this builds is
/// the thing they live in.
pub(crate) async fn runtime_for(
    source: PluginSource,
    budget: Duration,
) -> anyhow::Result<(AsyncRuntime, AsyncContext, Option<Bound>)> {
    let runtime = AsyncRuntime::new()
        .map_err(|err| anyhow::anyhow!("creating the QuickJS runtime: {err}"))?;
    let bound = bind(&runtime, source, budget).await;
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|err| anyhow::anyhow!("creating the QuickJS context: {err}"))?;
    Ok((runtime, context, bound))
}

#[cfg(test)]
mod tests;
