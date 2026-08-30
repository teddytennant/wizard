//! The registration side of the fleet: one [`Entrypoint`] under the name
//! `wizard fleet` looks up, and nothing else.
//!
//! Kept in its own file for the same reason `native/plugin.rs` is — this is
//! the whole of the plugin's contract with the rest of the tree, and burying
//! it under two thousand lines of supervision code is how it ends up being
//! edited by accident.
//!
//! # Why this one is `with_status`
//!
//! `wizard fleet` is the first surface with a *tree* under it, and two of its
//! three leaves say something with their exit code that they do not say with
//! an error. `fleet stop` on a project where nothing is running prints one
//! plain sentence and exits 1: not a failure — there is no backtrace worth
//! printing and nothing went wrong — but not a success either, because a
//! script that stops a fleet in a loop needs to know it stopped nothing. The
//! `Result<()>` shape the window and the ACP server use cannot express that
//! without turning it into an `Err`, which would change what the user reads in
//! order to keep a signature uniform.

use crate::entrypoint::{self, Entrypoint};
use crate::kernel::{Capability, Ctx, Plugin, PluginManifest, Service};

/// The line `wizard --help` gives `fleet`.
///
/// Core held it as a doc comment on the `clap` variant and printed it whether
/// or not the build had a fleet in it. Moved here verbatim, minus the
/// trailing stop `clap` strips off a doc comment on its way into the same
/// slot. The `FleetCmd` variants underneath keep their doc comments: parsing
/// `fleet run -n 3` is core's job on every build, so their help has to be
/// there on every build too.
const ABOUT: &str = "Fleet mode: decompose a mission into independent tasks and run them as \
                     parallel headless workers, each in its own git worktree, then merge the \
                     fleet branches back. See docs/fleet.md";

/// Parallel sovereign workers over git worktrees, as a plugin.
pub struct FleetPlugin {
    manifest: PluginManifest,
}

impl FleetPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "fleet".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "`wizard fleet`: decompose a mission and run it as parallel headless \
                     workers over git worktrees"
                    .to_string(),
                // A surface over the agent core declares everything the agent
                // core can do, because that is what it can do: this one plans
                // and synthesizes with real turns on the user's key (`Model`),
                // spawns `wizard --mode sovereign` children and drives git
                // (`Process`), lays out worktrees and result JSON under
                // `.wizard/fleet/` (`Filesystem`), draws a progress bar per
                // slot (`Ui`), and every worker it starts has the whole tool
                // set including the network (`Network`, `Agent`).
                //
                // Nothing enforces this — see `native/plugin.rs` for the long
                // version. `Capability` gates the Lua host bridge, and a
                // compiled-in Rust plugin reaches past it into the crate. The
                // declaration is what a reader consults to find out what a
                // plugin touches, and one that claims less than it does is a
                // manifest that lies.
                capabilities: vec![
                    Capability::Filesystem,
                    Capability::Process,
                    Capability::Network,
                    Capability::Model,
                    Capability::Ui,
                    Capability::Agent,
                ],
                optional_deps: Vec::new(),
                // In `server` as well as `full`: a fleet run is headless by
                // construction — its workers are `wizard --mode sovereign`
                // children with no terminal — so a box with no display is
                // where it makes the most sense, not the least. Out of `pi`
                // and `minimal`, where N parallel agents and N git worktrees
                // are not what the machine is for.
                profiles: vec![
                    "server".to_string(),
                    "default".to_string(),
                    "full".to_string(),
                ],
            },
        }
    }
}

impl Default for FleetPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for FleetPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provide(
            entrypoint::FLEET,
            Service::native(Entrypoint::with_status(
                entrypoint::FLEET,
                ABOUT,
                super::run,
            )),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::FleetCmd;

    /// `apply` registers the one thing it claims to, under the name core
    /// looks up **and the argument type core looks it up with**. The second
    /// half is what a `Config`-shaped registration would fail, and it would
    /// fail it silently: `installed` would answer `None` and `wizard fleet`
    /// would report that this build has no fleet while the fleet sat in it.
    ///
    /// A kernel of its own rather than the process one, so this still means
    /// something in a binary where some other test already booted plugins.
    #[test]
    fn applying_the_plugin_registers_the_fleet_entrypoint() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        kernel
            .load(std::sync::Arc::new(FleetPlugin::new()))
            .expect("the fleet plugin loads");
        let found = kernel
            .services()
            .inject_as::<Entrypoint<FleetCmd>>(entrypoint::FLEET)
            .expect("the fleet registered its entrypoint");
        assert_eq!(found.name(), entrypoint::FLEET);
    }

    /// Unloading takes it back, so a reload does not leave two fleets
    /// answering to one name.
    #[tokio::test]
    async fn unloading_the_plugin_withdraws_the_entrypoint() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        let id = kernel
            .load(std::sync::Arc::new(FleetPlugin::new()))
            .expect("the fleet plugin loads");
        kernel.unload(&id).await.expect("it unloads");
        assert!(
            kernel
                .services()
                .inject_as::<Entrypoint<FleetCmd>>(entrypoint::FLEET)
                .is_none(),
            "the entrypoint outlived the plugin that registered it"
        );
    }
}
