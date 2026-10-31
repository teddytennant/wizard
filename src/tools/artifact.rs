//! `Artifact`: Claude Code's page-publishing tool, kept on this machine.
//!
//! The input schema is Claude Code's (every property of its `Artifact` tool,
//! so a call written for it validates), but the page is written under
//! `~/.wizard/artifacts/<slug>/` and the link is `file://`. See
//! [`crate::artifact`] for what is honoured and what is named as unavailable.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};
use crate::artifact::Artifacts;

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ArtifactTool(None))]
}

pub struct ArtifactTool(Option<Artifacts>);

impl ArtifactTool {
    #[cfg(test)]
    pub fn at(store: Artifacts) -> Self {
        Self(Some(store))
    }
}

#[async_trait]
impl Tool for ArtifactTool {
    fn name(&self) -> &str {
        "Artifact"
    }

    fn description(&self) -> &str {
        "Publish an HTML page as a local artifact: LOCAL, not hosted. The page and its files are \
         written under ~/.wizard/artifacts/<slug>/ and you get a file:// URL (and an \
         http://127.0.0.1:8765/<slug>/ one while `wizard artifact serve` runs). Nobody else can \
         open the link. Use it when a page reads better than terminal text: a report, a chart, a \
         diagram, a small tool. Write one self-contained .html file first, with a <title> of two \
         to four words, then call publish with file_path.\n\
         \n\
         Actions (default publish): publish a file_path (with url to update that artifact in \
         place; edits={old_str,new_str} pairs change the file and republish; files maps published \
         path to a source file or to null to remove it; asset: true uploads images, fonts and \
         scripts and returns relative urls to reference), list (scope mine, files, assets), read \
         (the page, or path/paths files), open, pin, unpin, delete (url alone removes the \
         artifact, url and path one file), quickstart.\n\
         \n\
         Limits: 16MB for a page or text file, 15MB per binary file, 255 files and 64MB per \
         publish, 511 files and 256MB per artifact. Not available locally: runtime capabilities \
         (live data, shared state, viewer identity, Claude calls), artifact types, design \
         systems, comments and sharing; they are reported, not applied."
    }

    fn parameters(&self) -> Value {
        let text = |d: &str| json!({ "type": "string", "description": d });
        let flag = |d: &str| json!({ "type": "boolean", "description": d });
        let strings = |d: &str| {
            json!({ "type": "array", "items": { "type": "string" }, "description": d })
        };
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["publish", "list", "read", "delete", "open", "pin", "unpin", "quickstart", "list_files", "list_assets", "list_types", "describe_type", "read_file", "read_asset", "delete_asset", "upload_asset", "copy_from", "watch", "unwatch", "status"],
                    "description": "One of publish (default), list, read, delete, open, pin, unpin, quickstart. Claude Code 2.1.314's names list_files, list_assets, read_file, read_asset, delete_asset, upload_asset and copy_from are accepted as the matching scope or asset form; list_types and describe_type say there are no types; watch, unwatch and status say there is nothing to watch."
                },
                "after": text("list with scope assets only: continuation token. Local listings are never paged."),
                "asset": flag("publish with url: true uploads file_path (or each of file_paths) to the artifact's assets instead of publishing a page, or with from_url and asset_ids copies assets from another local artifact."),
                "asset_ids": strings("publish with asset and from_url: 1 to 10 asset ids from the source artifact."),
                "auto_open": { "type": "string", "enum": ["at_create", "after_first_write"], "description": "Accepted and ignored: artifact types do not exist locally." },
                "capabilities": { "type": "object", "description": "Runtime capabilities. Reported as not applied: a local page is static." },
                "contract": text("Accepted and ignored: there is no hosted runtime to version."),
                "description": text("One sentence saying what the page is or does; kept in the local list."),
                "edits": {
                    "type": "array",
                    "description": "Small changes to the file at file_path, applied before publishing: each old_str must occur exactly once; all or none; the source file is saved with them.",
                    "items": {
                        "type": "object",
                        "properties": { "old_str": { "type": "string" }, "new_str": { "type": "string" } },
                        "required": ["old_str", "new_str"]
                    }
                },
                "favicon": text("Deprecated; use icon."),
                "file_path": text("publish: the local .html page. With asset: the file to upload."),
                "file_paths": strings("publish with asset: up to 25 local files to upload in one call."),
                "files": {
                    "description": "Supporting files: {\"published/path\": \"source/path\" | {from} | {artifact, path} | null}, or a list of {path}. The key is what the HTML references; null removes it.",
                    "anyOf": [
                        { "type": "object" },
                        { "type": "array", "items": { "type": "object" } }
                    ]
                },
                "force": flag("Accepted and ignored: a local store has no newer remote version to overwrite."),
                "from_url": text("publish with asset: the source local artifact whose assets are copied."),
                "icon": text("One short generic word for the artifact (chart, calendar, recipe...); kept in the local list."),
                "intent": text("quickstart: accepted and ignored."),
                "label": text("A short name for this publish, at most 60 characters."),
                "limit": { "type": "integer", "description": "list: the maximum number of artifacts to return (default 25, at most 200)." },
                "out_dir": text("Accepted and ignored: files are already on disk; read names their path."),
                "overwrite_unread": strings("Accepted and ignored: a local store does not track what you read."),
                "page": flag("read: accepted and ignored."),
                "path": text("read: a published path inside the artifact (index.html is the page), or an asset id. delete: the one file or asset id to remove."),
                "paths": strings("read: several published paths in place of path."),
                "pin": flag("publish: true also pins the artifact in the local list."),
                "prompt": text("read: accepted and ignored; local reads return the raw file."),
                "root": text("Base directory that relative files sources resolve against."),
                "scope": text("list: mine (default), shared, all, types, files (with url) or assets (with url)."),
                "title": text("Fallback title when the HTML has no <title>."),
                "type": text("Accepted and ignored: artifact types do not exist locally."),
                "type_query": text("Accepted and ignored: artifact types do not exist locally."),
                "type_url": text("Not supported locally; publishing with it is refused."),
                "url": text("An existing local artifact's file:// URL, served URL or slug: publish updates it in place; read, delete, open, pin and the files/assets scopes act on it.")
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Edit
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let store = match &self.0 {
            Some(store) => store.clone(),
            None => match Artifacts::standard() {
                Ok(store) => store,
                Err(err) => return Ok(ToolOutput::error(err)),
            },
        };
        let cwd = ctx.cwd.clone();
        let result = tokio::task::spawn_blocking(move || store.call(&args, &cwd))
            .await
            .map_err(|err| ToolError::Execution {
                tool: "Artifact".to_string(),
                source: anyhow::Error::new(err),
            })?;
        Ok(match result {
            Ok(text) => ToolOutput::ok(text),
            Err(err) => ToolOutput::error(err),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The properties of Claude Code's `Artifact` input, as the real tool
    /// lists them (2.1.316's schema, which 2.1.314 shares for these).
    const CLAUDE_CODE_PROPERTIES: &[&str] = &[
        "action", "after", "asset", "asset_ids", "auto_open", "capabilities", "contract",
        "description", "edits", "favicon", "file_path", "file_paths", "files", "force",
        "from_url", "icon", "intent", "label", "limit", "out_dir", "overwrite_unread", "page",
        "path", "paths", "pin", "prompt", "root", "scope", "title", "type", "type_query",
        "type_url", "url",
    ];

    #[test]
    fn the_schema_accepts_every_property_claude_code_does_and_requires_none() {
        let tool = ArtifactTool(None);
        let schema = tool.parameters();
        let ours = schema["properties"].as_object().unwrap();
        for prop in CLAUDE_CODE_PROPERTIES {
            assert!(ours.contains_key(*prop), "missing {prop}");
        }
        assert!(schema.get("required").is_none());
        assert!(tool.deferred_by_default());
    }

    #[tokio::test]
    async fn a_call_publishes_through_the_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ArtifactTool::at(Artifacts::at(tmp.path().join("store")));
        std::fs::write(tmp.path().join("p.html"), "<title>Hello Page</title>").unwrap();
        let ctx = ToolContext::new(tmp.path());
        let out = tool
            .execute(json!({"file_path": "p.html"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("not hosted"), "{}", out.content);
        let out = tool
            .execute(json!({"action": "list"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("Hello Page"), "{}", out.content);
        let out = tool
            .execute(json!({"action": "nope"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
