//! Native `memory` tool: persist project facts across sessions.
//!
//! Backed by `crate::memory::MemoryStore` (one markdown file per memory
//! under `~/.wizard/memory/<project-slug>/`). The index of saved memories is
//! injected into the system prompt at session start, so a save made now is
//! recalled automatically next session. Confined to `~/.wizard/memory`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::memory::{MemoryStore, MemoryType};

use super::{
    MAX_OUTPUT_BYTES, Tool, ToolContext, ToolError, ToolOutput, parse_args, truncate_output,
};

/// Arguments for [`MemoryTool`].
#[derive(Debug, Deserialize)]
pub struct MemoryArgs {
    /// One of `save`, `read`, `delete`.
    pub action: String,
    /// Kebab-case memory name (the file is `<name>.md`).
    pub name: String,
    /// What the memory is about: `user`, `feedback`, `project`, or
    /// `reference` (save only). Named `type` on the wire.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// One-line summary shown in the index (save only).
    #[serde(default)]
    pub description: Option<String>,
    /// The fact to remember (save only).
    #[serde(default)]
    pub content: Option<String>,
}

/// `memory` — save, read, or delete a persistent per-project memory.
pub struct MemoryTool;

impl MemoryTool {
    /// Require an argument that only some actions take, with a usage hint.
    fn require<'a>(
        &self,
        value: Option<&'a str>,
        field: &str,
        action: &str,
    ) -> Result<&'a str, ToolError> {
        value.ok_or_else(|| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: format!("action '{action}' requires '{field}'"),
        })
    }

    /// The memory type for a save, rejecting an unknown one with the four
    /// spellings the model may use.
    fn kind(&self, args: &MemoryArgs) -> Result<MemoryType, ToolError> {
        let raw = self.require(args.kind.as_deref(), "type", "save")?;
        raw.parse()
            .map_err(|err: anyhow::Error| ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: err.to_string(),
            })
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    /// The pointer to the memory rules is load-bearing and has already dangled
    /// once: it named a system-prompt section after that section's rules had
    /// moved behind the `manual` lookup, so the rules were reachable from
    /// nowhere. `the_description_sends_the_model_somewhere_that_exists` now
    /// follows it. Point it at a real destination or inline the rule, never at
    /// a place that "should" carry it.
    fn description(&self) -> &str {
        "Persist a fact about this project or its user across sessions. Actions: \
         'save' (record or update a durable fact: needs type, description, content), \
         'read' (full body plus linked memories), 'delete' (drop a wrong/obsolete one). \
         Types: user, feedback, project, reference. Before you save or delete, read \
         `manual` topic `memory`: it says what earns a place and what must never be \
         written down. Names are kebab-case; descriptions are one line."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["save", "read", "delete"], "description": "What to do with the memory" },
                "name": { "type": "string", "description": "Kebab-case memory name (lowercase letters, digits, hyphens). Reuse an existing name to update that memory in place." },
                "type": {
                    "type": "string",
                    "enum": ["user", "feedback", "project", "reference"],
                    "description": "What the memory is about (required for save): 'user' who the user is; 'feedback' how you should work, with the why; 'project' ongoing work, goals, constraints, dates absolute; 'reference' a pointer to an external resource"
                },
                "description": { "type": "string", "description": "One-line summary shown in the memory index (required for save)" },
                "content": { "type": "string", "description": "The fact to remember, markdown allowed, [[links]] to related memories allowed (required for save)" }
            },
            "required": ["action", "name"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: MemoryArgs = parse_args(self.name(), args)?;
        let store = MemoryStore::open_auto(&ctx.cwd).map_err(|err| ToolError::Execution {
            tool: self.name().to_string(),
            source: err,
        })?;

        match args.action.as_str() {
            "save" => {
                let kind = self.kind(&args)?;
                let description =
                    self.require(args.description.as_deref(), "description", "save")?;
                let content = self.require(args.content.as_deref(), "content", "save")?;
                match store.save(&args.name, kind, description, content) {
                    Ok(()) => Ok(ToolOutput::ok(format!(
                        "Saved memory '{}' ({kind}). It will appear in the system prompt's memory index next session.",
                        args.name
                    ))),
                    Err(err) => Ok(ToolOutput::error(format!("{err:#}"))),
                }
            }
            "read" => match store.read(&args.name) {
                Ok(contents) => {
                    let output = format!("{contents}{}", link_trailer(&store, &contents));
                    Ok(ToolOutput::ok(truncate_output(output, MAX_OUTPUT_BYTES)))
                }
                Err(err) => Ok(ToolOutput::error(format!("{err:#}"))),
            },
            "delete" => match store.delete(&args.name) {
                Ok(()) => Ok(ToolOutput::ok(format!("Deleted memory '{}'.", args.name))),
                Err(err) => Ok(ToolOutput::error(format!("{err:#}"))),
            },
            other => Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: format!("unknown action '{other}' (save|read|delete)"),
            }),
        }
    }
}

/// The `[[links]]` a memory points at, appended to a `read` so a recall can
/// follow them: each saved link is one more `read` away, and each unsaved one
/// is a memory worth writing. Empty when the body links nowhere.
fn link_trailer(store: &MemoryStore, contents: &str) -> String {
    let links = store.links(contents);
    if links.is_empty() {
        return String::new();
    }
    let mut trailer = String::from("\nLinks:\n");
    for link in links {
        let state = if link.saved {
            "saved — read it to follow the link"
        } else {
            "not saved yet"
        };
        trailer.push_str(&format!("- {} ({state})\n", link.name));
    }
    trailer
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Temp project dir removed on drop, along with the memory store dir
    /// derived from it under `~/.wizard/memory/`.
    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(&self.0)
        }

        fn store(&self) -> MemoryStore {
            MemoryStore::open(&self.0).expect("open store")
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.store().dir());
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Follow the pointer the description hands the model.
    ///
    /// This description used to delegate the memory rules to "the system prompt
    /// Memory section" after those rules had moved behind the `manual` lookup,
    /// so the chain ran description -> prompt -> nothing and the rules that keep
    /// the store from becoming a junk drawer were unreachable. A pointer nobody
    /// walks is a pointer that rots.
    #[test]
    fn the_description_sends_the_model_somewhere_that_exists() {
        let description = MemoryTool.description();
        assert!(
            !description.contains("system prompt Memory section"),
            "the rules do not live in the prompt any more"
        );

        const LEAD: &str = "`manual` topic `";
        let start = description
            .find(LEAD)
            .expect("the description must say where the memory rules are")
            + LEAD.len();
        let tail = &description[start..];
        let topic = &tail[..tail.find('`').expect("the topic id is closed")];

        let page = crate::agent::prompts::manual_page(topic).unwrap_or_else(|| {
            panic!(
                "the memory tool sends the model to `manual` topic {topic:?}, which no page serves"
            )
        });
        assert!(
            page.body.contains("A memory has to earn its place"),
            "topic {topic:?} resolves, but not to the rules: {}",
            page.title
        );
    }

    #[tokio::test]
    async fn save_read_delete_dispatch() {
        let tmp = TempProject::new();

        let out = MemoryTool
            .execute(
                json!({
                    "action": "save",
                    "name": "test-style",
                    "type": "feedback",
                    "description": "tests use TempDir",
                    "content": "Unit tests build their own temp dirs."
                }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Saved memory 'test-style' (feedback)"));
        let entries = tmp.store().list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, MemoryType::Feedback);

        let out = MemoryTool
            .execute(
                json!({ "action": "read", "name": "test-style" }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("description: tests use TempDir"));
        assert!(out.content.contains("type: feedback"));
        assert!(
            out.content
                .contains("Unit tests build their own temp dirs.")
        );

        let out = MemoryTool
            .execute(
                json!({ "action": "delete", "name": "test-style" }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(tmp.store().list().unwrap().is_empty());
    }

    /// A `read` reports the memories the body links to, so the model can
    /// follow them — including the ones nobody has written yet.
    #[tokio::test]
    async fn read_reports_the_links_a_memory_points_at() {
        let tmp = TempProject::new();
        tmp.store()
            .save(
                "deploy-flow",
                MemoryType::Project,
                "how we ship",
                "Tagged releases only.",
            )
            .unwrap();
        tmp.store()
            .save(
                "ci-gates",
                MemoryType::Project,
                "what CI enforces",
                "Blocks a merge until [[deploy-flow]] is green; see [[release-notes]].",
            )
            .unwrap();

        let out = MemoryTool
            .execute(json!({ "action": "read", "name": "ci-gates" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content
                .contains("- deploy-flow (saved — read it to follow the link)"),
            "a saved link is one read away: {}",
            out.content
        );
        assert!(
            out.content.contains("- release-notes (not saved yet)"),
            "a dangling link is a memory worth writing, not an error: {}",
            out.content
        );

        // A memory that links nowhere gets no trailer.
        let out = MemoryTool
            .execute(
                json!({ "action": "read", "name": "deploy-flow" }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.content.contains("Links:"));
    }

    #[tokio::test]
    async fn read_missing_memory_is_a_tool_output_error() {
        let tmp = TempProject::new();
        let out = MemoryTool
            .execute(json!({ "action": "read", "name": "absent" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no memory named 'absent'"));
    }

    #[tokio::test]
    async fn save_requires_type_description_and_content() {
        let tmp = TempProject::new();
        for incomplete in [
            json!({ "action": "save", "name": "incomplete" }),
            json!({ "action": "save", "name": "incomplete", "type": "user" }),
            json!({ "action": "save", "name": "incomplete", "type": "user", "description": "d" }),
        ] {
            let err = MemoryTool
                .execute(incomplete.clone(), &tmp.ctx())
                .await
                .expect_err("an incomplete save must be rejected: {incomplete}");
            assert!(matches!(err, ToolError::InvalidArgs { .. }));
        }
        assert!(tmp.store().list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_rejects_an_unknown_type() {
        let tmp = TempProject::new();
        let err = MemoryTool
            .execute(
                json!({
                    "action": "save",
                    "name": "mystery",
                    "type": "archived",
                    "description": "d",
                    "content": "c"
                }),
                &tmp.ctx(),
            )
            .await
            .expect_err("an unknown type must be rejected");
        assert!(
            matches!(&err, ToolError::InvalidArgs { message, .. } if message.contains("user|feedback|project|reference")),
            "the error names the types the model may use: {err}"
        );
    }

    #[tokio::test]
    async fn unknown_action_is_invalid_args() {
        let tmp = TempProject::new();
        let err = MemoryTool
            .execute(json!({ "action": "forget", "name": "x" }), &tmp.ctx())
            .await
            .expect_err("unknown action must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn traversal_name_is_rejected_without_touching_disk() {
        let tmp = TempProject::new();
        let out = MemoryTool
            .execute(
                json!({
                    "action": "save",
                    "name": "../evil",
                    "type": "project",
                    "description": "d",
                    "content": "c"
                }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("kebab-case"));
    }
}
