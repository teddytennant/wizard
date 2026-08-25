//! The one table that names every compiled-in plugin, and the process kernel
//! they load into.
//!
//! `docs/plugins.md` describes this file as "the one table mapping feature to
//! constructor; it is the only file that names every Rust plugin". That is
//! [`compiled_in`]. The rest of the module is the answer to the question the
//! kernel left open: **who calls `Kernel::load`, and when.**
//!
//! # One kernel per process, two halves, loaded at different moments
//!
//! There is exactly one kernel in a running Wizard, because there is exactly
//! one set of installed plugins per process — the same reason
//! [`crate::llm::registry`]'s `INSTALLED` is a global. `Kernel` itself stays
//! instantiable more than once (every kernel test makes its own); this module
//! owns the singleton.
//!
//! It boots in two halves, because the two kinds of plugin have different
//! costs and different callers:
//!
//! - **Rust plugins load lazily, on the first thing that needs them.** They
//!   are compiled in and their `apply` is a handful of map inserts, so there
//!   is nothing to defer and nothing to schedule: [`kernel`] loads them inside
//!   a `OnceLock`, and the first caller pays microseconds. Crucially this is
//!   *synchronous* and needs no tokio runtime, which is what lets
//!   [`crate::llm::registry`] reach it from `ProviderConfig::build` — a unit
//!   test, `wizard doctor`, the settings sheet's probe — none of which hold a
//!   kernel handle or, in some cases, a runtime.
//! - **Lua plugins load once, from [`boot`], at the top of [`crate::run`].**
//!   They are files: a `read_dir`, then a VM and a script per plugin. That is
//!   real work, it is async, and it must happen exactly once for the process
//!   rather than on whichever code path happened to touch a plugin first.
//!
//! Launch cost of the whole thing on a machine with no plugins installed is
//! one `read_dir` that returns `ENOENT`. With plugins installed it is one
//! LuaJIT VM per plugin, which is the cost the user asked for by installing
//! them.
//!
//! # Why [`boot`] is called from `run` and not from a surface
//!
//! `src/lib.rs`'s dispatch chain has seventeen arms — the TUI, `wizard -p`,
//! the gateway, ACP, `mcp serve`, fleet, doctor, the scheduler, and so on —
//! and every one of them is reached through [`crate::run`]. Wiring the kernel
//! into each surface separately would mean seventeen places to forget, and the
//! ones that get forgotten are exactly the ones nobody runs interactively.
//! One call, above the chain, before any arm has had a chance to return.
//!
//! # Failure is a warning, always
//!
//! A plugin that will not load costs its own registrations and nothing else.
//! Every load below is fallible-and-logged, and the Rust half additionally
//! catches a panic in `apply`: a compiled-in plugin is still third-party code
//! from the kernel's point of view, and "wizard will not start" is not an
//! acceptable outcome for a broken one. The kernel's registries are behind
//! mutexes it recovers from poisoning, so a panic mid-`apply` leaves a
//! half-registered plugin's ledger unloaded rather than a wedged kernel.
//!
//! # The host bridge
//!
//! [`host::WizardHost`] is installed into the kernel below, so `wizard.http`,
//! `wizard.process`, `wizard.model`, `wizard.ui` and `wizard.agent` reach the
//! real thing rather than an error. Four of the five need a running agent, and
//! an agent binds itself through [`host::bind`] when it is built; see
//! [`host`] for what each namespace resolves to and what an unbound process
//! still answers.

#[cfg(feature = "acp")]
pub mod acp;
#[cfg(feature = "provider-anthropic")]
pub mod anthropic;
#[cfg(feature = "fleet")]
pub mod fleet;
pub mod host;

#[cfg(feature = "provider-chatgpt")]
pub mod chatgpt;
#[cfg(feature = "provider-cloudflare")]
pub mod cloudflare;
#[cfg(feature = "provider-llamacpp")]
pub mod llamacpp;
// The window and the agent core under it: two directories, one plugin. `gui`
// is not a second plugin and registers nothing — it is sessions, the config
// store, git and OAuth, the half of the GUI that draws nothing and that
// another front end could be written against. It stays a sibling rather than
// a child of `native` because nesting it would say the window owns it, and
// `native/mod.rs` is explicit that the window is a *client* of it. Both are
// behind the one `native` feature, which `install.sh`, the release workflow
// and `docs/native-gui.md` all name.
#[cfg(feature = "native")]
pub mod gui;
#[cfg(feature = "native")]
pub mod native;
#[cfg(feature = "provider-ollama")]
pub mod ollama;
#[cfg(feature = "provider-openai")]
pub mod openai;
#[cfg(feature = "tool-web")]
pub mod web;
#[cfg(feature = "provider-xai")]
pub mod xai;

#[cfg(feature = "graph")]
pub mod graph;

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::kernel::{Kernel, KernelOptions, Plugin, PluginSource};
use crate::tools::registry::ToolRegistry;

/// Every Rust plugin this build ships, in load order.
///
/// The only file in the tree that names them. Each line is one cargo feature,
/// and deleting the feature deletes the plugin: the vector is shorter, nothing
/// else in the tree changes, and the `kind`/tool/command/entrypoint it
/// registered is simply absent. That is the "delete any one plugin" rule from
/// `docs/plugins.md`, and it is what the `--no-default-features` leg of
/// `contrib/check-plugin-work.sh` proves.
///
/// Seven of them are providers. Three are *surfaces* — the window, the ACP
/// server and the fleet — and they are the reason `docs/plugins.md`'s first
/// rule is a rule rather than an observation: `src/lib.rs` used to call
/// `native::run`, `acp::run` and `fleet::run` by name, and the entrypoints
/// they register are what replaced those three calls.
//
// Built by pushing rather than as a `vec![]` literal because every line is
// `#[cfg]`-gated, and an attribute on an element of a vec literal is not
// stable Rust. Both lints below are consequences of that shape and of the
// empty build being legal, which is the whole point of the file.
#[allow(unused_mut, clippy::vec_init_then_push)]
fn compiled_in() -> Vec<Arc<dyn Plugin>> {
    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
    #[cfg(feature = "acp")]
    plugins.push(Arc::new(acp::AcpPlugin::new()));
    #[cfg(feature = "provider-anthropic")]
    plugins.push(Arc::new(anthropic::AnthropicPlugin::new()));
    #[cfg(feature = "provider-chatgpt")]
    plugins.push(Arc::new(chatgpt::ChatGptPlugin::new()));
    #[cfg(feature = "provider-cloudflare")]
    plugins.push(Arc::new(cloudflare::CloudflarePlugin::new()));
    #[cfg(feature = "fleet")]
    plugins.push(Arc::new(fleet::FleetPlugin::new()));
    #[cfg(feature = "provider-llamacpp")]
    plugins.push(Arc::new(llamacpp::LlamaCppPlugin::new()));
    #[cfg(feature = "native")]
    plugins.push(Arc::new(native::NativePlugin::new()));
    #[cfg(feature = "provider-ollama")]
    plugins.push(Arc::new(ollama::OllamaPlugin::new()));
    #[cfg(feature = "provider-openai")]
    plugins.push(Arc::new(openai::OpenAiPlugin::new()));
    #[cfg(feature = "provider-xai")]
    plugins.push(Arc::new(xai::XaiPlugin::new()));
    #[cfg(feature = "tool-web")]
    plugins.push(Arc::new(web::WebPlugin::new()));
    #[cfg(feature = "graph")]
    plugins.push(Arc::new(graph::GraphPlugin::new()));
    plugins
}

static KERNEL: OnceLock<Kernel> = OnceLock::new();
/// The bridge behind this process's `wizard.*`, kept beside the kernel so
/// [`host::bind`] can reach it as a `WizardHost` rather than as the
/// `dyn HostBridge` the kernel stores.
static HOST: OnceLock<Arc<host::WizardHost>> = OnceLock::new();
/// Set by [`boot`] before the kernel is built, so `--cwd` reaches the sandbox
/// confinement. Absent when something touched the kernel first, in which case
/// the process's working directory is the honest answer.
static PROJECT_ROOT: OnceLock<PathBuf> = OnceLock::new();
/// Latched by [`boot`] so the Lua half runs once even if two surfaces call it.
static LUA_LOADED: OnceLock<()> = OnceLock::new();

/// The process kernel, with every compiled-in Rust plugin loaded.
///
/// Idempotent and cheap after the first call. Safe to call from anywhere,
/// including outside a tokio runtime and from a unit test — which is the
/// point, because [`crate::llm::registry`] calls it before answering what a
/// `kind = "..."` means.
pub fn kernel() -> &'static Kernel {
    KERNEL.get_or_init(|| {
        let mut options = KernelOptions::default();
        if let Some(root) = PROJECT_ROOT.get() {
            options.project_root = root.clone();
        }
        // The host is built from the same root the kernel confines file
        // helpers to, so a `wizard.process.run` with no agent bound runs where
        // a sandboxed plugin can already read and write.
        let host = Arc::new(host::WizardHost::new(options.project_root.clone()));
        let _ = HOST.set(Arc::clone(&host));
        options.host = host;
        let kernel = Kernel::new(options);
        LOADING.set(true);
        for plugin in compiled_in() {
            load_rust(&kernel, plugin);
        }
        LOADING.set(false);
        kernel
    })
}

thread_local! {
    /// True on the thread that is inside [`kernel`]'s initializer.
    ///
    /// `OnceLock::get_or_init` deadlocks if its own closure calls it again, and
    /// the closure above does exactly the thing that would: it loads plugins,
    /// and a provider plugin's registration calls `llm::registry::install`,
    /// which ensures. Thread-local rather than a global flag because the
    /// re-entrancy is per-thread by construction — a *different* thread
    /// arriving during the load must block on the `OnceLock` and see the
    /// finished kernel, which is exactly what skipping the ensure would break.
    static LOADING: Cell<bool> = const { Cell::new(false) };
}

/// Load one Rust plugin, surviving anything it does.
///
/// `catch_unwind` rather than trust, because the promise this module makes is
/// that a broken plugin cannot stop Wizard from starting, and an `apply` that
/// panics is a broken plugin exactly as much as one that returns `Err`. The
/// `AssertUnwindSafe` is sound here for a narrow reason: everything the closure
/// touches is behind a `Mutex` the kernel already recovers from poisoning
/// (`unwrap_or_else(PoisonError::into_inner)` at every lock site), so an
/// interrupted `apply` leaves a partially-filled registry that the next
/// operation reads normally rather than a torn one.
fn load_rust(kernel: &Kernel, plugin: Arc<dyn Plugin>) {
    let name = plugin.manifest().name.clone();
    match std::panic::catch_unwind(AssertUnwindSafe(|| kernel.load(plugin))) {
        Ok(Ok(_)) => tracing::debug!("plugin '{name}' loaded"),
        Ok(Err(err)) => tracing::warn!("plugin '{name}' did not load: {err}"),
        Err(_) => tracing::warn!("plugin '{name}' panicked while loading; it is not available"),
    }
}

/// Bring the plugin set up for this process. Call once, from [`crate::run`].
///
/// `project_root` is `--cwd` when it was given, before the chdir any single
/// dispatch arm performs: the kernel confines a sandboxed plugin's file
/// helpers to it, and a confinement computed from the wrong directory is worse
/// than none because it looks like it is working.
///
/// Returns nothing and fails at nothing. Every outcome a caller could branch
/// on — no plugin directory, a plugin whose manifest will not parse, a script
/// that errors on its first line — is a warning in the log and a session that
/// starts anyway.
pub async fn boot(project_root: Option<&Path>) {
    if let Some(root) = project_root {
        let _ = PROJECT_ROOT.set(root.to_path_buf());
    }
    let kernel = kernel();
    // `set` is the latch: exactly one caller gets `Ok`, so a second `boot`
    // (a test, a surface that calls it defensively) is a no-op rather than a
    // second copy of every user plugin failing to claim its own tool names.
    if LUA_LOADED.set(()).is_err() {
        return;
    }
    load_user_plugins(kernel).await;
}

/// Load every `~/.wizard/plugins/<name>/` that looks like a plugin.
///
/// Sorted, so two plugins that both want a name are refused in the same order
/// on every machine and the error message is reproducible. Loaded as
/// [`PluginSource::Registry`], which is the bounded profile: a plugin that
/// arrived by being dropped in a directory has not been read by anybody, so it
/// runs interpreted under the deadline hook. First-party status is a property
/// of shipping in the binary, and the binary is [`compiled_in`].
async fn load_user_plugins(kernel: &Kernel) {
    let root = kernel.plugin_root().to_path_buf();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        // No plugin directory is the common case on a fresh install and says
        // nothing worth logging at warning level.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            tracing::warn!("plugin directory {} unreadable: {err}", root.display());
            return;
        }
    };

    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        // A directory without a `plugin.lua` is not a broken plugin, it is not
        // a plugin: `~/.wizard/plugins` is a place people leave notes and
        // half-finished checkouts, and warning about those trains the warning
        // out of usefulness.
        .filter(|path| path.join("plugin.lua").is_file())
        .collect();
    dirs.sort();

    for dir in dirs {
        match kernel.load_lua(&dir, PluginSource::Registry).await {
            Ok(id) => tracing::info!("plugin '{id}' loaded from {}", dir.display()),
            Err(err) => tracing::warn!("plugin at {} did not load: {err:#}", dir.display()),
        }
    }
}

/// Copy every plugin-registered tool into `registry`, and say how many went.
///
/// The bridge from the kernel's registries to the agent's. Called from
/// [`crate::agent::build_tool_registry`], which every agent-bearing surface
/// goes through, and from `mcp serve`, which builds its own.
pub fn install_tools_into(registry: &mut ToolRegistry) -> usize {
    kernel().install_tools_into(registry)
}

/// This process's host bridge, once the kernel exists.
///
/// Ensures the kernel first, for the same reason `llm::registry` does: a
/// binding made before anything had looked the kernel up would land in a
/// `OnceLock` nobody had filled. `None` only on the thread that is inside the
/// initializer, which cannot be an agent binding itself.
pub(crate) fn host_bridge() -> Option<Arc<host::WizardHost>> {
    if LOADING.get() {
        return None;
    }
    let _ = kernel();
    HOST.get().cloned()
}

/// True on the thread that is currently inside the kernel's initializer.
///
/// The one thing anybody outside this module needs to know about [`LOADING`],
/// and it is needed for the same reason [`ensure_providers`] checks it:
/// [`kernel`] is a `OnceLock::get_or_init`, so a call that re-enters it from
/// inside its own closure deadlocks. [`crate::entrypoint::installed`] is the
/// second such caller — a plugin that asked, during `apply`, whether some
/// surface was registered would otherwise hang the process rather than get an
/// answer.
pub(crate) fn loading() -> bool {
    LOADING.get()
}

/// Make sure the compiled-in plugins have registered before a provider kind is
/// resolved.
///
/// Called by [`crate::llm::registry`] and by nothing else. It exists because a
/// provider plugin's registration has to be visible to `ProviderConfig::build`
/// no matter who calls it — including callers that run long before [`boot`],
/// or in a process where `run` is never entered at all, which is every unit
/// test in the tree.
///
/// Safe to call from anywhere in the registry, reads and writes alike: the
/// one call that would otherwise re-enter — a loading plugin's own
/// `registry::install` — is short-circuited by [`LOADING`], because that
/// thread is already inside the initializer and the plugins it is waiting on
/// are itself.
pub(crate) fn ensure_providers() {
    if LOADING.get() {
        return;
    }
    let _ = kernel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::registry::{self, ProviderKind};

    /// Whatever this build ships, the table and the kernel agree about it, and
    /// every plugin in it actually loaded. A plugin that panicked or was
    /// refused a name would show up here as a missing id.
    #[test]
    fn every_compiled_in_plugin_is_loaded() {
        let kernel = kernel();
        for plugin in compiled_in() {
            let name = &plugin.manifest().name;
            assert!(
                kernel.loaded().iter().any(|id| id.as_str() == name),
                "plugin '{name}' is in the table but did not load"
            );
        }
    }

    /// Every provider kind this build ships is present *because a plugin
    /// registered it*, and every kind it does not ship is absent with an error
    /// that says so.
    ///
    /// The table is the feature list, so this is one assertion per row and the
    /// `#[cfg]` on each row is the whole of the "delete any one plugin" rule:
    /// with the feature on, the kernel holds the name and a config can select
    /// the kind; with it off, `installed` answers `None` and [`registry::unknown`]
    /// names the kind and lists what is left. `contrib/check-plugin-work.sh`
    /// builds both sides.
    #[test]
    fn a_kind_is_installed_exactly_when_its_plugin_is_compiled_in() {
        /// `(cargo feature is on, plugin name, kinds it registers)`.
        ///
        /// Written out rather than derived from [`compiled_in`] on purpose: a
        /// plugin that quietly stopped registering one of its two kinds would
        /// still be in `compiled_in`, and a test generated from the thing it is
        /// testing cannot see that.
        const EXPECTED: &[(bool, &str, &[ProviderKind])] = &[
            (
                cfg!(feature = "provider-anthropic"),
                "anthropic",
                &[ProviderKind::ANTHROPIC],
            ),
            (
                cfg!(feature = "provider-chatgpt"),
                "chatgpt",
                &[ProviderKind::CHATGPT_OAUTH],
            ),
            (
                cfg!(feature = "provider-cloudflare"),
                "cloudflare",
                &[ProviderKind::CLOUDFLARE],
            ),
            (
                cfg!(feature = "provider-llamacpp"),
                "llamacpp",
                &[ProviderKind::LLAMACPP],
            ),
            (
                cfg!(feature = "provider-ollama"),
                "ollama",
                &[ProviderKind::OLLAMA],
            ),
            (
                cfg!(feature = "provider-openai"),
                "openai",
                &[ProviderKind::OPENAI, ProviderKind::OPENROUTER],
            ),
            (
                cfg!(feature = "provider-xai"),
                "xai",
                &[ProviderKind::XAI, ProviderKind::XAI_OAUTH],
            ),
        ];

        let kernel = kernel();
        let loaded = kernel.provider_names();
        for (compiled_in, name, kinds) in EXPECTED {
            for kind in *kinds {
                if *compiled_in {
                    assert!(
                        loaded.iter().any(|n| n == kind.as_str()),
                        "the kernel should hold the kind plugin '{name}' registered: {kind}"
                    );
                    let descriptor =
                        registry::installed(kind).expect("a config can select an installed kind");
                    assert_eq!(descriptor.kind(), kind);
                    assert!(!descriptor.display_name().is_empty(), "{kind}");
                } else {
                    assert!(
                        registry::installed(kind).is_none(),
                        "kind '{kind}' is installed on a build without plugin '{name}'"
                    );
                    let message = registry::unknown(kind).to_string();
                    assert!(message.contains(kind.as_str()), "{message}");
                }
            }
        }

        // Nothing reached the process kernel that no row above accounts for.
        // This is what would catch a provider coming back through some path
        // other than a plugin — the `builtin.rs` table, for instance, whose
        // deletion this test is the receipt for.
        //
        // Swept over the *kernel's* slot rather than `registry::kinds()`
        // because the process registry is shared with every other test in the
        // binary, and the kernel tests that exercise `Ctx::provider` leave
        // their own kinds in it. The kernel singleton is written only by
        // `compiled_in`, so it is the order-independent half of the pair.
        for name in &loaded {
            assert!(
                EXPECTED
                    .iter()
                    .any(|(on, _, kinds)| *on && kinds.iter().any(|k| k.as_str() == name)),
                "kind '{name}' is in the process kernel but no compiled-in plugin claims it"
            );
        }
    }

    /// The back-compat claim, from the outside: a **stock** build answers to
    /// all nine kinds that have ever been written into a `config.toml`.
    ///
    /// `builtin.rs` used to make this assertion over its own table, which
    /// meant it could only ever agree with itself. Asserted here against
    /// literal strings — the exact bytes on disk — it is the thing a user
    /// actually depends on, and it fails if a kind quietly changes spelling on
    /// its way into a plugin. Skipped on any build that is not stock, because
    /// leaving a plugin out is the whole point of the feature.
    #[test]
    #[cfg(all(
        feature = "provider-anthropic",
        feature = "provider-chatgpt",
        feature = "provider-cloudflare",
        feature = "provider-llamacpp",
        feature = "provider-ollama",
        feature = "provider-openai",
        feature = "provider-xai",
    ))]
    fn a_stock_build_still_answers_to_all_nine_shipped_kinds() {
        let installed = registry::kinds();
        for id in [
            "anthropic",
            "chatgptoauth",
            "cloudflare",
            "llamacpp",
            "ollama",
            "openai",
            "openrouter",
            "xai",
            "xaioauth",
        ] {
            let kind = ProviderKind::new(id);
            assert!(installed.contains(&kind), "{id} is not installed");
            let descriptor = registry::installed(&kind).expect("installed");
            assert_eq!(descriptor.kind().as_str(), id);
            assert!(!descriptor.display_name().is_empty(), "{id}");
        }
    }

    /// The tool half of "delete any one plugin", asserted in both directions.
    ///
    /// A provider that is absent still has a `kind` string a user can type, so
    /// [`registry::unknown`] is its degrade path. A tool has no equivalent: an
    /// absent tool must be **absent from the roster**, because the roster is
    /// what the model is told it can call. Advertising a tool that cannot run
    /// costs a turn to discover, in the middle of somebody's work, and there is
    /// no error message that makes that acceptable.
    ///
    /// So this asserts the kernel's slot rather than a message, and the row is
    /// written out rather than derived from [`compiled_in`] for the reason the
    /// provider table gives: a plugin that quietly stopped registering one of
    /// its three tools would still be in `compiled_in`.
    #[test]
    fn a_tool_is_registered_exactly_when_its_plugin_is_compiled_in() {
        /// `(cargo feature is on, plugin name, tools it registers)`.
        const EXPECTED: &[(bool, &str, &[&str])] = &[(
            cfg!(feature = "tool-web"),
            "web",
            &["web_fetch", "web_search", "x_search"],
        )];

        let kernel = kernel();
        let registered = kernel.tool_names();
        for (compiled_in, name, tools) in EXPECTED {
            for tool in *tools {
                assert_eq!(
                    registered.iter().any(|n| n == tool),
                    *compiled_in,
                    "tool '{tool}' from plugin '{name}'"
                );
            }
        }

        // And nothing reached the kernel's tool slot that no row accounts for.
        // The kernel tests register their own tools into their own kernels, so
        // this reads the process one, which only `compiled_in` writes.
        for name in &registered {
            assert!(
                EXPECTED
                    .iter()
                    .any(|(on, _, tools)| *on && tools.contains(&name.as_str())),
                "tool '{name}' is in the process kernel but no compiled-in plugin claims it"
            );
        }
    }

    /// The surface half of "delete any one plugin": `wizard <name>` finds a
    /// body exactly on the builds that compiled one in.
    ///
    /// A third degrade path, different again from the other two. An absent
    /// provider still has a `kind` a user can type, so it degrades to a named
    /// error. An absent tool must vanish from the roster, because the roster
    /// is what the model is told it can call. An absent *surface* can do
    /// neither: the `clap` variant is in core and stays parseable whatever the
    /// feature set, so `wizard acp --help` still lists it and somebody will
    /// still type it. So it degrades to a sentence naming the flag that brings
    /// it back — [`crate::entrypoint::absent`] — and the thing this test pins
    /// is that the lookup behind that sentence is honest in both directions.
    ///
    /// The `true` direction is not decoration. A surface registering under the
    /// right name but the wrong *argument type* fails the `TypeId` downcast
    /// and is indistinguishable at the call site from a surface that was never
    /// compiled in, so a build with the feature on would tell the user to
    /// rebuild with the feature on.
    #[test]
    fn an_entrypoint_is_registered_exactly_when_its_plugin_is_compiled_in() {
        use crate::entrypoint::{self, Entrypoint};

        let kernel = kernel();
        let services = kernel.services();

        // Two argument types, so this is two lookups rather than one loop; a
        // table would have to erase the type that is the whole point of the
        // assertion. Written out for the same reason the provider and tool
        // tables are: derived from `compiled_in`, it could only agree with
        // itself.
        assert_eq!(
            services
                .inject_as::<Entrypoint>(entrypoint::GUI)
                .map(|entry| entry.name()),
            cfg!(feature = "native").then_some(entrypoint::GUI),
            "the window's entrypoint"
        );
        assert_eq!(
            services
                .inject_as::<Entrypoint>(entrypoint::ACP)
                .map(|entry| entry.name()),
            cfg!(feature = "acp").then_some(entrypoint::ACP),
            "the ACP server's entrypoint"
        );
        assert_eq!(
            services
                .inject_as::<Entrypoint<crate::cli::FleetCmd>>(entrypoint::FLEET)
                .map(|entry| entry.name()),
            cfg!(feature = "fleet").then_some(entrypoint::FLEET),
            "the fleet's entrypoint"
        );
    }

    /// The half that matters to the model: a plugin tool reaches the registry
    /// every agent-bearing surface composes from, and an absent one leaves no
    /// trace in it.
    ///
    /// [`install_tools_into`] is the only bridge, and `build_tool_registry`
    /// and `mcp serve` are its only callers, so asserting it here covers both
    /// without standing up an agent.
    #[test]
    fn plugin_tools_reach_the_agents_registry_and_only_when_compiled_in() {
        let mut registry = ToolRegistry::with_native_tools();
        let native = registry.len();
        let installed = install_tools_into(&mut registry);
        assert_eq!(registry.len(), native + installed);

        let advertised: Vec<String> = registry
            .specs()
            .into_iter()
            .map(|spec| spec.function.name)
            .collect();
        for tool in ["web_fetch", "web_search", "x_search"] {
            assert_eq!(
                advertised.iter().any(|name| name == tool),
                cfg!(feature = "tool-web"),
                "{tool} in the advertised roster"
            );
        }
    }

    /// `graph` registers nothing, on purpose, and that has to stay true by
    /// assertion rather than by nobody having noticed.
    ///
    /// It is the one plugin whose product is a data model — see
    /// [`graph::GraphPlugin`] — so the day it grows a tool or a command is the
    /// day that decision should be made deliberately rather than found in a
    /// diff. The plugin still has to *load*, which
    /// [`every_compiled_in_plugin_is_loaded`] covers.
    #[test]
    #[cfg(feature = "graph")]
    fn the_graph_plugin_loads_and_registers_nothing() {
        let kernel = kernel();
        assert!(kernel.loaded().iter().any(|id| id.as_str() == "graph"));
        let manifest = graph::GraphPlugin::new();
        assert!(
            manifest.manifest().capabilities.is_empty(),
            "arithmetic over a store the caller holds needs no grant"
        );
        for name in kernel.tool_names() {
            assert!(!name.starts_with("graph"), "{name}");
        }
        for name in kernel.command_names() {
            assert!(!name.starts_with("graph"), "{name}");
        }
    }

    /// Exactly one backend has a process `/server` manages. Ollama runs
    /// locally too and must not be caught by that flag.
    #[test]
    fn only_llamacpp_owns_a_local_server() {
        for kind in registry::kinds() {
            let descriptor = registry::installed(&kind).expect("installed");
            assert_eq!(
                descriptor.manages_local_server(),
                kind == ProviderKind::LLAMACPP,
                "{kind}"
            );
        }
    }
}
