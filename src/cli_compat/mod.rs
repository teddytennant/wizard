//! Claude Code's subcommands, spelled the same here.
//!
//! `claude mcp`, `claude plugin`, `claude auth`, `claude setup-token`,
//! `claude install`, `claude import`, `claude purge`, `claude stop` and
//! `claude rm` each get a `wizard` spelling that does the closest thing
//! Wizard has. The ones that name an Anthropic service (`ultrareview`,
//! `auto-mode`) or a Claude Code background-session lifecycle Wizard does not
//! keep (`attach`, `logs`, `respawn`) parse and say so on stderr, exit 1.
//!
//! `docs/claude-code-parity.md` has the table.

mod auth;
mod mcp_cmd;
mod misc;
mod plugin_cmd;

use anyhow::Result;

pub use mcp_cmd::{parse_env, parse_header};

/// Where an MCP server is declared. Same three words as Claude Code.
///
/// `local` is private to you in this project (kept in
/// `~/.wizard/local-mcp.json`), `user` is every project (`~/.wizard/mcp.toml`),
/// `project` is shared through the repository (`.mcp.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scope {
    Local,
    User,
    Project,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Local => "local",
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// `-t` values. `sse` parses so the refusal can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Transport {
    Stdio,
    Sse,
    Http,
}

/// `wizard mcp ...`: configure and manage MCP servers.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum McpCmd {
    /// Add an MCP server.
    ///
    /// Examples:
    ///   wizard mcp add --transport http sentry https://mcp.sentry.dev/mcp
    ///   wizard mcp add my-server -e API_KEY=xxx -- npx my-mcp-server
    Add {
        name: String,
        /// The command to run (stdio), or the URL (http).
        #[arg(value_name = "commandOrUrl")]
        command_or_url: String,
        /// Arguments for the command.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Where to declare it: local (this project, private), user, or
        /// project (`.mcp.json`).
        #[arg(short = 's', long, value_enum, default_value = "local")]
        scope: Scope,
        /// stdio (default), http. `sse` is refused: Wizard has no SSE client.
        #[arg(short = 't', long, value_enum)]
        transport: Option<Transport>,
        /// Environment variables for the server process: -e KEY=value.
        #[arg(short = 'e', long, num_args = 1..)]
        env: Vec<String>,
        /// Headers for an http server: -H "X-Api-Key: abc123".
        #[arg(short = 'H', long, num_args = 1..)]
        header: Vec<String>,
        /// Accepted for Claude Code compatibility; Wizard has no MCP OAuth
        /// flow, so it is ignored with a warning.
        #[arg(long, value_name = "port")]
        callback_port: Option<u16>,
        /// As above.
        #[arg(long, value_name = "clientId")]
        client_id: Option<String>,
        /// As above.
        #[arg(long)]
        client_secret: bool,
    },

    /// Add an MCP server (stdio or http) with a JSON string.
    AddJson {
        name: String,
        json: String,
        #[arg(short = 's', long, value_enum, default_value = "local")]
        scope: Scope,
        /// Accepted for Claude Code compatibility; ignored.
        #[arg(long)]
        client_secret: bool,
    },

    /// Import MCP servers from Claude Desktop's config file.
    AddFromClaudeDesktop {
        #[arg(short = 's', long, value_enum, default_value = "local")]
        scope: Scope,
    },

    /// Get details about an MCP server.
    Get { name: String },

    /// List configured MCP servers, with a health check for each.
    List,

    /// Authenticate with an MCP server. Wizard sends the headers you
    /// configured and has no OAuth flow, so this says so.
    Login {
        name: String,
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        client_secret: bool,
        #[arg(long)]
        callback_port: Option<u16>,
    },

    /// Clear stored OAuth credentials for an MCP server (Wizard stores none).
    Logout { name: String },

    /// Remove an MCP server.
    Remove {
        name: String,
        /// With no scope, removed from whichever scope holds it.
        #[arg(short = 's', long, value_enum)]
        scope: Option<Scope>,
    },

    /// Forget which `.mcp.json` servers this project trusted.
    ResetProjectChoices,

    /// Serve Wizard's own tools over stdio, like `wizard mcp-serve`.
    Serve {
        /// Also advertise agent-authored scripted tools from ~/.wizard/tools/.
        #[arg(long)]
        scripted: bool,
        /// Accepted for Claude Code compatibility.
        #[arg(short = 'd', long)]
        debug: bool,
        /// Accepted for Claude Code compatibility.
        #[arg(long)]
        verbose: bool,
    },
}

/// `wizard plugin ...`: Claude-format plugins and marketplaces.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum PluginCmd {
    /// List installed plugins.
    List {
        #[arg(long)]
        json: bool,
        /// Accepted; Wizard lists marketplace plugins with `marketplace list`.
        #[arg(long)]
        available: bool,
    },

    /// Install a plugin: name@marketplace, owner/repo, a git URL or a directory.
    #[command(visible_alias = "i")]
    Install {
        plugin: String,
        /// Install from this marketplace source (owner/repo, URL or path).
        #[arg(long, value_name = "source")]
        marketplace: Option<String>,
        #[arg(short = 's', long)]
        scope: Option<String>,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },

    /// Uninstall an installed plugin.
    #[command(visible_alias = "remove")]
    Uninstall {
        plugin: String,
        #[arg(short = 's', long)]
        scope: Option<String>,
        #[arg(long)]
        keep_data: bool,
        #[arg(long)]
        json: bool,
    },

    /// Enable a disabled plugin.
    Enable {
        plugin: String,
        #[arg(short = 's', long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Disable an enabled plugin.
    Disable {
        plugin: Option<String>,
        /// Disable every enabled plugin.
        #[arg(short = 'a', long)]
        all: bool,
        #[arg(short = 's', long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Update a plugin to the latest version: reinstalls it from its marketplace.
    Update {
        plugin: String,
        #[arg(short = 's', long)]
        scope: Option<String>,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },

    /// Validate a plugin or marketplace manifest.
    Validate {
        path: std::path::PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        strict: bool,
    },

    /// Show a plugin's component inventory.
    Details { name: String },

    /// Scaffold a new plugin in `./<name>`.
    #[command(visible_alias = "new")]
    Init { name: String },

    /// Tag a plugin release `<name>--v<version>` in git.
    Tag {
        /// The plugin directory (default: the current one).
        path: Option<std::path::PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        push: bool,
    },

    /// Remove auto-installed dependencies no longer needed. Wizard installs
    /// none, so there is never anything to remove.
    #[command(visible_alias = "autoremove")]
    Prune {
        #[arg(long)]
        dry_run: bool,
        #[arg(short = 's', long)]
        scope: Option<String>,
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Manage marketplaces.
    Marketplace {
        #[command(subcommand)]
        cmd: MarketplaceCmd,
    },
}

/// `wizard plugin marketplace ...`.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum MarketplaceCmd {
    /// Add a marketplace from a URL, path, or GitHub repo.
    Add {
        source: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        scope: Option<String>,
    },
    /// List configured marketplaces.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Remove a configured marketplace.
    #[command(visible_alias = "rm")]
    Remove {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Update marketplaces from their source.
    Update { name: Option<String> },
}

/// `wizard auth ...`.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum AuthCmd {
    /// Sign in. With no provider this is `wizard --login xai`; name `xai`,
    /// `chatgpt`, `claude` or `cursor` for another.
    Login {
        provider: Option<String>,
        /// Claude Code's flag: sign in with a Claude subscription, which is
        /// the `claude` provider here.
        #[arg(long)]
        claudeai: bool,
        /// Claude Code's flag: Wizard takes API keys from the environment
        /// (`ANTHROPIC_API_KEY`) or `/provider`; this says so.
        #[arg(long)]
        console: bool,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        sso: bool,
    },
    /// Sign out of a provider.
    Logout { provider: Option<String> },
    /// Show which providers are signed in. JSON unless `--text`.
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        text: bool,
    },
}

/// `wizard agents --json`: the running sessions, like `claude agents --json`.
fn agents_json(all: bool, under: Option<&std::path::Path>) -> Result<i32> {
    let rows: Vec<_> = crate::session_registry::list()
        .into_iter()
        .filter(|row| all || !row.state.is_terminal())
        .filter(|row| under.is_none_or(|dir| std::path::Path::new(&row.cwd).starts_with(dir)))
        .map(|row| {
            serde_json::json!({
                "id": row.id,
                "name": row.name,
                "cwd": row.cwd,
                "model": row.model,
                "mode": row.mode,
                "state": row.state,
                "activity": row.activity,
                "pid": row.pid,
                "startedAt": row.started_unix,
                "updatedAt": row.updated_unix,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(0)
}

/// Settle the Claude Code subcommands.
///
/// Returns `Some(exit code)` when the subcommand finished here. Returns
/// `None` after rewriting `cli` into the invocation it is a spelling of
/// (`mcp serve` is `mcp-serve`, `install` is `update`, `auth login` is
/// `--login`), so the main flow carries on with one code path for each.
pub async fn dispatch(cli: &mut crate::cli::Cli) -> Result<Option<i32>> {
    use crate::cli::Command;

    let chdir = |cli: &crate::cli::Cli| -> Result<()> {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        Ok(())
    };
    let Some(command) = cli.command.clone() else {
        return Ok(None);
    };
    let code = match command {
        Command::Mcp {
            cmd: McpCmd::Serve { scripted, .. },
        } => {
            cli.command = Some(Command::McpServe(crate::cli::McpServeCmd { scripted }));
            return Ok(None);
        }
        Command::Mcp { cmd } => {
            chdir(cli)?;
            mcp_cmd::run(cmd).await?
        }
        Command::ClaudePlugin { cmd } => {
            chdir(cli)?;
            plugin_cmd::run(cmd)?
        }
        Command::Auth {
            cmd:
                AuthCmd::Login {
                    provider,
                    claudeai,
                    console,
                    email,
                    sso,
                },
        } => {
            cli.login = Some(auth::login_provider(
                provider.as_deref(),
                claudeai,
                console,
                email.as_deref(),
                sso,
            )?);
            cli.command = None;
            return Ok(None);
        }
        Command::Auth { cmd } => auth::run(cmd)?,
        Command::SetupToken => misc::run_setup_token()?,
        Command::Install { target, force } => {
            cli.command = Some(Command::Update {
                check: false,
                to: misc::install_target(target.as_deref()),
                force,
                rollback: false,
            });
            return Ok(None);
        }
        Command::Import {
            source,
            dry_run,
            yes,
        } => misc::run_import(source.as_deref(), dry_run, yes)?,
        Command::Purge {
            path,
            all,
            dry_run,
            interactive,
            yes,
        } => misc::purge(path.as_deref(), all, dry_run, yes, interactive)?,
        Command::Stop { id } => misc::stop_session(&id)?,
        Command::Rm { id } => misc::remove_session(&id)?,
        Command::Agents {
            json: true,
            all,
            under,
        } => agents_json(all, under.as_deref())?,
        Command::Attach(_) => stub(misc::Stub::Attach),
        Command::Logs(_) => stub(misc::Stub::Logs),
        Command::Respawn(_) => stub(misc::Stub::Respawn),
        Command::AutoMode(_) => stub(misc::Stub::AutoMode),
        Command::Ultrareview(_) => stub(misc::Stub::Ultrareview),
        _ => return Ok(None),
    };
    Ok(Some(code))
}

fn stub(which: misc::Stub) -> i32 {
    eprintln!("{}", misc::stub_message(which));
    1
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied()))
            .unwrap_or_else(|err| panic!("{args:?}: {err}"))
            .command
            .expect("a subcommand")
    }

    #[test]
    fn claude_code_invocations_parse() {
        // Straight from `claude mcp add --help`.
        let Command::Mcp {
            cmd:
                super::McpCmd::Add {
                    name,
                    command_or_url,
                    args,
                    env,
                    ..
                },
        } = parse(&[
            "mcp",
            "add",
            "my-server",
            "-e",
            "API_KEY=xxx",
            "--",
            "npx",
            "-y",
            "pkg",
        ])
        else {
            panic!("not mcp add");
        };
        assert_eq!(name, "my-server");
        assert_eq!(command_or_url, "npx");
        assert_eq!(args, ["-y", "pkg"]);
        assert_eq!(env, ["API_KEY=xxx"]);

        let Command::Mcp {
            cmd: super::McpCmd::Add {
                transport, header, ..
            },
        } = parse(&[
            "mcp",
            "add",
            "--transport",
            "http",
            "corridor",
            "https://app.corridor.dev/api/mcp",
            "--header",
            "Authorization: Bearer x",
        ])
        else {
            panic!("not mcp add");
        };
        assert_eq!(transport, Some(super::Transport::Http));
        assert_eq!(header, ["Authorization: Bearer x"]);

        assert!(matches!(
            parse(&["mcp", "remove", "x", "-s", "project"]),
            Command::Mcp {
                cmd: super::McpCmd::Remove {
                    scope: Some(super::Scope::Project),
                    ..
                }
            }
        ));
        assert!(matches!(parse(&["mcp", "list"]), Command::Mcp { .. }));
        assert!(matches!(
            parse(&["mcp", "serve", "-d"]),
            Command::Mcp { .. }
        ));
        assert!(matches!(
            parse(&["plugin", "marketplace", "add", "owner/repo"]),
            Command::ClaudePlugin { .. }
        ));
        assert!(matches!(
            parse(&["plugin", "i", "p@m", "--scope", "user"]),
            Command::ClaudePlugin { .. }
        ));
        assert!(matches!(
            parse(&["auth", "status", "--text"]),
            Command::Auth { .. }
        ));
        assert!(matches!(
            parse(&["auth", "login", "--claudeai"]),
            Command::Auth { .. }
        ));
        assert!(matches!(parse(&["setup-token"]), Command::SetupToken));
        assert!(matches!(
            parse(&["upgrade", "--check"]),
            Command::Update { .. }
        ));
        assert!(matches!(parse(&["kill", "abc"]), Command::Stop { .. }));
        assert!(matches!(
            parse(&["agents", "--json", "--all"]),
            Command::Agents {
                json: true,
                all: true,
                ..
            }
        ));
        assert!(matches!(
            parse(&["ultrareview", "123", "--json"]),
            Command::Ultrareview(_)
        ));
        assert!(matches!(
            parse(&["auto-mode", "defaults"]),
            Command::AutoMode(_)
        ));
        assert!(matches!(
            parse(&["purge", "--all", "-y"]),
            Command::Purge { .. }
        ));
        assert!(matches!(
            parse(&["import", "codex", "--dry-run"]),
            Command::Import { .. }
        ));
    }

    #[test]
    fn builtins_keeps_the_name_plugin_used_to_have() {
        assert!(matches!(
            parse(&["builtins", "list"]),
            Command::Plugin { .. }
        ));
    }

    #[tokio::test]
    async fn mcp_serve_becomes_mcp_serve() {
        let mut cli = Cli::try_parse_from(["wizard", "mcp", "serve", "--scripted"]).unwrap();
        assert_eq!(super::dispatch(&mut cli).await.unwrap(), None);
        assert!(matches!(
            cli.command,
            Some(Command::McpServe(crate::cli::McpServeCmd {
                scripted: true
            }))
        ));
    }

    #[tokio::test]
    async fn install_becomes_update_and_login_becomes_the_flag() {
        let mut cli = Cli::try_parse_from(["wizard", "install", "1.2.3", "--force"]).unwrap();
        assert_eq!(super::dispatch(&mut cli).await.unwrap(), None);
        assert!(matches!(
            cli.command,
            Some(Command::Update { force: true, to: Some(ref to), .. }) if to == "v1.2.3"
        ));

        let mut cli = Cli::try_parse_from(["wizard", "auth", "login", "chatgpt"]).unwrap();
        assert_eq!(super::dispatch(&mut cli).await.unwrap(), None);
        assert_eq!(cli.login.as_deref(), Some("chatgpt"));
        assert!(cli.command.is_none());
    }
}
