//! The Lua half of [`super::compiled_in`]: first-party plugins that ship
//! inside the binary.
//!
//! [`super::compiled_in`] is the one table naming every Rust plugin. This is
//! the one table naming every Lua plugin Wizard ships with, and the two are
//! deliberately the same shape — one line per plugin, each behind the cargo
//! feature that owns it, and deleting the feature deletes the plugin.
//!
//! # Why `include_str!` and not a directory on disk
//!
//! `~/.wizard/plugins/<name>/` already exists and [`super::load_user_plugins`]
//! already reads it, so the obvious mechanism for shipping a first-party Lua
//! plugin is for `install.sh` to copy one in there. That was tried on paper
//! and does not survive three questions.
//!
//! **`cargo test` would not have it.** A test binary never runs `install.sh`,
//! so `git_status` would be absent from every registry a test composes, and
//! the port would be proven by nothing. Pointing the tests at the developer's
//! own `~/.wizard/plugins` is worse: the suite would then pass or fail on the
//! contents of somebody's home directory.
//!
//! **`cargo install wizard` would not have it either.** Nor would `nix build`,
//! nor a `cargo build` from a checkout, nor the release binary anybody
//! downloads and runs without the installer. A tool that is present or absent
//! depending on how the binary arrived is not a tool the model can be told
//! about.
//!
//! **And a file on disk cannot be first-party.** [`PluginSource::FirstParty`]
//! is what turns the instruction hook off and the JIT on — see
//! `docs/plugins.md` on the async spike — and it is a claim about *who wrote
//! this code*, which for a file under `~/.wizard` is "whoever last edited it".
//! Loading a user-writable file unbounded would make the bound a formality:
//! anything that could drop a plugin in that directory could drop it in this
//! one. Shipping in the binary is the only place the claim is true, which is
//! exactly the rule [`super::compiled_in`] already follows for Rust.
//!
//! So a first-party Lua plugin is `include_str!`d, both halves of it, and the
//! bytes the kernel loads are the bytes in the repository. `~/.wizard/plugins`
//! keeps its meaning: it is where *other people's* plugins go, and they stay
//! bounded.
//!
//! # When they load
//!
//! Rust plugins load inside [`super::kernel`]'s `OnceLock`, synchronously,
//! because their `apply` is a few map inserts. A Lua plugin's is a LuaJIT VM
//! and a script, and [`crate::kernel::lua::load_source`] is `async` — it spawns
//! the VM's task and awaits its first answer. There is no synchronous door
//! into that, and adding one would mean a `block_on` inside a `OnceLock`
//! initializer that some callers reach from inside a runtime.
//!
//! So they load from [`ensure`], which is idempotent and is called from the
//! two places that need the tools to exist: [`super::boot`], which every
//! surface goes through, and [`crate::agent::build_tool_registry`], which
//! every agent-bearing surface *and every test that composes a registry* goes
//! through. The second is what makes `cargo test` see them without a fixture.
//!
//! `mcp serve` and `harness export` compose their own registries and need no
//! call of their own: both are dispatch arms of `crate::run`, and [`boot`] is
//! above the chain. Their *tests* do call [`ensure`], and have to — a test
//! that composed a bundle from a registry the loader had never filled would
//! agree with itself about a bundle the real export never writes.
//!
//! [`boot`]: super::boot

use tokio::sync::OnceCell;

use crate::kernel::manifest::{PluginManifest, PluginSource};
use crate::kernel::{Kernel, lua};

/// One first-party Lua plugin, as it exists in the binary.
struct BundledPlugin {
    /// Path under `src/plugins/lua/`, used as the chunk name so a Lua
    /// traceback names a file somebody can open.
    origin: &'static str,
    manifest: &'static str,
    script: &'static str,
}

/// Every Lua plugin this build ships, in load order.
///
/// The only place in the tree that names one, which is the same promise
/// [`super::compiled_in`] makes about Rust plugins. A build without the
/// feature has a shorter vector and nothing else changes: the tools are not
/// registered, so the model is not told about them, which is the degrade path
/// `docs/plugins.md` requires of an absent tool.
#[allow(unused_mut, clippy::vec_init_then_push)]
fn bundled() -> Vec<BundledPlugin> {
    let mut plugins: Vec<BundledPlugin> = Vec::new();
    #[cfg(feature = "tool-git")]
    plugins.push(BundledPlugin {
        origin: "src/plugins/lua/git/plugin.lua",
        manifest: include_str!("lua/git/manifest.toml"),
        script: include_str!("lua/git/plugin.lua"),
    });
    plugins
}

/// Latch, so two surfaces calling [`ensure`] do not load every plugin twice
/// and lose the second copy to a name conflict.
static LOADED: OnceCell<()> = OnceCell::const_new();

/// Load the bundled Lua plugins into the process kernel. Cheap after the first
/// call, and safe to call from anywhere with a runtime under it.
pub async fn ensure() {
    let kernel = super::kernel();
    LOADED.get_or_init(|| load_into(kernel)).await;
}

/// Load every bundled plugin into `kernel`.
///
/// Separate from [`ensure`] because the tests load them into a kernel of their
/// own — one rooted in a temp directory, with a host they control — and a
/// bundled plugin that could only be exercised through the process singleton
/// would be a plugin whose tests could not run twice.
///
/// A plugin that will not load costs its own registrations and nothing else,
/// which is [`super::load_rust`]'s rule applied to the other half. The failure
/// is loud in the log because a *bundled* plugin failing is a bug in this
/// repository rather than in somebody's install.
pub(crate) async fn load_into(kernel: &Kernel) {
    for plugin in bundled() {
        let manifest = match PluginManifest::parse(plugin.manifest) {
            Ok(manifest) => manifest,
            Err(err) => {
                tracing::error!("bundled plugin {} has a bad manifest: {err}", plugin.origin);
                continue;
            }
        };
        let name = manifest.name.clone();
        match lua::load_source(
            kernel,
            manifest,
            PluginSource::FirstParty,
            plugin.script,
            &format!("@{}", plugin.origin),
            None,
            None,
        )
        .await
        {
            Ok(id) => tracing::debug!("bundled plugin '{id}' loaded"),
            Err(err) => tracing::error!("bundled plugin '{name}' did not load: {err:#}"),
        }
    }
}

/// A host for the bundled plugins that reaches the real implementations, on a
/// kernel of the test's own.
///
/// [`super::host::WizardHost`] unbound is exactly what a plugin gets in a
/// process with no agent in front of it, so this is not a stub: `exec` runs
/// real programs through the real runner, and the only thing it lacks is the
/// agent-shaped half the git plugin never asks for.
#[cfg(all(test, feature = "tool-git"))]
pub(crate) fn test_kernel(root: &std::path::Path) -> Kernel {
    Kernel::new(crate::kernel::KernelOptions {
        project_root: root.to_path_buf(),
        plugin_root: root.join("plugins"),
        host: std::sync::Arc::new(super::host::WizardHost::new(root)),
        ..crate::kernel::KernelOptions::default()
    })
}

// Everything in there is the git plugin's, so it goes with the feature. The
// day a second plugin is bundled this becomes `#[cfg(test)]` and the git half
// moves behind its own module.
#[cfg(all(test, feature = "tool-git"))]
mod tests;
