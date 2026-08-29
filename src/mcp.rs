//! The seam between Wizard and the Model Context Protocol: the `mcp.toml`
//! format, and the lookup that finds whatever can speak the protocol.
//!
//! The protocol itself is not here. `src/plugins/mcp/` is, behind
//! `--features mcp`: the JSON-RPC framing, the stdio child and its
//! environment scrub, the streamable-HTTP transport, the `tools/list`
//! pagination, the image decoding, the reserved-name table, and the
//! `wizard mcp-serve` surface that points all of it the other way. What is
//! left in this file is a name, two small traits and a holder —
//! the [`crate::server`] shape, for the same reason: the *shape* is core's
//! because half a dozen core call sites are written against it, and the
//! *bytes on the wire* are one subsystem's.
//!
//! # Why the file format stayed in core
//!
//! [`McpConfig`] and its two supporting types are the on-disk shape of
//! `~/.wizard/mcp.toml`, and three core modules read or write one without
//! ever dialing a server:
//!
//! - [`crate::evolve`] upserts a `[[server]]` the model proposed, and its
//!   `EvolveChannel::McpServer` is a core enum five consumers match on.
//! - [`crate::import_claude`] translates Claude Code's `mcpServers` into
//!   these types and writes the file, on a machine that may never run one.
//! - [`crate::doctor`] lists what is configured before it probes anything.
//!
//! That is the `[web]`/`[mesh]`/`[fleet]` rule with one more consumer than
//! usual: a config file that was valid yesterday does not stop parsing
//! because somebody built without a feature, and `wizard import-claude` on a
//! build with no MCP client still writes a file the *next* build can use.
//! Pushing the format down into the plugin would have made two of those three
//! modules unbuildable without it, which is a plugin-to-plugin edge in
//! everything but name — and `evolve` would additionally have had to construct
//! a plugin's type to name a channel, which is the objection
//! `docs/plugins.md` records against `schedule`'s `ServiceSpec`.
//!
//! # Why [`McpManager`] stayed, and is now a holder rather than a manager
//!
//! Four surfaces hold this type across a whole process — the TUI in an
//! `Arc<Mutex<_>>`, the window and the ACP server in an `Arc<RwLock<_>>`, the
//! gateway by `&mut` — and three of those four are themselves plugins. Had
//! they held the plugin's own type, `mcp` would not be removable without
//! removing them too: four plugin-to-plugin edges to buy nothing. So the
//! holder is core's and what it holds is a `dyn` trait object, exactly as
//! `App::mesh` holds a [`SessionTee`](crate::app::SessionTee) and for exactly
//! the same reason. Every one of those four call sites is unchanged by the
//! split, which is the receipt on the boundary being in the right place.
//!
//! # What an absent plugin looks like
//!
//! Zero connections and zero tools, which is also what an empty `mcp.toml`
//! looks like — so a build with no MCP client and no configured servers says
//! nothing, because there is nothing to say. Configure a server on such a
//! build and [`McpManager::reload`] fails with the sentence naming the
//! feature, which the TUI shows as a notice and `wizard doctor` as a failed
//! check per server. That is "degrade in presence, never in behaviour": the
//! failure is loud exactly when somebody asked for something this binary
//! cannot do, and silent when they did not.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::tools::Tool;

/// The service name the MCP plugin registers its connector under, and the one
/// this module injects.
///
/// A `const` for the reason [`crate::entrypoint::GUI`] is one: the two ends
/// are core and a feature-gated plugin, and a typo in either compiles into a
/// build where every `mcp.toml` server silently fails to connect while the
/// whole client sits in the binary.
pub const MCP_CONNECTOR: &str = "mcp-connector";

/// The cargo feature that brings the client and `wizard mcp-serve` back.
///
/// Named here rather than at each call site because, unlike a tool's feature
/// (which [`crate::plugins::run_tool`] takes as an argument so core holds no
/// table), this one has four call sites in two files and they must all print
/// the same flag.
const FEATURE: &str = "mcp";

/// A live set of connections to the servers in `mcp.toml`.
///
/// Two methods and no more, because two is what core asks: *what can the
/// model call* and *how many answered*. Everything else an MCP client does —
/// the handshake, pagination, respawning a crashed stdio child, decoding an
/// image block — happens on the other side of this trait and core never has
/// an opinion about it.
///
/// `disconnect` is the third and it is not a question: a stdio server is a
/// child process, so replacing a client has to close the old one rather than
/// drop it and hope. It takes `&self` rather than `self: Box<Self>` because
/// [`McpManager::reload`] tears down before it knows whether the rebuild will
/// succeed, and a half-consumed box is not a state worth having.
#[async_trait]
pub trait McpClient: Send + Sync {
    /// Every connected server's tools, as registry-ready [`Tool`]s, with
    /// names already made unique against each other and against the names
    /// Wizard itself can register.
    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>>;

    /// How many servers completed the handshake. Fewer than were configured
    /// is normal and is what the TUI's "connected 2 of 3" reports.
    fn connection_count(&self) -> usize;

    /// Close every connection.
    async fn disconnect(&self);
}

/// How a plugin makes an [`McpClient`], and how [`crate::doctor`] asks after
/// one server without making one.
///
/// Two methods because core asks two questions of the protocol it does not
/// implement: *bring up everything in this file*, and *tell me about this one
/// entry*. Doctor could have been served by connecting a whole manager and
/// reading [`McpManager::connection_count`], and that would report "2 of 3"
/// where a diagnostic has to name which one failed and why.
///
/// Owned arguments because both futures are `'static`: the TUI runs the
/// connect on a background task, off the draw path, and a borrowed config
/// would have to outlive a task nobody holds. Same trade
/// [`crate::app::tee::TeeFactory`] and [`crate::server::LocalServer::start`]
/// make.
#[async_trait]
pub trait McpConnector: Send + Sync {
    /// Dial every server in `config`, skipping the ones that will not come up.
    ///
    /// `Err` is for a failure of the *whole* attempt. One server that refuses
    /// its handshake is a warning and a smaller `connection_count`, because a
    /// session must not lose its other five servers to one bad entry.
    async fn connect(&self, config: McpConfig) -> Result<Box<dyn McpClient>>;

    /// Handshake with one server and describe what answered, for
    /// `wizard doctor`.
    ///
    /// A **sentence** rather than a status enum, which is
    /// [`crate::server::LocalServer`]'s decision and the same argument: what
    /// is worth reporting is the protocol revision the server claimed and how
    /// many tools it listed, and neither is a shape core can enumerate without
    /// describing one protocol's internals. The connect budget is the
    /// plugin's too, so a server that fails doctor on time is one that would
    /// fail a session on time.
    async fn probe(&self, server: McpServerConfig) -> Result<String>;
}

/// The registered [`McpConnector`], boxed into something the service registry
/// can hand back.
///
/// A newtype for the mechanical reason [`crate::server::LocalServerHandle`]
/// gives: `inject_as` is an `Arc<dyn Any>` downcast and `Arc::downcast` needs
/// a `Sized` target, so publishing an `Arc<dyn McpConnector>` would mean the
/// injector naming `Arc<Arc<dyn McpConnector>>` to get it back.
pub struct McpConnectorHandle(Box<dyn McpConnector>);

impl McpConnectorHandle {
    /// Wrap a plugin's implementation for registration.
    pub fn new(connector: impl McpConnector + 'static) -> Self {
        Self(Box::new(connector))
    }
}

impl std::ops::Deref for McpConnectorHandle {
    type Target = dyn McpConnector;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl std::fmt::Debug for McpConnectorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnectorHandle").finish()
    }
}

/// The connector some plugin registered, or [`None`] on a build without one.
///
/// [`None`] is the whole contract, the same one
/// [`crate::entrypoint::installed`] has: an absent plugin is an `inject` that
/// answers nothing, never a link error and never a panic. The `loading()`
/// guard is there for the same reason too — a plugin asking this from inside
/// its own `apply` would re-enter the kernel's `OnceLock` and deadlock.
pub fn installed() -> Option<Arc<McpConnectorHandle>> {
    if crate::plugins::loading() {
        return None;
    }
    crate::plugins::kernel()
        .services()
        .inject_as::<McpConnectorHandle>(MCP_CONNECTOR)
}

/// The error a caller gets when it asked this build to reach an MCP server
/// and this build has no client.
///
/// It reads like [`crate::entrypoint::absent`] because it is the same
/// situation one subcommand further in: the config was written by a person
/// who meant it, so the only useful answer is which flag makes it work.
fn absent() -> anyhow::Error {
    anyhow::anyhow!(
        "this build has no MCP client — it was compiled without the `{FEATURE}` feature, so \
         the servers in mcp.toml cannot be reached.\n\
         \n\
         To get it: `cargo build --release --features {FEATURE}` from a checkout, or install \
         a stock release binary, which has it — `{FEATURE}` is on by default and every \
         published `wizard` carries it."
    )
}

/// Handshake with one configured server and say what answered.
///
/// `wizard doctor`'s half of the seam, and it returns a **sentence** rather
/// than a status enum for the reason [`crate::server`]'s three methods do:
/// what is worth reporting about a connect is the server's own name, its
/// protocol revision and how many tools it listed, and none of that is a shape
/// core can enumerate without describing one protocol's internals. The budget
/// is the plugin's too, so a server that is slow enough to fail here is slow
/// enough to fail in a session, which is the whole point of doctor probing at
/// the runtime's own timeout.
pub async fn probe(server: &McpServerConfig) -> Result<String> {
    let Some(connector) = installed() else {
        return Err(absent());
    };
    connector.probe(server.clone()).await
}

/// This process's MCP connections, or the absence of any.
///
/// The name is unchanged from when this type *was* the client, and
/// deliberately: it is still the thing that owns a process's connections and
/// still what a surface holds for the life of a session. What moved out from
/// under it is the protocol. `docs/plugins.md` calls this shape "the shape
/// stays, the thing moves", after `src/app/tee.rs`.
///
/// One per process by convention rather than by construction — the TUI, the
/// window and the ACP server each build exactly one and share it, because
/// connecting per agent would run one copy of every configured stdio server
/// per agent, each a real OS process.
#[derive(Default)]
pub struct McpManager {
    client: Option<Box<dyn McpClient>>,
}

impl McpManager {
    /// No servers connected. The state every surface starts in, and the state
    /// a build with no MCP plugin stays in.
    pub fn empty() -> Self {
        Self { client: None }
    }

    /// Connect every server in `config`.
    ///
    /// Fails only when `config` names servers and this build cannot reach
    /// them; a server that is merely down is skipped with a warning by the
    /// client, because one bad server must not cost a session its other five.
    pub async fn connect_all(config: &McpConfig) -> Result<Self> {
        let mut manager = Self::empty();
        manager.reload(config).await?;
        Ok(manager)
    }

    /// How many servers answered.
    pub fn connection_count(&self) -> usize {
        self.client
            .as_ref()
            .map_or(0, |client| client.connection_count())
    }

    /// Every connected server's tools, ready for the registry.
    ///
    /// An empty vector when nothing is connected, which is why
    /// [`crate::tools::registry::ToolRegistry::attach_mcp`] needs no branch:
    /// no plugin, no servers and every server down are one answer.
    pub async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        match &self.client {
            Some(client) => client.tools().await,
            None => Ok(Vec::new()),
        }
    }

    /// Drop every connection and reconnect from `config` — `/reload`, the
    /// TUI's background connect, and the import that follows `/settings`.
    ///
    /// An empty `config` is a successful disconnect rather than a refusal,
    /// even on a build with no plugin: a user with no servers configured has
    /// asked for nothing and must not be told about a feature they do not
    /// need. That is the line between degrading in presence and nagging.
    pub async fn reload(&mut self, config: &McpConfig) -> Result<()> {
        // Tear down first and unconditionally. The old connections are stdio
        // children and HTTP sessions, and a rebuild that fails must not leave
        // them running behind a handle nobody holds any more.
        if let Some(client) = self.client.take() {
            client.disconnect().await;
        }
        if config.servers.is_empty() {
            return Ok(());
        }
        let Some(connector) = installed() else {
            bail!(absent());
        };
        self.client = Some(connector.connect(config.clone()).await?);
        Ok(())
    }
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager")
            .field("connections", &self.connection_count())
            .finish()
    }
}

/// Contents of `~/.wizard/mcp.toml`:
///
/// ```toml
/// [[server]]
/// name = "computer-use"
/// transport = "stdio"
/// command = "uvx"
/// args = ["mcp-computer-use"]
///
/// [[server]]
/// name = "remote"
/// transport = "http"
/// url = "https://mcp.example.com/mcp"
/// [server.headers]
/// Authorization = "Bearer literal-token"
/// X-Api-Key = "env:MY_API_KEY"   # resolved from the environment at connect time
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default, rename = "server")]
    pub servers: Vec<McpServerConfig>,
}

impl McpConfig {
    /// Load from `path`, returning an empty config when the file is missing.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw)
                .with_context(|| format!("invalid MCP config at {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => {
                Err(err).with_context(|| format!("failed to read MCP config at {}", path.display()))
            }
        }
    }

    /// Persist to `path` (used by `/evolve` when registering a server).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("failed to serialize MCP config")?;
        std::fs::write(path, raw)
            .with_context(|| format!("failed to write MCP config to {}", path.display()))
    }
}

/// Transport used to reach an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// Spawn `command args...` and speak JSON-RPC over stdin/stdout.
    Stdio,
    /// Streamable HTTP endpoint at `url`.
    Http,
}

/// One `[[server]]` entry in `mcp.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique server name; used to namespace colliding tool names.
    pub name: String,
    pub transport: McpTransport,
    /// Executable to spawn (stdio transport).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments for `command` (stdio transport).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Endpoint URL (http transport).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra environment variables for the spawned process (stdio transport).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
    /// Extra HTTP headers sent on every request (http transport), e.g.
    /// `Authorization`. A value of the form `env:VAR` is resolved from the
    /// environment at connect time so the token never sits in `mcp.toml`.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub headers: std::collections::HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_load_missing_file_is_empty() {
        let config = McpConfig::load(Path::new("/nonexistent/wizard-mcp.toml"))
            .expect("missing file should yield default config");
        assert!(config.servers.is_empty());
    }

    #[test]
    fn config_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wizard-mcp-test-{}", std::process::id()));
        let path = dir.join("mcp.toml");
        let config = McpConfig {
            servers: vec![
                McpServerConfig {
                    name: "computer-use".into(),
                    transport: McpTransport::Stdio,
                    command: Some("uvx".into()),
                    args: vec!["mcp-computer-use".into()],
                    url: None,
                    env: std::collections::HashMap::from([("FOO".to_string(), "bar".to_string())]),
                    headers: std::collections::HashMap::new(),
                },
                McpServerConfig {
                    name: "search".into(),
                    transport: McpTransport::Http,
                    command: None,
                    args: vec![],
                    url: Some("http://127.0.0.1:8808/mcp".into()),
                    env: std::collections::HashMap::new(),
                    headers: std::collections::HashMap::from([
                        ("Authorization".to_string(), "Bearer tok-123".to_string()),
                        ("X-Api-Key".to_string(), "env:MY_API_KEY".to_string()),
                    ]),
                },
            ],
        };
        config.save(&path).expect("save should succeed");
        let loaded = McpConfig::load(&path).expect("load should succeed");
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(loaded.servers.len(), 2);
        assert_eq!(loaded.servers[0].name, "computer-use");
        assert_eq!(loaded.servers[0].transport, McpTransport::Stdio);
        assert_eq!(loaded.servers[0].command.as_deref(), Some("uvx"));
        assert_eq!(
            loaded.servers[0].env.get("FOO").map(String::as_str),
            Some("bar")
        );
        assert_eq!(loaded.servers[1].transport, McpTransport::Http);
        assert_eq!(
            loaded.servers[1].url.as_deref(),
            Some("http://127.0.0.1:8808/mcp")
        );
        assert_eq!(
            loaded.servers[1]
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer tok-123")
        );
        assert_eq!(
            loaded.servers[1]
                .headers
                .get("X-Api-Key")
                .map(String::as_str),
            Some("env:MY_API_KEY")
        );
        assert!(loaded.servers[0].headers.is_empty());
    }

    #[test]
    fn headers_parse_from_toml_and_default_empty() {
        let raw = r#"
[[server]]
name = "remote"
transport = "http"
url = "https://mcp.example.com/mcp"

[server.headers]
Authorization = "Bearer abc"
"#;
        let config: McpConfig = toml::from_str(raw).expect("valid toml");
        assert_eq!(
            config.servers[0]
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer abc")
        );

        // Absent table defaults to empty (older configs keep parsing).
        let raw = "[[server]]\nname = \"plain\"\ntransport = \"http\"\nurl = \"https://x/mcp\"\n";
        let config: McpConfig = toml::from_str(raw).expect("valid toml");
        assert!(config.servers[0].headers.is_empty());
    }

    /// A manager with nothing connected is the same thing on every build, and
    /// it is what every surface starts holding: no connections, no tools, and
    /// no error for a user who configured no servers.
    #[tokio::test]
    async fn an_empty_manager_has_no_connections_and_no_tools() {
        let manager = McpManager::empty();
        assert_eq!(manager.connection_count(), 0);
        assert!(
            manager
                .tools()
                .await
                .expect("no tools, no error")
                .is_empty()
        );
    }

    /// Reloading an empty config is a successful disconnect whether or not
    /// this build has a client, because a user with no `[[server]]` entries
    /// has asked for nothing. This is the half of the degrade path that must
    /// stay *quiet*; the loud half is below.
    #[tokio::test]
    async fn reloading_an_empty_config_never_mentions_the_feature() {
        let mut manager = McpManager::empty();
        manager
            .reload(&McpConfig::default())
            .await
            .expect("nothing configured, nothing to say");
        assert_eq!(manager.connection_count(), 0);
    }

    /// The other half: a build with no client, asked to reach a configured
    /// server, says which feature would let it. Skipped when the plugin *is*
    /// compiled in, because then the call really does try to spawn `uvx`.
    #[tokio::test]
    #[cfg(not(feature = "mcp"))]
    async fn a_configured_server_on_a_build_with_no_client_names_the_feature() {
        let mut manager = McpManager::empty();
        let config = McpConfig {
            servers: vec![McpServerConfig {
                name: "somewhere".to_string(),
                transport: McpTransport::Stdio,
                command: Some("uvx".to_string()),
                args: Vec::new(),
                url: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            }],
        };
        let err = manager
            .reload(&config)
            .await
            .expect_err("no client")
            .to_string();
        assert!(err.contains("--features mcp"), "{err}");
        assert!(err.contains("mcp.toml"), "{err}");
    }

    /// And `wizard doctor`'s half says the same thing, for the same reason:
    /// somebody configured a server this binary cannot reach, and the useful
    /// answer is the flag rather than a connection error.
    #[tokio::test]
    #[cfg(not(feature = "mcp"))]
    async fn probing_a_server_on_a_build_with_no_client_names_the_feature() {
        let server = McpServerConfig {
            name: "somewhere".to_string(),
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
            url: Some("https://example.com/mcp".to_string()),
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        };
        let err = probe(&server).await.expect_err("no client").to_string();
        assert!(err.contains("--features mcp"), "{err}");
    }
}
