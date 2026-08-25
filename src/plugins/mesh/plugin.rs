//! `mesh`, as a plugin: two registrations and a manifest.
//!
//! The mesh is the largest thing that has gone through the door — ~12k lines
//! over a QUIC transport, two certificate verifiers, an mDNS browser, a peer
//! store, a consent ledger and a terminal surface — and what core sees of it
//! is two names in a registry.
//!
//! # The two registrations, and why neither is a tool or a command
//!
//! - [`entrypoint::PEERS`] is `wizard peers`, a whole clap subcommand tree
//!   whose `trust` argument is [`Trust`](super::Trust) itself. It is a
//!   [`Subcommand`] for the reason `wizard gui` is an
//!   [`Entrypoint`](crate::entrypoint::Entrypoint): it runs before there is a
//!   session, takes arguments core cannot type, and returns an exit code.
//! - [`tee::SESSION_TEE`](crate::app::tee::SESSION_TEE) is the factory
//!   [`App`](crate::app::App) opens a session's tee from. It is a service
//!   rather than an event handler because a tee is a *live object* with a
//!   lifetime — bound socket, running mDNS, a `leave` that has to say goodbye
//!   — and an event handler is a callback with nowhere to keep one.
//!
//! No tool and no slash command, deliberately. Everything the mesh does that a
//! human asks for is a CLI subcommand, and everything it does that a human does
//! not ask for is the tee. A `mesh_publish` tool would be a model deciding who
//! watches this session, which is a trust decision and therefore a person's.
//!
//! # `apply` opens no socket
//!
//! It cannot: [`crate::plugins::kernel`] is a `OnceLock` initializer that runs
//! synchronously, sometimes with no tokio runtime, from unit tests and from
//! `wizard doctor`. Both lines below are map inserts. The socket is bound by
//! whichever registration is *used* — `wizard peers ping` dialling, or the tee
//! at session start with `[mesh] listen = true` — which is also the
//! default-off posture the mesh has always had: nothing here is a reason for a
//! wizard that was merely started to be on the network.

use crate::app::tee;
use crate::entrypoint::{self, Subcommand};
use crate::kernel::{Capability, Ctx, Plugin, PluginManifest, Service};

/// The P2P mesh, as a plugin.
///
/// Behind `--features mesh`, on by default. Leaving it out removes
/// `wizard peers`, the session tee, quinn, rustls and mdns-sd from the build:
/// `App::mesh` is a `None` nothing can fill, and `wizard peers` prints what
/// [`crate::run`] prints for any subcommand nothing answers to.
pub struct MeshPlugin {
    manifest: PluginManifest,
}

impl MeshPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "mesh".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "Peer-to-peer mesh: identity, QUIC transport, trust, `wizard peers`"
                    .to_string(),
                capabilities: vec![
                    // A UDP socket, mDNS multicast, and dialling peers by
                    // address. The one plugin whose network use is not HTTP.
                    Capability::Network,
                    // `~/.wizard/node.key` and `~/.wizard/mesh/peers.json`:
                    // this node's identity and every decision made about a
                    // peer.
                    Capability::Filesystem,
                    // A watched peer's turns render in this machine's
                    // transcript, which is `wizard.ui`'s territory even though
                    // the tee reaches it directly rather than through the
                    // bridge. Declared because the manifest is what a reader
                    // consults, and one that omitted this would say the mesh
                    // never writes on somebody's screen.
                    Capability::Ui,
                ],
                optional_deps: Vec::new(),
                // In `server` as well as `full`: a headless box is exactly the
                // machine somebody wants to watch from a laptop. Not in `pi`,
                // which `docs/plugins.md` defines as "no mesh".
                profiles: vec!["full".to_string(), "server".to_string()],
            },
        }
    }
}

impl Default for MeshPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for MeshPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provide(
            entrypoint::PEERS,
            Service::native(Subcommand::new(
                entrypoint::PEERS,
                super::cli::SUMMARY,
                |args| super::cli::run_args(args),
            )),
        );
        ctx.provide(tee::SESSION_TEE, Service::native(super::tee::factory()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TeeFactory;

    /// `apply` registers both halves, under the names core looks up. A kernel
    /// of its own rather than the process one, so this still means something
    /// in a binary where another test booted plugins first.
    #[test]
    fn applying_the_plugin_registers_the_peers_tree_and_the_tee_factory() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        kernel
            .load(std::sync::Arc::new(MeshPlugin::new()))
            .expect("the mesh plugin loads");
        let peers = kernel
            .services()
            .inject_as::<Subcommand>(entrypoint::PEERS)
            .expect("the mesh registered its subcommand");
        assert_eq!(peers.name(), entrypoint::PEERS);
        assert!(
            kernel
                .services()
                .inject_as::<TeeFactory>(tee::SESSION_TEE)
                .is_some(),
            "the mesh registered its tee factory"
        );
    }

    /// Unload withdraws both, which is the property that makes `mesh` a
    /// plugin rather than a directory: a `wizard peers` that still answered
    /// after the plugin went away would be a registration the ledger lost
    /// track of.
    #[tokio::test]
    async fn unloading_the_plugin_withdraws_both_registrations() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        let id = kernel
            .load(std::sync::Arc::new(MeshPlugin::new()))
            .expect("the mesh plugin loads");
        kernel.unload(&id).await.expect("it unloads");
        assert!(
            kernel
                .services()
                .inject_as::<Subcommand>(entrypoint::PEERS)
                .is_none(),
            "`wizard peers` outlived the plugin that registered it"
        );
        assert!(
            kernel
                .services()
                .inject_as::<TeeFactory>(tee::SESSION_TEE)
                .is_none(),
            "the tee factory outlived the plugin that registered it"
        );
    }
}
