//! What the `system` init line lists, read off the live agent and config.

use std::path::Path;

use serde_json::json;

use super::{Price, RunInfo};
use crate::config::Config;
use crate::hooks::toolmap;

/// Flags that change the stream, as the headless runner reads them.
#[derive(Debug, Clone, Default)]
pub struct StreamFlags {
    pub partial: bool,
    pub hook_events: bool,
    pub max_budget_usd: Option<f64>,
    pub max_turns: Option<u32>,
    pub additional_directories: Vec<String>,
}

impl StreamFlags {
    pub fn from_flags(flags: &crate::cli_claude::ClaudeFlags) -> Self {
        Self {
            partial: flags.include_partial_messages,
            hook_events: flags.include_hook_events,
            max_budget_usd: flags.max_budget_usd,
            max_turns: flags.max_turns,
            additional_directories: flags
                .add_dir
                .iter()
                .map(|dir| dir.display().to_string())
                .collect(),
        }
    }
}

/// Build the init details for a run. `tools` are Wizard's tool names; they are
/// listed under their Claude Code names, the ones the `tool_use` blocks carry.
pub fn describe(
    config: &Config,
    project_root: &Path,
    session_id: &str,
    tools: &[String],
    flags: StreamFlags,
) -> RunInfo {
    let active = config.active();
    let mut skills: Vec<String> = crate::agent::load_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    skills.sort();
    let mut slash_commands: Vec<String> = crate::commands::load(project_root)
        .into_iter()
        .map(|command| command.name)
        .chain(skills.iter().cloned())
        .collect();
    slash_commands.sort();
    slash_commands.dedup();
    let mut agents: Vec<String> = crate::agent::subagent::builtin_configs()
        .into_iter()
        .chain(crate::claude_dir::agents::available(project_root))
        .map(|agent| agent.name)
        .collect();
    agents.sort();
    agents.dedup();
    let plugins = crate::claude_dir::plugins::enabled(project_root)
        .into_iter()
        .map(|plugin| {
            json!({
                "name": plugin.name,
                "path": plugin.root.display().to_string(),
                "source": plugin.origin,
            })
        })
        .collect();
    let mcp_servers = Config::mcp_config_path()
        .and_then(|path| crate::mcp::McpConfig::load(&path))
        .map(|mcp| {
            mcp.servers
                .iter()
                .map(|server| json!({"name": server.name, "status": "configured"}))
                .collect()
        })
        .unwrap_or_default();
    let mode = config
        .run_permissions
        .mode
        .or(config.permissions.mode)
        .unwrap_or_default();
    RunInfo {
        session_id: session_id.to_string(),
        cwd: project_root.display().to_string(),
        model: active.model.clone(),
        permission_mode: mode.as_str().to_string(),
        output_style: crate::claude_dir::styles::active_name(project_root),
        tools: tools
            .iter()
            .map(|tool| toolmap::claude_name(tool))
            .collect(),
        slash_commands,
        skills,
        agents,
        plugins,
        mcp_servers,
        price: Price {
            endpoint: active.base_url.clone(),
            usd_per_mtok_in: active.usd_per_mtok_in,
            usd_per_mtok_out: active.usd_per_mtok_out,
            self_hosted: crate::usage::self_hosted(&active.kind),
        },
        partial: flags.partial,
        hook_events: flags.hook_events,
        max_budget_usd: flags.max_budget_usd,
        max_turns: flags.max_turns,
        additional_directories: flags.additional_directories,
    }
}
