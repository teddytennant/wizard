//! `.claude/agents/*.md` as Wizard subagents.
//!
//! A Claude Code agent is a markdown file: frontmatter (`name`,
//! `description`, `tools`, `model`) and a body that is the system prompt.
//! `tools` names Claude Code tools; they map to Wizard's by purpose. `model`
//! and the other keys have no per-subagent equivalent and are ignored.

use std::path::Path;

use crate::agent::subagent::SubagentConfig;

/// The Wizard tool that does what Claude Code's `claude_tool` does, if there is
/// one.
fn wizard_tool(claude_tool: &str) -> Option<&'static str> {
    // `Bash(git *)` style scoped entries name the tool before the paren.
    let base = claude_tool.split('(').next().unwrap_or(claude_tool).trim();
    Some(match base {
        "Read" | "NotebookRead" => "read_file",
        "Grep" | "Glob" => "search_files",
        "LS" => "list_files",
        "Edit" | "MultiEdit" | "NotebookEdit" => "edit_file",
        "Write" => "write_file",
        "Bash" | "BashOutput" => "execute",
        _ => return None,
    })
}

/// The tool scope a `tools:` value asks for. `None` (the parent's full set)
/// when the key is absent or names nothing Wizard has.
fn tool_scope(tools: Option<&String>) -> Option<Vec<String>> {
    let mut scope: Vec<String> = Vec::new();
    for tool in tools?.split(',') {
        if let Some(mapped) = wizard_tool(tool.trim())
            && !scope.iter().any(|existing| existing == mapped)
        {
            scope.push(mapped.to_string());
        }
    }
    (!scope.is_empty()).then_some(scope)
}

/// Parse one agent file. `None` when it has no usable name or prompt.
pub fn parse_agent(path: &Path) -> Option<SubagentConfig> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (meta, body) = super::split_frontmatter(&raw);
    let name = meta
        .get("name")
        .cloned()
        .or_else(|| path.file_stem()?.to_str().map(str::to_string))?;
    if name.trim().is_empty() || body.trim().is_empty() {
        return None;
    }
    let value = serde_json::json!({
        "name": name,
        "description": meta.get("description").cloned().unwrap_or_default(),
        "system_prompt": body,
        "tool_scope": tool_scope(meta.get("tools")),
    });
    serde_json::from_value(value).ok()
}

/// Every agent under `dir`, in file-name order.
pub fn load_dir(dir: &Path) -> Vec<SubagentConfig> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    paths.sort();
    paths.iter().filter_map(|path| parse_agent(path)).collect()
}

/// Agents from the user's and the project's `.claude/agents`, plus those
/// shipped by enabled Claude-format plugins, lowest priority first.
pub fn available(project_root: &Path) -> Vec<SubagentConfig> {
    let mut dirs = super::plugins::component_dirs(project_root, "agents");
    dirs.extend(super::dirs_for(project_root, "agents"));
    let mut agents: Vec<SubagentConfig> = dirs.iter().flat_map(|dir| load_dir(dir)).collect();
    if let Some(json) = std::env::var_os(crate::cli_claude::env::EXTRA_AGENTS) {
        agents.extend(from_json(&json.to_string_lossy()));
    }
    agents
}

/// `--agents '{"reviewer": {"description": "...", "prompt": "...", "tools": ["Read"]}}'`.
pub fn from_json(json: &str) -> Vec<SubagentConfig> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(json) else {
        tracing::warn!("--agents is not a JSON object; ignored");
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, def)| {
            let prompt = def.get("prompt").and_then(|p| p.as_str())?;
            let tools = match def.get("tools") {
                Some(serde_json::Value::Array(list)) => Some(
                    list.iter()
                        .filter_map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                Some(serde_json::Value::String(text)) => Some(text.clone()),
                _ => None,
            };
            let value = serde_json::json!({
                "name": name,
                "description": def.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                "system_prompt": prompt,
                "tool_scope": tool_scope(tools.as_ref()),
            });
            serde_json::from_value(value).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::testing::Tmp;
    use super::*;

    #[test]
    fn a_claude_agent_becomes_a_subagent_with_mapped_tools() {
        let tmp = Tmp::new("agent");
        let path = tmp.write(
            "rev.md",
            "---\nname: rev\ndescription: reviews code\ntools: Read, Grep, Bash(git diff:*), WebFetch\nmodel: sonnet\n---\nYou review.",
        );
        let agent = parse_agent(&path).expect("parses");
        assert_eq!(agent.name, "rev");
        assert_eq!(agent.description, "reviews code");
        assert_eq!(agent.system_prompt, "You review.");
        assert_eq!(
            agent.tool_scope.as_deref(),
            Some(&["read_file", "search_files", "execute"].map(String::from)[..]),
            "WebFetch has no Wizard name and is dropped"
        );
    }

    #[test]
    fn no_tools_key_means_the_full_set_and_the_file_stem_names_it() {
        let tmp = Tmp::new("agent2");
        let path = tmp.write("helper.md", "---\ndescription: d\n---\nDo it.");
        let agent = parse_agent(&path).expect("parses");
        assert_eq!(agent.name, "helper");
        assert!(agent.tool_scope.is_none());
    }

    #[test]
    fn the_agents_flag_defines_subagents_from_json() {
        let agents = from_json(
            r#"{"reviewer":{"description":"Reviews code","prompt":"You review.","tools":["Read","Grep"]},
                "broken":{"description":"no prompt"}}"#,
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "reviewer");
        assert_eq!(
            agents[0].tool_scope.as_deref(),
            Some(&["read_file", "search_files"].map(String::from)[..])
        );
        assert!(from_json("not json").is_empty());
    }

    #[test]
    fn an_empty_prompt_is_not_an_agent() {
        let tmp = Tmp::new("agent3");
        let path = tmp.write("x.md", "---\nname: x\n---\n");
        assert!(parse_agent(&path).is_none());
    }
}
