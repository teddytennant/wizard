//! The plugin kernel: the registries, the event bus, and the plugin graph.
//!
//! Wizard is a plugin host. `docs/plugins.md` is the design; this is Phase 1 of
//! it — the kernel itself, built and proven alone before anything is ported
//! into it. **Nothing in the tree calls into here yet, and that is deliberate.**
//! The rule the migration runs under is that the kernel is additive and
//! dormant: it can be wrong without breaking a session, and it gets a phase of
//! its own to be wrong in.
//!
//! What it is, in one paragraph. A plugin — Rust or Lua, the kernel cannot tell
//! them apart — is handed a [`Ctx`] and registers against it: tools, slash
//! commands, providers, event handlers, services. Every one of those
//! registrations is written into that plugin's ledger, and unloading it drops
//! all of them in one step ([`lifecycle`]). Handlers subscribe to lifecycle
//! events through an async bus that lets them observe, rewrite the payload, or
//! veto, and a handler that errors or panics is logged and skipped ([`bus`]).
//! Plugins reach each other by name through `provide`/`inject`, where `inject`
//! returning `None` is the composability rule rather than a failure
//! ([`services`]). Lua plugins get one long-lived VM each, created at load and
//! dropped at unload, with the existing `src/tools/lua.rs` sandbox reused
//! rather than reimplemented ([`lua`]).
//!
//! # The three things worth knowing before reading further
//!
//! **The `Ctx` shape is identical from Rust and from Lua.** Not similar — the
//! same ten calls, with the same meanings, so a plugin can be ported between
//! the two languages without being redesigned. Every divergence is a bug, and
//! `ctx.rs` carries the list of the two places the languages genuinely cannot
//! meet (a native service is invisible to Lua; a Lua teardown runs inside its
//! own VM).
//!
//! **Names are owned.** A tool, command or provider name may be held by exactly
//! one plugin, and a second plugin claiming it is refused at load rather than
//! shadowing it. Services are the exception and may be overridden, because
//! overriding a service is a thing plugins legitimately do and because
//! [`ServiceRegistry::withdraw_owned`] makes the override survive its
//! predecessor's unload.
//!
//! **The kernel owns its own registries.** It does not reach into
//! [`crate::tools::registry::ToolRegistry`]; it holds plugin tools separately
//! and [`Kernel::install_tools_into`] is the one-way bridge a later phase will
//! use. That keeps this module additive: a bug in here cannot deregister a
//! native tool, because it never had one.

pub mod bus;
pub mod ctx;
pub mod lifecycle;
pub mod lua;
pub mod manifest;
pub mod services;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;

use crate::llm::registry::{self, ProviderDescriptor};
use crate::tools::Tool;
use crate::tools::registry::ToolRegistry;

pub use crate::commands::{CommandHandler, PluginCommand};
pub use bus::{Dispatch, Event, EventBus, EventHandler, HandlerId, Verdict};
pub use ctx::Ctx;
pub use lifecycle::{DisposalReport, Effect, Ledger, LoadedPlugin, PluginId, PluginKind};
pub use manifest::{Capability, CapabilitySet, PluginManifest, PluginSource};
pub use services::{Service, ServiceRef, ServiceRegistry};

/// A Rust plugin.
///
/// Compiled in behind a cargo feature named after the plugin. The trait is two
/// methods because a plugin has exactly two jobs: say what it needs, and
/// register. Anything a plugin wants to keep — a store, a socket, a handle to
/// a service it injected — lives in the implementor, and anything the kernel
/// has to undo goes through the [`Ctx`].
///
/// `apply` takes `&self` rather than `self` so the same plugin value can be
/// applied again after an unload, which is what makes reload cheap.
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;
    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()>;
}

/// Host facilities a plugin reaches through `wizard.*`, behind the capability
/// that names each one.
///
/// A trait rather than direct calls into `crate::tools::web` and friends
/// because of the dormancy rule at the top of this file: the kernel must be
/// wireable without being wired. [`UnwiredHost`] is the default and refuses
/// everything with a message that says why, which is the honest behaviour for
/// Phase 1 — a plugin that calls `wizard.http.get` today gets a clear error
/// rather than a silent success, and the day the transport is attached the
/// plugin does not change.
///
/// Every method is `async` because every one of them is I/O, and because the
/// whole reason `mlua`'s `async` feature is on is that a plugin has to be able
/// to await these from straight-line Lua.
#[async_trait::async_trait]
pub trait HostBridge: Send + Sync {
    /// Fetch a URL. Gated on [`Capability::Network`].
    async fn http(&self, method: &str, url: &str, body: Option<String>) -> anyhow::Result<String>;
    /// One completion, billed to the user and attributed to `plugin`. Gated on
    /// [`Capability::Model`].
    async fn model(&self, plugin: &str, prompt: &str) -> anyhow::Result<String>;
    /// Write a line to the transcript. Gated on [`Capability::Ui`].
    async fn notify(&self, plugin: &str, text: &str) -> anyhow::Result<()>;
    /// Start a subagent and wait for it. Gated on [`Capability::Agent`].
    async fn spawn_agent(&self, plugin: &str, task: &str) -> anyhow::Result<String>;
    /// Run a command. Gated on [`Capability::Process`].
    async fn run(&self, plugin: &str, command: &str) -> anyhow::Result<String>;
}

/// The bridge every kernel gets until a surface attaches a real one.
///
/// It refuses rather than no-ops. A no-op `notify` looks like a working plugin
/// that nobody can hear, and a no-op `http` returning an empty body looks like
/// a web page that is blank — both of which cost somebody an afternoon.
pub struct UnwiredHost;

#[async_trait::async_trait]
impl HostBridge for UnwiredHost {
    async fn http(
        &self,
        _method: &str,
        url: &str,
        _body: Option<String>,
    ) -> anyhow::Result<String> {
        Err(unwired("wizard.http", &format!("fetching {url}")))
    }

    async fn model(&self, _plugin: &str, _prompt: &str) -> anyhow::Result<String> {
        Err(unwired("wizard.model", "a completion"))
    }

    async fn notify(&self, _plugin: &str, _text: &str) -> anyhow::Result<()> {
        Err(unwired("wizard.ui", "a transcript write"))
    }

    async fn spawn_agent(&self, _plugin: &str, _task: &str) -> anyhow::Result<String> {
        Err(unwired("wizard.agent", "a subagent"))
    }

    async fn run(&self, _plugin: &str, command: &str) -> anyhow::Result<String> {
        Err(unwired("wizard.process", &format!("running {command}")))
    }
}

fn unwired(table: &str, what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{table} is not wired to a host in this build, so {what} cannot be carried out. \
         The plugin kernel is present but dormant; see docs/plugins.md."
    )
}

/// A registration and the plugin that made it.
struct Owned<T> {
    owner: PluginId,
    value: T,
}

/// The registries a `Ctx` writes into and an unload sweeps.
///
/// One struct rather than five fields on the kernel so [`lifecycle::dispose`]
/// can be handed the whole set without a reference to the kernel — which is
/// what keeps the lock discipline visible: `dispose` never touches the plugin
/// map, so it can never deadlock against the caller that is holding it.
pub(crate) struct Slots {
    pub(crate) bus: EventBus,
    pub(crate) services: ServiceRegistry,
    tools: Mutex<HashMap<String, Owned<Arc<dyn Tool>>>>,
    commands: Mutex<HashMap<String, Owned<PluginCommand>>>,
    providers: Mutex<HashMap<String, Owned<ProviderDescriptor>>>,
}

impl Slots {
    fn new() -> Self {
        Self {
            bus: EventBus::new(),
            services: ServiceRegistry::new(),
            tools: Mutex::new(HashMap::new()),
            commands: Mutex::new(HashMap::new()),
            providers: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn insert_tool(
        &self,
        owner: &PluginId,
        tool: Arc<dyn Tool>,
    ) -> Result<String, KernelError> {
        let name = tool.name().to_string();
        let mut map = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        claim(&mut map, "tool", owner, name.clone(), tool)?;
        Ok(name)
    }

    /// Register a slash command, in both places it has to exist.
    ///
    /// The second registration whose *consumer* is not the kernel, and it goes
    /// the same way [`Slots::insert_provider`] does and for the same reason.
    /// (This is also the second place the "additive and dormant" rule at the
    /// top of this file is narrower than it reads: the palette merges the
    /// registry on every keystroke, and it is empty because nothing loads a
    /// plugin yet, not because nothing reads it.)
    /// `SlashCommand::parse` is called from `App::submit`, from the window's
    /// `route`, from the gateway's `apply_command` — none of which hold a
    /// kernel handle — so the palette resolves a `/name` against the
    /// process-wide [`crate::commands::plugin`] registry. Written and swept
    /// here together with this kernel's slot rather than by a later publish
    /// step, because a publish step is a window in which an unloaded plugin's
    /// command still resolves.
    pub(crate) fn insert_command(
        &self,
        owner: &PluginId,
        command: PluginCommand,
    ) -> Result<String, KernelError> {
        let name = command.name.clone();
        let mut map = self.commands.lock().unwrap_or_else(PoisonError::into_inner);
        claim(&mut map, "command", owner, name.clone(), command.clone())?;
        // The global can refuse where the slot did not: a built-in owns the
        // name, or another kernel's plugin took it. Roll the slot back so the
        // two never disagree about who owns a name.
        if let Err(taken) = crate::commands::plugin::install(owner.as_str(), command) {
            map.remove(&name);
            return Err(KernelError::NameTaken {
                kind: "command",
                name: taken.name,
                holder: taken.holder,
                claimant: owner.to_string(),
            });
        }
        Ok(name)
    }

    /// Register a provider, in both places it has to exist.
    ///
    /// A provider was the first registration whose *consumer* is not the kernel
    /// ([`Slots::insert_command`] is the other, and followed this one).
    /// A tool is read out of the kernel and copied into the agent's registry;
    /// a provider is read out of `config.toml`, by `ProviderConfig::build`,
    /// which runs in places that hold no kernel handle at all (unit tests,
    /// `wizard doctor`, the settings sheet's probe). So it lands in the
    /// process-wide [`registry`] as well as in this kernel's slot, and the two
    /// are written and swept together here rather than by a separate publish
    /// step — a publish step is a window in which an unloaded plugin's
    /// provider is still selectable, and "unload has to be exact" is the
    /// reason there is a kernel.
    pub(crate) fn insert_provider(
        &self,
        owner: &PluginId,
        descriptor: ProviderDescriptor,
    ) -> Result<String, KernelError> {
        // The kind is the name, taken from the descriptor rather than passed
        // alongside it, for the same reason a tool's name comes off the tool:
        // a plugin must not be able to register under one id and answer to
        // another.
        let name = descriptor.kind().to_string();
        let mut map = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        claim(
            &mut map,
            "provider",
            owner,
            name.clone(),
            descriptor.clone(),
        )?;
        // The global can refuse where the slot did not: a built-in already
        // holds the kind, or another kernel does. Roll the slot back so the
        // two never disagree about who owns a name.
        if let Err(taken) = registry::install(descriptor) {
            map.remove(&name);
            return Err(KernelError::NameTaken {
                kind: "provider",
                name: taken.kind.to_string(),
                holder: "a provider registered outside this kernel".to_string(),
                claimant: owner.to_string(),
            });
        }
        Ok(name)
    }

    pub(crate) fn remove_tools(&self, names: &[String]) -> usize {
        let mut map = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        names
            .iter()
            .filter(|name| map.remove(*name).is_some())
            .count()
    }

    pub(crate) fn remove_commands(&self, names: &[String]) -> usize {
        let mut map = self.commands.lock().unwrap_or_else(PoisonError::into_inner);
        names
            .iter()
            .filter(|name| {
                // Only names this kernel actually holds are withdrawn from the
                // process registry, for the same reason as
                // [`Slots::remove_providers`]: sweeping by name alone would let
                // a plugin that failed to claim `/deploy` uninstall the one
                // that holds it on its way out.
                let owned = map.remove(*name).is_some();
                if owned {
                    crate::commands::plugin::uninstall(name);
                }
                owned
            })
            .count()
    }

    pub(crate) fn remove_providers(&self, names: &[String]) -> usize {
        let mut map = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        names
            .iter()
            .filter(|name| {
                // Only kinds this kernel actually holds are withdrawn from the
                // process registry. Sweeping by name alone would let a plugin
                // that failed to claim `anthropic` still uninstall the
                // built-in one on its way out.
                let owned = map.remove(*name).is_some();
                if owned {
                    registry::uninstall(&crate::llm::registry::ProviderKind::new(name.as_str()));
                }
                owned
            })
            .count()
    }
}

/// Take `name` for `owner`, or refuse because somebody else has it.
///
/// Refusing rather than shadowing, because the three things this guards are
/// all *named by a user or a model*: a tool the model calls, a command a user
/// types, a provider a config file selects. Two plugins quietly answering to
/// one of those is a bug report about the wrong plugin.
fn claim<T>(
    map: &mut HashMap<String, Owned<T>>,
    kind: &'static str,
    owner: &PluginId,
    name: String,
    value: T,
) -> Result<(), KernelError> {
    if let Some(existing) = map.get(&name) {
        return Err(KernelError::NameTaken {
            kind,
            name,
            holder: existing.owner.to_string(),
            claimant: owner.to_string(),
        });
    }
    map.insert(
        name,
        Owned {
            owner: owner.clone(),
            value,
        },
    );
    Ok(())
}

/// Why the kernel refused something.
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("plugin '{0}' is already loaded")]
    AlreadyLoaded(String),
    #[error("no plugin named '{0}' is loaded")]
    NotLoaded(String),
    #[error(
        "{kind} '{name}' is already registered by plugin '{holder}'; '{claimant}' cannot take it"
    )]
    NameTaken {
        kind: &'static str,
        name: String,
        holder: String,
        claimant: String,
    },
    #[error("plugin '{plugin}' needs the '{capability}' capability to {action}")]
    Denied {
        plugin: String,
        capability: Capability,
        action: String,
    },
    #[error(transparent)]
    Manifest(#[from] manifest::ManifestError),
    #[error("plugin '{plugin}' failed to load: {source}")]
    Apply {
        plugin: String,
        #[source]
        source: anyhow::Error,
    },
}

/// How a kernel is set up. Everything has a default so a test can say
/// `Kernel::new(KernelOptions::default())` and a surface can fill in only what
/// it has.
pub struct KernelOptions {
    /// Project root. Confines a sandboxed plugin's file helpers, exactly as it
    /// confines a registry-installed scripted tool's.
    pub project_root: PathBuf,
    /// The `[plugins]` table from `config.toml`, as JSON: a map from plugin
    /// name to that plugin's slice. What `ctx:config()` hands back.
    pub config: Value,
    /// Where Lua plugin directories live. `ctx:plugin(name)` resolves a child
    /// under it, and nothing may escape it — see `lua::host::plugin_fn`.
    pub plugin_root: PathBuf,
    /// Where `wizard.http`, `wizard.model`, `wizard.ui`, `wizard.agent` and
    /// `wizard.process` land.
    pub host: Arc<dyn HostBridge>,
    /// Compute a *bounded* plugin may spend inside one call before the
    /// in-VM hook stops it. Ignored for an unbounded (first-party) plugin,
    /// which has no hook at all.
    pub call_budget: std::time::Duration,
}

impl Default for KernelOptions {
    fn default() -> Self {
        Self {
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            config: Value::Object(serde_json::Map::new()),
            plugin_root: crate::config::Config::wizard_dir()
                .unwrap_or_else(|_| PathBuf::from(".wizard"))
                .join("plugins"),
            host: Arc::new(UnwiredHost),
            call_budget: lua::DEFAULT_CALL_BUDGET,
        }
    }
}

struct KernelInner {
    slots: Slots,
    plugins: Mutex<HashMap<PluginId, LoadedPlugin>>,
    /// Load order, so unloading everything happens newest-first and a plugin
    /// never outlives something it injected from.
    order: Mutex<Vec<PluginId>>,
    config: Mutex<Value>,
    project_root: PathBuf,
    plugin_root: PathBuf,
    host: Arc<dyn HostBridge>,
    call_budget: std::time::Duration,
}

/// The kernel. Cheap to clone; every clone is the same kernel.
#[derive(Clone)]
pub struct Kernel {
    inner: Arc<KernelInner>,
}

impl Default for Kernel {
    fn default() -> Self {
        Kernel::new(KernelOptions::default())
    }
}

impl Kernel {
    pub fn new(options: KernelOptions) -> Self {
        Kernel {
            inner: Arc::new(KernelInner {
                slots: Slots::new(),
                plugins: Mutex::new(HashMap::new()),
                order: Mutex::new(Vec::new()),
                config: Mutex::new(options.config),
                project_root: options.project_root,
                plugin_root: options.plugin_root,
                host: options.host,
                call_budget: options.call_budget,
            }),
        }
    }

    pub fn bus(&self) -> &EventBus {
        &self.inner.slots.bus
    }

    pub fn services(&self) -> &ServiceRegistry {
        &self.inner.slots.services
    }

    pub fn project_root(&self) -> &Path {
        &self.inner.project_root
    }

    pub fn plugin_root(&self) -> &Path {
        &self.inner.plugin_root
    }

    pub fn call_budget(&self) -> std::time::Duration {
        self.inner.call_budget
    }

    /// Load a Lua plugin from a directory holding `manifest.toml` and
    /// `plugin.lua`.
    pub async fn load_lua(
        &self,
        dir: &Path,
        source: PluginSource,
    ) -> Result<PluginId, KernelError> {
        lua::load_dir(self, dir, source, None, None).await
    }

    pub fn host(&self) -> Arc<dyn HostBridge> {
        Arc::clone(&self.inner.host)
    }

    pub(crate) fn slots(&self) -> &Slots {
        &self.inner.slots
    }

    /// One plugin's slice of `config.toml`, or `null` when it has none.
    pub fn config_for(&self, plugin: &str) -> Value {
        let config = self
            .inner
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        config.get(plugin).cloned().unwrap_or(Value::Null)
    }

    /// Replace the whole plugin config table. Emits nothing on its own — the
    /// caller decides whether this is a [`Event::ConfigReload`], because the
    /// same call is used to seed a kernel at startup.
    pub fn set_config(&self, config: Value) {
        *self
            .inner
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = config;
    }

    /// Publish an event to every subscriber, in order.
    pub async fn emit(&self, event: Event, payload: Value) -> Dispatch {
        self.inner.slots.bus.emit(event, payload).await
    }

    /// Load a Rust plugin.
    ///
    /// A failed `apply` is disposed rather than left behind: a plugin that
    /// registered two tools and then errored on the third would otherwise
    /// leave the two behind under a plugin that is not loaded, which is
    /// residue by another name.
    pub fn load(&self, plugin: Arc<dyn Plugin>) -> Result<PluginId, KernelError> {
        self.load_with_source(plugin, PluginSource::FirstParty, None, None)
    }

    pub(crate) fn load_with_source(
        &self,
        plugin: Arc<dyn Plugin>,
        source: PluginSource,
        parent: Option<PluginId>,
        config: Option<Value>,
    ) -> Result<PluginId, KernelError> {
        let manifest = plugin.manifest().clone();
        manifest.validate()?;
        let id = PluginId::new(&manifest.name);
        self.reserve(&id)?;

        let manifest = Arc::new(manifest);
        let mut ctx = self.context(&id, Arc::clone(&manifest), config);
        let outcome = plugin.apply(&mut ctx);
        let ledger = ctx.into_ledger();

        match outcome {
            Ok(()) => {
                self.finish_load(LoadedPlugin {
                    id: id.clone(),
                    manifest,
                    source,
                    parent,
                    kind: PluginKind::Rust(plugin),
                    ledger,
                });
                Ok(id)
            }
            Err(err) => {
                lifecycle::dispose(&self.inner.slots, &id, ledger, |_| None);
                self.release(&id);
                Err(KernelError::Apply {
                    plugin: id.to_string(),
                    source: err,
                })
            }
        }
    }

    /// A [`Ctx`] for `id`. `pub(crate)` because a `Ctx` that nothing is going
    /// to record a ledger from is a leak generator.
    pub(crate) fn context(
        &self,
        id: &PluginId,
        manifest: Arc<PluginManifest>,
        config: Option<Value>,
    ) -> Ctx {
        let caps = manifest.capability_set();
        let config = config.unwrap_or_else(|| self.config_for(id.as_str()));
        Ctx::new(self.clone(), id.clone(), manifest, caps, config)
    }

    /// Take a name, or refuse because it is taken. Separate from the rest of
    /// the load so a Lua load can hold the name across the async VM start.
    pub(crate) fn reserve(&self, id: &PluginId) -> Result<(), KernelError> {
        let mut order = self
            .inner
            .order
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if order.contains(id) {
            return Err(KernelError::AlreadyLoaded(id.to_string()));
        }
        order.push(id.clone());
        Ok(())
    }

    /// Give a reserved name back, for a load that did not finish.
    pub(crate) fn release(&self, id: &PluginId) {
        let mut order = self
            .inner
            .order
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        order.retain(|loaded| loaded != id);
    }

    pub(crate) fn finish_load(&self, loaded: LoadedPlugin) {
        let id = loaded.id.clone();
        self.inner
            .plugins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, loaded);
    }

    /// Unload one plugin and everything it registered, children included.
    ///
    /// Async because a Lua plugin's VM is a task and its teardowns run inside
    /// it. A Rust plugin's unload never awaits anything, so this is a cheap
    /// call for those.
    pub async fn unload(&self, id: &PluginId) -> Result<DisposalReport, KernelError> {
        let loaded = self
            .inner
            .plugins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .ok_or_else(|| KernelError::NotLoaded(id.to_string()))?;
        self.release(id);

        // Children are taken out of the map here, synchronously, so nothing
        // can load a plugin under a child's name while the parent is still
        // disposing. Their VMs are shut down below with the parent's.
        //
        // The `remove` is in a block of its own so the plugin-map guard is
        // dropped before `release` takes the order lock. `loaded()` takes them
        // the other way round, and two call sites that disagree about the
        // order of two locks is a deadlock that reproduces once a month on the
        // machine that is busiest.
        let mut orphans = Vec::new();
        for child in loaded.ledger.children() {
            let taken = {
                let mut plugins = self
                    .inner
                    .plugins
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                plugins.remove(child)
            };
            if let Some(child) = taken {
                self.release(&child.id);
                orphans.push(child);
            }
        }

        let LoadedPlugin { ledger, kind, .. } = loaded;
        let mut child_reports: HashMap<PluginId, DisposalReport> = HashMap::new();
        let mut vms = Vec::new();
        for orphan in orphans {
            let LoadedPlugin {
                id: child_id,
                ledger,
                kind,
                ..
            } = orphan;
            let report = lifecycle::dispose(&self.inner.slots, &child_id, ledger, |_| None);
            if let PluginKind::Lua(vm) = kind {
                vms.push(vm);
            }
            child_reports.insert(child_id, report);
        }

        let mut report = lifecycle::dispose(&self.inner.slots, id, ledger, |child| {
            child_reports.remove(child)
        });
        if let PluginKind::Lua(vm) = kind {
            vms.push(vm);
        }

        for vm in vms {
            let shutdown = vm.shutdown().await;
            report.effects += shutdown.effects;
            report.effect_failures.extend(shutdown.failures);
        }

        tracing::debug!(plugin = %id, removed = report.total(), "plugin unloaded");
        Ok(report)
    }

    /// Unload everything, newest first.
    pub async fn unload_all(&self) -> Vec<DisposalReport> {
        let mut reports = Vec::new();
        loop {
            let next = {
                let order = self
                    .inner
                    .order
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                order.last().cloned()
            };
            let Some(id) = next else { break };
            match self.unload(&id).await {
                Ok(report) => reports.push(report),
                // A name in `order` with nothing in `plugins` is a reservation
                // for a load still in flight. Dropping it is the only way this
                // loop can terminate.
                Err(_) => self.release(&id),
            }
        }
        reports
    }

    /// Unload then load again, so a reload is exactly a fresh start.
    pub async fn reload(&self, plugin: Arc<dyn Plugin>) -> Result<PluginId, KernelError> {
        let id = PluginId::new(&plugin.manifest().name);
        if self.is_loaded(&id) {
            self.unload(&id).await?;
        }
        self.load(plugin)
    }

    pub fn is_loaded(&self, id: &PluginId) -> bool {
        self.inner
            .plugins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(id)
    }

    /// Loaded plugins, in load order.
    pub fn loaded(&self) -> Vec<PluginId> {
        // Copied out rather than held, so this never holds the order lock and
        // the plugin lock at the same time. A name in `order` with nothing in
        // `plugins` is a load still in flight and is filtered out.
        let order: Vec<PluginId> = self
            .inner
            .order
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let plugins = self
            .inner
            .plugins
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        order
            .into_iter()
            .filter(|id| plugins.contains_key(id))
            .collect()
    }

    pub fn manifest_of(&self, id: &PluginId) -> Option<Arc<PluginManifest>> {
        self.inner
            .plugins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|loaded| Arc::clone(&loaded.manifest))
    }

    /// A plugin-registered tool by name.
    pub fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.inner
            .slots
            .tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|owned| Arc::clone(&owned.value))
    }

    /// Every plugin-registered tool, sorted by name so a `/tools` listing does
    /// not reorder itself between runs.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let map = self
            .inner
            .slots
            .tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut tools: Vec<(&String, Arc<dyn Tool>)> = map
            .iter()
            .map(|(name, owned)| (name, Arc::clone(&owned.value)))
            .collect();
        tools.sort_by(|a, b| a.0.cmp(b.0));
        tools.into_iter().map(|(_, tool)| tool).collect()
    }

    pub fn tool_names(&self) -> Vec<String> {
        sorted_keys(&self.inner.slots.tools)
    }

    pub fn command(&self, name: &str) -> Option<PluginCommand> {
        self.inner
            .slots
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|owned| owned.value.clone())
    }

    pub fn command_names(&self) -> Vec<String> {
        sorted_keys(&self.inner.slots.commands)
    }

    pub fn provider(&self, name: &str) -> Option<ProviderDescriptor> {
        self.inner
            .slots
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|owned| owned.value.clone())
    }

    pub fn provider_names(&self) -> Vec<String> {
        sorted_keys(&self.inner.slots.providers)
    }

    /// Copy every plugin tool into a [`ToolRegistry`], and say how many went.
    ///
    /// The one-way bridge between the kernel's registries and the agent's. A
    /// copy rather than a shared handle on purpose: the agent's registry is
    /// snapshotted per turn, and a kernel that could deregister a tool
    /// mid-turn would be a source of "unknown tool" errors that only reproduce
    /// under a concurrent unload.
    pub fn install_tools_into(&self, registry: &mut ToolRegistry) -> usize {
        let tools = self.tools();
        let count = tools.len();
        for tool in tools {
            registry.register(tool);
        }
        count
    }

    /// Everything registered across every plugin. The number an exact-disposal
    /// test asserts is zero.
    pub fn residue(&self) -> Residue {
        Residue {
            plugins: self.loaded().len(),
            tools: self
                .inner
                .slots
                .tools
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
            commands: self
                .inner
                .slots
                .commands
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
            providers: self
                .inner
                .slots
                .providers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
            handlers: self.inner.slots.bus.len(),
            services: self.inner.slots.services.len(),
        }
    }
}

fn sorted_keys<T>(map: &Mutex<HashMap<String, Owned<T>>>) -> Vec<String> {
    let map = map.lock().unwrap_or_else(PoisonError::into_inner);
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    names
}

/// What is still registered. `Residue::default()` is what a kernel looks like
/// after everything it ever loaded has been unloaded, and saying so as one
/// value means a test asserts the whole thing rather than the four fields
/// somebody remembered.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Residue {
    pub plugins: usize,
    pub tools: usize,
    pub commands: usize,
    pub providers: usize,
    pub handlers: usize,
    pub services: usize,
}

impl Residue {
    pub fn is_empty(&self) -> bool {
        *self == Residue::default()
    }
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
