//! `Ctx` — the whole plugin-facing API, and the only one.
//!
//! Ten calls, listed in `docs/plugins.md` and implemented here: `tool`,
//! `command`, `provider`, `on`, `emit`, `provide`, `inject`, `plugin`,
//! `effect`, `config`. A plugin gets one of these and nothing else. There is no
//! second, richer API for in-tree plugins, because the moment there is one the
//! Rust and Lua worlds stop being the same shape and a plugin stops being
//! portable between them.
//!
//! # Why every method takes `&self`
//!
//! [`Plugin::apply`](super::Plugin::apply) is handed `&mut Ctx`, matching the
//! spec, but the methods only need `&self`: the ledger is behind a mutex
//! because Lua's host functions capture a `Ctx` clone and register from inside
//! the VM, where there is no `&mut` to be had. One implementation for both
//! languages is worth an `Arc<Mutex<_>>` that is uncontended in practice — a
//! plugin registers from one place at a time — and it is what lets the Lua host
//! be a thin translation layer rather than a parallel implementation of
//! registration.
//!
//! # The two places the languages genuinely differ
//!
//! Everything else is identical. These two are not, and both are consequences
//! of what Lua can hold rather than choices:
//!
//! 1. **A native service is invisible to Lua.** `ctx:inject("web")` on a
//!    service some Rust plugin provided as an `Arc<dyn Trait>` returns `nil`,
//!    because Lua cannot call it. It is the same `nil` an absent service gives,
//!    so a Lua plugin's degrade path already covers the case.
//! 2. **A Lua teardown runs inside its own VM.** `ctx:effect(fn)` from Lua
//!    cannot become a Rust `FnOnce`, so it is recorded in the VM and run there
//!    during shutdown, after the registries are already clear. From Rust an
//!    effect is a closure in the ledger. The observable ordering is the same in
//!    both: registries first, teardowns last, teardowns in reverse.

use std::any::Any;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;

use crate::commands::PluginCommand;
use crate::llm::registry::ProviderDescriptor;
use crate::tools::Tool;

use super::bus::{Dispatch, Event, EventHandler, HandlerId};
use super::lifecycle::{Effect, Ledger, PluginId};
use super::manifest::{Capability, CapabilitySet, PluginManifest, PluginSource};
use super::services::{Service, ServiceRef};
use super::{HostBridge, Kernel, KernelError, Plugin};

/// The plugin-facing API.
///
/// Cloneable, and every clone writes into the same ledger — which is the point:
/// the Lua host holds clones inside host functions, and a registration made
/// through any of them is disposed with the plugin.
#[derive(Clone)]
pub struct Ctx {
    kernel: Kernel,
    plugin: PluginId,
    manifest: Arc<PluginManifest>,
    caps: CapabilitySet,
    config: Value,
    ledger: Arc<Mutex<Ledger>>,
}

impl Ctx {
    pub(crate) fn new(
        kernel: Kernel,
        plugin: PluginId,
        manifest: Arc<PluginManifest>,
        caps: CapabilitySet,
        config: Value,
    ) -> Self {
        Self {
            kernel,
            plugin,
            manifest,
            caps,
            config,
            ledger: Arc::new(Mutex::new(Ledger::new())),
        }
    }

    /// Take the ledger out at the end of `apply`.
    ///
    /// Every outstanding clone of this `Ctx` is left holding an empty ledger
    /// rather than a dangling one, so a host function that fires after the
    /// kernel has taken the record records into nothing and the registration
    /// is refused downstream instead of being silently unowned. In practice
    /// nothing calls a `Ctx` after its plugin has finished loading; this is
    /// what makes "in practice" not load-bearing.
    pub(crate) fn into_ledger(self) -> Ledger {
        let mut guard = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *guard)
    }

    pub fn id(&self) -> &PluginId {
        &self.plugin
    }

    pub fn name(&self) -> &str {
        self.plugin.as_str()
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.caps
    }

    pub fn kernel(&self) -> &Kernel {
        &self.kernel
    }

    pub fn host(&self) -> Arc<dyn HostBridge> {
        self.kernel.host()
    }

    /// Whether this plugin holds `cap`.
    pub fn has(&self, cap: Capability) -> bool {
        self.caps.contains(cap)
    }

    /// Refuse unless this plugin holds `cap`.
    ///
    /// `action` completes the sentence "needs the 'network' capability to ..."
    /// — it is read by a user deciding whether to grant, so it names what the
    /// plugin was trying to do and not which function it called.
    pub fn require(&self, cap: Capability, action: &str) -> Result<(), KernelError> {
        if self.has(cap) {
            return Ok(());
        }
        Err(KernelError::Denied {
            plugin: self.plugin.to_string(),
            capability: cap,
            action: action.to_string(),
        })
    }

    /// `ctx:tool(spec)` — register a tool the model can call.
    ///
    /// The name comes from the tool itself, so a plugin cannot register one
    /// name and answer to another.
    pub fn tool(&self, tool: Arc<dyn Tool>) -> Result<(), KernelError> {
        let name = self.kernel.slots().insert_tool(&self.plugin, tool)?;
        self.ledger().record_tool(name);
        Ok(())
    }

    /// `ctx:command(spec)` — register a slash command.
    ///
    /// A [`PluginCommand`] rather than a type of the kernel's own, for the same
    /// reason [`Ctx::provider`] takes a [`ProviderDescriptor`]: the consumer
    /// defines the shape. What a slash command *is* — a name, an argument hint,
    /// a description, which surfaces run it — is `src/commands/`'s question,
    /// and a second answer here would be a second thing for the palette to
    /// merge.
    ///
    /// Refuses a name a built-in owns or another plugin already registered; see
    /// [`crate::commands::plugin`] for why it refuses rather than shadows. The
    /// caller may discard the error and carry on — a warning is logged either
    /// way — which is the degrade path a plugin with a fallback name wants.
    pub fn command(&self, command: PluginCommand) -> Result<(), KernelError> {
        let name = self.kernel.slots().insert_command(&self.plugin, command)?;
        self.ledger().record_command(name);
        Ok(())
    }

    /// `ctx:provider(spec)` — register a backend `config.toml` can select.
    ///
    /// A [`ProviderDescriptor`] rather than a live
    /// [`LlmProvider`](crate::llm::provider::LlmProvider), which is what this
    /// took before and what made the call unreachable in practice: a provider
    /// instance is bound to one base URL, one model and one key, and every one
    /// of those comes out of the user's config. There was no way for a
    /// `kind = "..."` to name an instance somebody had already constructed. A
    /// descriptor is *how to build one from a config*, which is the thing the
    /// config side has always needed.
    ///
    /// Registering here records the descriptor against this plugin so an
    /// unload withdraws it. It becomes visible to `config.toml` when the
    /// kernel publishes it with
    /// [`Kernel::install_providers`](super::Kernel::install_providers).
    pub fn provider(&self, descriptor: ProviderDescriptor) -> Result<(), KernelError> {
        let name = self
            .kernel
            .slots()
            .insert_provider(&self.plugin, descriptor)?;
        self.ledger().record_provider(name);
        Ok(())
    }

    /// `ctx:on(event, handler, priority)` — subscribe.
    pub fn on(&self, event: Event, priority: i32, handler: Arc<dyn EventHandler>) -> HandlerId {
        let id = self
            .kernel
            .slots()
            .bus
            .subscribe(&self.plugin, event, priority, handler);
        self.ledger().record_handler(id);
        id
    }

    /// [`Ctx::on`] for a plain closure, which is what most Rust plugins want.
    pub fn on_fn<F, Fut>(&self, event: Event, priority: i32, handler: F) -> HandlerId
    where
        F: Fn(Event, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<super::Verdict>> + Send + 'static,
    {
        self.on(event, priority, Arc::new(handler))
    }

    /// `ctx:emit(event, payload)` — publish one.
    pub async fn emit(&self, event: Event, payload: Value) -> Dispatch {
        self.kernel.emit(event, payload).await
    }

    /// `ctx:provide(name, service)` — expose a service to other plugins.
    pub fn provide(&self, name: impl Into<String>, service: Service) {
        let name = name.into();
        self.kernel
            .slots()
            .services
            .provide(&self.plugin, name.clone(), service);
        self.ledger().record_service(name);
    }

    /// `ctx:inject(name)` — take a service, or `None` if absent.
    ///
    /// Never an error. See [`super::services`]: a plugin that treats a missing
    /// service as fatal has made itself a hard dependency, and hard
    /// dependencies are what the profile system exists to not have.
    pub fn inject(&self, name: &str) -> Option<Service> {
        self.kernel.slots().services.inject(name)
    }

    /// [`Ctx::inject`] plus the downcast.
    pub fn inject_as<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.kernel.slots().services.inject_as::<T>(name)
    }

    /// A handle that keeps resolving, so it goes `None` when the provider
    /// unloads. For a plugin that holds a service across turns.
    pub fn inject_ref(&self, name: impl Into<String>) -> ServiceRef {
        self.kernel.slots().services.reference(name)
    }

    /// `ctx:plugin(child, config)` — load a child plugin under this one.
    ///
    /// The child is disposed with the parent. `config` overrides the child's
    /// slice of `config.toml`, which is the point of the call: a plugin that
    /// composes another one usually wants to configure it rather than let the
    /// user do it twice.
    pub fn plugin(
        &self,
        child: Arc<dyn Plugin>,
        config: Option<Value>,
    ) -> Result<PluginId, KernelError> {
        let id = self.kernel.load_with_source(
            child,
            PluginSource::FirstParty,
            Some(self.plugin.clone()),
            config,
        )?;
        self.ledger().record_child(id.clone());
        Ok(id)
    }

    /// Record a child the Lua host loaded directly.
    ///
    /// `Ctx::plugin` does this for a Rust child; a Lua child is loaded through
    /// [`crate::kernel::lua::load_dir`], which returns an id rather than taking
    /// a `Ctx`, so the host records it here. Recording a child nothing loaded
    /// would arrange for it to be disposed twice, which is why this is not on
    /// the public surface.
    pub(crate) fn record_child(&self, id: PluginId) {
        self.ledger().record_child(id);
    }

    /// `ctx:effect(dispose)` — register a teardown.
    ///
    /// The escape hatch for state the kernel cannot see: an open socket, a temp
    /// directory, a child process. Runs after every registry entry is gone, in
    /// reverse registration order.
    pub fn effect(&self, label: impl Into<String>, dispose: impl FnOnce() + Send + 'static) {
        self.ledger().record_effect(Effect::new(label, dispose));
    }

    /// `ctx:config()` — this plugin's slice of `config.toml`. `null` when it
    /// has none, so a plugin reads defaults out of it rather than branching on
    /// presence.
    pub fn config(&self) -> &Value {
        &self.config
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("plugin", &self.plugin)
            .field("capabilities", &self.caps.to_string())
            .finish()
    }
}
