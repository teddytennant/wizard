//! `gateway`, as a plugin: two entrypoints and a manifest.
//!
//! The messaging gateway is the sixth surface through the door
//! [`crate::entrypoint`] opened for `wizard gui`, and the first plugin that
//! owns *two* CLI surfaces rather than one. That is what makes it worth its
//! own file rather than four lines at the bottom of [`super`].
//!
//! # Two entrypoints, two names
//!
//! - [`entrypoint::GATEWAY`] is `wizard --gateway`: the long-running bot
//!   process. It takes a [`Config`](crate::config::Config), returns when the
//!   poll loop stops, and is an [`Entrypoint`] for exactly the reasons
//!   `wizard gui` is one — it runs before there is a session (it builds its
//!   own headless agent per chat), and it does not return until the surface
//!   is finished.
//! - [`entrypoint::GATEWAY_SERVICE`] is `wizard gateway setup|install|logs|…`:
//!   administering that process. It takes the parsed
//!   [`GatewayCmd`](crate::cli::GatewayCmd), which is the
//!   [`FleetCmd`](crate::cli::FleetCmd) shape — core parses the tree because
//!   `--help` has to keep listing it on a build with no gateway, and only the
//!   body moves. It keeps its own exit code, like the fleet's: `gateway
//!   status` on a machine with no unit installed is neither an error worth a
//!   backtrace nor a success a script should branch on.
//!
//! **Two names, not one name at two argument types.** The obvious spelling
//! was one `"gateway"` carrying both, on the strength of
//! [`Entrypoint`]'s type parameter: `Entrypoint<Config>` and
//! `Entrypoint<GatewayCmd>` really are different types and `inject_as`
//! really does separate them by `TypeId`. It does not work, and the gateway
//! is the first plugin to find out, because
//! [`ServiceRegistry`](crate::kernel::ServiceRegistry) is a
//! `HashMap<String, _>`: the second `provide` under a name *replaces* the
//! first, deliberately, so that a reload can put a service back without a
//! window where injectors see [`None`]. The downcast is what keeps a
//! mismatched lookup honest (it answers [`None`] rather than calling the
//! wrong body); it is not a second dimension of the key. So a plugin with two
//! surfaces needs two names, and `docs/plugins.md` now says so.
//!
//! # `apply` opens no socket and reads no token
//!
//! Same constraint as every other plugin: [`crate::plugins::kernel`] is a
//! `OnceLock` initializer that runs synchronously, sometimes with no tokio
//! runtime, from unit tests and from `wizard doctor`. Both lines below are map
//! inserts. The bot token is read, and the first long-poll issued, by
//! [`super::run`] — which is to say by somebody who typed `--gateway`.

use crate::cli::GatewayCmd;
use crate::config::Config;
use crate::entrypoint::{self, Entrypoint};
use crate::kernel::{Capability, Ctx, Plugin, PluginManifest, Service};

/// The messaging gateway, as a plugin.
///
/// Behind `--features gateway`, on by default. Leaving it out removes the
/// Telegram transport, the setup wizard and the service installer: both
/// `wizard --gateway` and `wizard gateway <verb>` print what [`crate::run`]
/// prints for any surface nothing answers to, and `[gateway]` in
/// `config.toml` still parses and round-trips — which is the same promise
/// `[web]`, `[mesh]` and `[fleet]` make, and for the same reason. A config
/// file that was valid yesterday does not become invalid because somebody
/// built without a feature.
pub struct GatewayPlugin {
    manifest: PluginManifest,
}

impl GatewayPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "gateway".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "Messaging gateway: drive an agent from a chat app".to_string(),
                capabilities: vec![
                    // Long-polling the platform's HTTP API, and downloading
                    // the photos and documents a message carries.
                    Capability::Network,
                    // Those downloads land on disk, and `gateway install`
                    // writes a unit file and a 0600 token file for a process
                    // that inherits no environment.
                    Capability::Filesystem,
                    // A gateway turn is a sovereign turn: it runs `execute`
                    // on this machine on behalf of whoever is in an allowed
                    // chat. Declaring less would be the manifest lying about
                    // the blast radius, which is what the grant prompt is
                    // generated from.
                    Capability::Process,
                    // One headless agent per chat, spending the user's tokens.
                    Capability::Model,
                ],
                optional_deps: Vec::new(),
                // `server` above all: a long-lived headless process with no
                // terminal is the shape of machine this is for, and it is the
                // profile `docs/plugins.md` defines as "full minus GUI … plus
                // gateway and ACP".
                profiles: vec!["full".to_string(), "server".to_string()],
            },
        }
    }
}

impl Default for GatewayPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for GatewayPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provide(
            entrypoint::GATEWAY,
            Service::native(Entrypoint::new(entrypoint::GATEWAY, |config: Config| {
                super::run(config)
            })),
        );
        ctx.provide(
            entrypoint::GATEWAY_SERVICE,
            Service::native(Entrypoint::with_status(
                entrypoint::GATEWAY_SERVICE,
                |cmd: GatewayCmd| super::run_service(cmd),
            )),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both surfaces register, under the names and at the argument types core
    /// looks them up with. A kernel of its own rather than the process one, so
    /// this still means something in a binary where another test booted
    /// plugins first.
    #[test]
    fn applying_the_plugin_registers_both_gateway_surfaces() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        kernel
            .load(std::sync::Arc::new(GatewayPlugin::new()))
            .expect("the gateway plugin loads");
        let serve = kernel
            .services()
            .inject_as::<Entrypoint<Config>>(entrypoint::GATEWAY)
            .expect("the gateway registered `wizard --gateway`");
        assert_eq!(serve.name(), entrypoint::GATEWAY);
        let admin = kernel
            .services()
            .inject_as::<Entrypoint<GatewayCmd>>(entrypoint::GATEWAY_SERVICE)
            .expect("the gateway registered `wizard gateway <verb>`");
        assert_eq!(admin.name(), entrypoint::GATEWAY_SERVICE);
    }

    /// The two names are two names, and swapping them answers nothing.
    ///
    /// This is the assertion the module docs are about: the registry keys on
    /// the name alone, so one name could not have carried both bodies, and the
    /// downcast is what turns a lookup at the wrong type into an honest
    /// [`None`] rather than into the wrong surface starting up.
    #[test]
    fn the_two_surfaces_do_not_answer_to_each_others_names_or_types() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        kernel
            .load(std::sync::Arc::new(GatewayPlugin::new()))
            .expect("the gateway plugin loads");
        let services = kernel.services();
        assert!(
            services
                .inject_as::<Entrypoint<GatewayCmd>>(entrypoint::GATEWAY)
                .is_none(),
            "`wizard --gateway` answered to the admin tree's argument type"
        );
        assert!(
            services
                .inject_as::<Entrypoint<Config>>(entrypoint::GATEWAY_SERVICE)
                .is_none(),
            "the admin tree answered to `wizard --gateway`'s argument type"
        );
    }

    /// Unload withdraws both. A `wizard --gateway` that still answered after
    /// the plugin went away would be a registration the ledger lost track of,
    /// which is the one thing a kernel exists to prevent.
    #[tokio::test]
    async fn unloading_the_plugin_withdraws_both_surfaces() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        let id = kernel
            .load(std::sync::Arc::new(GatewayPlugin::new()))
            .expect("the gateway plugin loads");
        kernel.unload(&id).await.expect("it unloads");
        let services = kernel.services();
        assert!(
            services
                .inject_as::<Entrypoint<Config>>(entrypoint::GATEWAY)
                .is_none(),
            "`wizard --gateway` outlived the plugin that registered it"
        );
        assert!(
            services
                .inject_as::<Entrypoint<GatewayCmd>>(entrypoint::GATEWAY_SERVICE)
                .is_none(),
            "`wizard gateway <verb>` outlived the plugin that registered it"
        );
    }
}
