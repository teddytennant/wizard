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
//! # What is not wired yet
//!
//! The host bridge is still [`crate::kernel::UnwiredHost`], so a Lua plugin
//! that calls `wizard.http` or `wizard.model` gets an error naming the reason
//! rather than a silent no-op. Attaching a real bridge is its own change; see
//! `docs/plugins.md`.

#[cfg(feature = "provider-anthropic")]
pub mod anthropic;
#[cfg(feature = "provider-chatgpt")]
pub mod chatgpt;
#[cfg(feature = "provider-cloudflare")]
pub mod cloudflare;
#[cfg(feature = "provider-llamacpp")]
pub mod llamacpp;
#[cfg(feature = "provider-ollama")]
pub mod ollama;
#[cfg(feature = "provider-openai")]
pub mod openai;
#[cfg(feature = "provider-xai")]
pub mod xai;

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
/// else in the tree changes, and the `kind`/tool/command it registered is
/// simply absent. That is the "delete any one plugin" rule from
/// `docs/plugins.md`, and it is what the `--no-default-features` leg of
/// `contrib/check-plugin-work.sh` proves.
//
// Built by pushing rather than as a `vec![]` literal because every line is
// `#[cfg]`-gated, and an attribute on an element of a vec literal is not
// stable Rust. Both lints below are consequences of that shape and of the
// empty build being legal, which is the whole point of the file.
#[allow(unused_mut, clippy::vec_init_then_push)]
fn compiled_in() -> Vec<Arc<dyn Plugin>> {
    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
    #[cfg(feature = "provider-anthropic")]
    plugins.push(Arc::new(anthropic::AnthropicPlugin::new()));
    #[cfg(feature = "provider-chatgpt")]
    plugins.push(Arc::new(chatgpt::ChatGptPlugin::new()));
    #[cfg(feature = "provider-cloudflare")]
    plugins.push(Arc::new(cloudflare::CloudflarePlugin::new()));
    #[cfg(feature = "provider-llamacpp")]
    plugins.push(Arc::new(llamacpp::LlamaCppPlugin::new()));
    #[cfg(feature = "provider-ollama")]
    plugins.push(Arc::new(ollama::OllamaPlugin::new()));
    #[cfg(feature = "provider-openai")]
    plugins.push(Arc::new(openai::OpenAiPlugin::new()));
    #[cfg(feature = "provider-xai")]
    plugins.push(Arc::new(xai::XaiPlugin::new()));
    plugins
}

static KERNEL: OnceLock<Kernel> = OnceLock::new();
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
