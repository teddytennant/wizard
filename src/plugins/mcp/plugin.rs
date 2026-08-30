//! `mcp`, as a plugin: two registrations and a manifest.
//!
//! The Model Context Protocol is the seventh surface through the door
//! [`crate::entrypoint`] opened for `wizard gui`, and the second plugin that
//! registers *two* things core looks up by name. The gateway was the first,
//! and it found the rule this file follows: the service registry is a
//! `HashMap<String, _>` and `provide` replaces a name already taken, so two
//! registrations means two names whatever their types.
//!
//! # The two are not two surfaces, and that is the difference from the gateway
//!
//! - [`crate::mcp::MCP_CONNECTOR`] is the **client**: an
//!   [`McpConnector`](crate::mcp::McpConnector) that
//!   [`McpManager`](crate::mcp::McpManager) injects to dial the servers in
//!   `mcp.toml`, and that `wizard doctor` injects to probe one. Nobody types a
//!   command to reach it; it is how a model gets tools it was not compiled
//!   with.
//! - [`entrypoint::MCP_SERVE`] is the **server**: `wizard mcp-serve`, a CLI
//!   subcommand whose `clap` variant stays in core and whose body is here.
//!
//! So one is a capability and one is a surface, and they degrade differently
//! for that reason. An absent client is zero tools plus, if `mcp.toml` names a
//! server, the sentence [`crate::mcp`] writes; an absent surface is
//! [`entrypoint::absent`], because the `clap` variant keeps parsing whatever
//! this build contains and somebody will still type the verb.
//!
//! # `apply` spawns nothing and dials nothing
//!
//! Same constraint as every other plugin: [`crate::plugins::kernel`] is a
//! `OnceLock` initializer that runs synchronously, sometimes with no tokio
//! runtime, from unit tests and from `wizard doctor`. Both lines below are map
//! inserts. The first `initialize` handshake is issued by whoever holds an
//! [`McpManager`](crate::mcp::McpManager) and asked it to connect — the TUI on
//! a background task, off the draw path, because `npx -y @playwright/mcp` is a
//! couple of seconds of npm resolution before it says a word.

use crate::cli::McpServeCmd;
use crate::entrypoint::{self, Entrypoint};
use crate::kernel::{Capability, Ctx, Plugin, PluginManifest, Service};
use crate::mcp::{self, McpConnectorHandle};

/// The Model Context Protocol, in both directions, as a plugin.
///
/// Behind `--features mcp`, on by default. Leaving it out removes the client
/// (so `mcp.toml` reaches nothing and the model is offered no MCP tools) and
/// `wizard mcp-serve` (so an editor pointed at this binary is told which flag
/// it needs). `mcp.toml` itself still parses, still round-trips, and is still
/// written by `wizard import-claude` and by `/evolve` — the same promise
/// `[web]`, `[mesh]`, `[fleet]` and `[gateway]` make about a config section,
/// applied to a config *file*.
pub struct McpPlugin {
    manifest: PluginManifest,
}

impl McpPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "Model Context Protocol: use other servers' tools, and serve ours"
                    .to_string(),
                capabilities: vec![
                    // The HTTP transport dials an operator-supplied URL, and
                    // an `env:VAR` header carries a bearer token to it.
                    Capability::Network,
                    // The stdio transport spawns `command args...` — `uvx`,
                    // `npx`, a binary on `PATH` — as a child of this process.
                    // This is the widest grant on the manifest and it is the
                    // honest one: what an MCP server may do is whatever the
                    // person who wrote the `[[server]]` entry chose.
                    Capability::Process,
                    // `wizard mcp-serve` hands another client this machine's
                    // `read_file`/`write_file`/`execute`, and a remote tool's
                    // result may carry an image this process decodes.
                    Capability::Filesystem,
                ],
                optional_deps: Vec::new(),
                // `server` as much as `full`: a headless box serving its tools
                // to an editor over stdio is exactly what `mcp-serve` is for,
                // and a gateway turn reaching a browser-automation server is
                // what the client is for.
                profiles: vec![
                    "server".to_string(),
                    "default".to_string(),
                    "full".to_string(),
                ],
            },
        }
    }
}

impl Default for McpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for McpPlugin {
    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provide(
            mcp::MCP_CONNECTOR,
            Service::native(McpConnectorHandle::new(super::Connector)),
        );
        ctx.provide(
            entrypoint::MCP_SERVE,
            Service::native(Entrypoint::new(
                entrypoint::MCP_SERVE,
                "Serve Wizard's own tools over stdio as an MCP server, so any MCP client \
                 (Claude Code, Cursor, another Wizard) can call them",
                |cmd: McpServeCmd| super::serve::run(cmd.scripted),
            )),
        );
        Ok(())
    }

    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::McpConnector;

    /// Both registrations land, under the names and at the types core looks
    /// them up with. A kernel of its own rather than the process one, so this
    /// still means something in a binary where another test booted plugins
    /// first.
    #[test]
    fn applying_the_plugin_registers_the_client_and_the_server() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        kernel
            .load(std::sync::Arc::new(McpPlugin::new()))
            .expect("the mcp plugin loads");
        let services = kernel.services();
        assert!(
            services
                .inject_as::<McpConnectorHandle>(mcp::MCP_CONNECTOR)
                .is_some(),
            "the client did not register"
        );
        let serve = services
            .inject_as::<Entrypoint<McpServeCmd>>(entrypoint::MCP_SERVE)
            .expect("`wizard mcp-serve` did not register");
        assert_eq!(serve.name(), entrypoint::MCP_SERVE);
    }

    /// Unload withdraws both. A `wizard mcp-serve` that still answered after
    /// the plugin went away would be a registration the ledger lost track of,
    /// which is the one thing a kernel exists to prevent — and a connector
    /// that outlived it would keep spawning child processes on behalf of a
    /// plugin that is gone.
    #[tokio::test]
    async fn unloading_the_plugin_withdraws_both() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        let id = kernel
            .load(std::sync::Arc::new(McpPlugin::new()))
            .expect("the mcp plugin loads");
        kernel.unload(&id).await.expect("it unloads");
        let services = kernel.services();
        assert!(
            services
                .inject_as::<McpConnectorHandle>(mcp::MCP_CONNECTOR)
                .is_none(),
            "the client outlived the plugin that registered it"
        );
        assert!(
            services
                .inject_as::<Entrypoint<McpServeCmd>>(entrypoint::MCP_SERVE)
                .is_none(),
            "`wizard mcp-serve` outlived the plugin that registered it"
        );
    }

    /// The connector really dials: a probe of a stdio server whose command
    /// does not exist fails with the spawn's own words rather than with a
    /// timeout, which is what tells a `wizard doctor` reader to fix their
    /// `command =` line instead of their network.
    #[tokio::test]
    async fn probing_a_server_that_cannot_be_spawned_says_so_quickly() {
        let server = crate::mcp::McpServerConfig {
            name: "nope".to_string(),
            transport: crate::mcp::McpTransport::Stdio,
            command: Some("wizard-no-such-mcp-server-binary".to_string()),
            args: Vec::new(),
            url: None,
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        };
        let err = super::super::Connector
            .probe(server)
            .await
            .expect_err("nothing to spawn")
            .to_string();
        assert!(!err.contains("no handshake within"), "{err}");
    }
}
