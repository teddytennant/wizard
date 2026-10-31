//! `Skill`: load a skill's instructions by name.
//!
//! Wizard puts the skill list in the system prompt and the model reads the
//! file; this is the Claude Code spelling of the same step, so a prompt that
//! says "invoke the X skill" has a tool to call. Skills are read from disk on
//! each call, so one added mid-session is found without a reload.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{invalid, str_arg};
use crate::skills::{self, Skill};
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(SkillTool { roots: None })]
}

/// `Skill`.
pub struct SkillTool {
    /// Skill roots to read; `None` is the session's own set.
    roots: Option<Vec<std::path::PathBuf>>,
}

impl SkillTool {
    fn load(&self) -> Vec<Skill> {
        let roots = self.roots.clone().unwrap_or_else(skills::default_roots);
        skills::load_skills(&roots).unwrap_or_default()
    }
}

/// The skill whose name is closest to `wanted`, for "did you mean".
fn closest<'a>(wanted: &str, all: &'a [Skill]) -> Option<&'a str> {
    let wanted = wanted.to_ascii_lowercase();
    all.iter().map(|s| s.name.as_str()).find(|name| {
        let name = name.to_ascii_lowercase();
        name.contains(&wanted) || wanted.contains(&name)
    })
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        "Load a skill's instructions by name and follow them. `args` is passed through to the \
         skill. Names come from the skill list in the system prompt."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "The name of a skill from the skill list." },
                "args": { "type": "string", "description": "Optional arguments for the skill" }
            },
            "required": ["skill"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(wanted) = str_arg(&args, "skill") else {
            return Ok(invalid(self.name(), "skill is required"));
        };
        let wanted = wanted.trim().trim_start_matches('/');
        let all = self.load();
        let Some(skill) = all.iter().find(|s| s.name == wanted) else {
            // Bundled in the binary rather than on disk, like the command.
            if wanted == "workflow-authoring" {
                return Ok(ToolOutput::ok(format!(
                    "Launching skill: workflow-authoring\n\n{}",
                    crate::workflow::AUTHORING.trim()
                )));
            }
            let hint = closest(wanted, &all)
                .map(|name| format!(" Did you mean {name}?"))
                .unwrap_or_default();
            return Ok(invalid(
                self.name(),
                format!("Unknown skill: {wanted}.{hint}"),
            ));
        };
        let dir = skill
            .path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut out = format!(
            "Launching skill: {}\nBase directory for this skill: {dir}\n\n{}",
            skill.name,
            skill.body.trim()
        );
        if let Some(extra) = str_arg(&args, "args").filter(|a| !a.trim().is_empty()) {
            out.push_str(&format!("\n\nARGUMENTS: {extra}"));
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_over(dir: &std::path::Path) -> SkillTool {
        SkillTool {
            roots: Some(vec![dir.to_path_buf()]),
        }
    }

    fn write_skill(root: &std::path::Path, name: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill\n---\n{body}\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_skill_loads_by_name_with_its_arguments() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "deploy", "Run the deploy steps.");
        let ctx = ToolContext::new(dir.path());
        let out = tool_over(dir.path())
            .execute(json!({ "skill": "/deploy", "args": "prod" }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("Launching skill: deploy\n"));
        assert!(out.content.contains("Run the deploy steps."));
        assert!(out.content.ends_with("ARGUMENTS: prod"));
    }

    #[tokio::test]
    async fn an_unknown_skill_suggests_a_near_one() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "deploy", "x");
        let ctx = ToolContext::new(dir.path());
        let out = tool_over(dir.path())
            .execute(json!({ "skill": "dep" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "Tool Skill input is invalid: Unknown skill: dep. Did you mean deploy?"
        );
    }
}
