//! `ListMcpResourcesTool` and `ReadMcpResourceTool`: the MCP `resources/list`
//! and `resources/read` requests, as the two tools Claude Code names for them.
//!
//! Added to the tool list [`Connections::tools`](super::Connections::tools)
//! returns whenever at least one server is connected, as MCP tools, so the
//! default `safe` profile advertises them by name only. A server that does not
//! implement resources answers `-32601`, which listing reads as "none" and
//! reading reports as unsupported.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::McpConnection;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolKind, ToolOutput, parse_args};

/// Names the two tools register under.
pub const LIST_NAME: &str = "ListMcpResourcesTool";
pub const READ_NAME: &str = "ReadMcpResourceTool";

/// The pair, over the connections that came up.
pub fn tools(connections: &[Arc<McpConnection>]) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ListResourcesTool {
            connections: connections.to_vec(),
        }),
        Arc::new(ReadResourceTool {
            connections: connections.to_vec(),
        }),
    ]
}

fn server_names(connections: &[Arc<McpConnection>]) -> String {
    connections
        .iter()
        .map(|c| c.name())
        .collect::<Vec<_>>()
        .join(", ")
}

struct ListResourcesTool {
    connections: Vec<Arc<McpConnection>>,
}

#[async_trait]
impl Tool for ListResourcesTool {
    fn name(&self) -> &str {
        LIST_NAME
    }

    fn description(&self) -> &str {
        "List resources the connected MCP servers offer. Each entry has a `server` field. \
         Pass `server` to ask one."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "Optional server name to filter resources by" }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Mcp
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        #[derive(serde::Deserialize)]
        struct Args {
            #[serde(default)]
            server: Option<String>,
        }
        let Args { server } = parse_args(LIST_NAME, args)?;
        let targets: Vec<&Arc<McpConnection>> = match &server {
            Some(wanted) => {
                let found: Vec<_> = self
                    .connections
                    .iter()
                    .filter(|c| c.name() == wanted)
                    .collect();
                if found.is_empty() {
                    return Ok(ToolOutput::error(format!(
                        "Server \"{wanted}\" not found. Available servers: {}",
                        server_names(&self.connections)
                    )));
                }
                found
            }
            None => self.connections.iter().collect(),
        };
        let mut all: Vec<Value> = Vec::new();
        for connection in targets {
            // One server failing to list must not hide the others' resources.
            match connection.list_resources().await {
                Ok(resources) => all.extend(resources.into_iter().map(|mut r| {
                    if let Some(object) = r.as_object_mut() {
                        object.insert("server".into(), json!(connection.name()));
                    }
                    r
                })),
                Err(err) => {
                    tracing::warn!(server = connection.name(), "resources/list failed: {err:#}")
                }
            }
        }
        if all.is_empty() {
            return Ok(ToolOutput::ok(
                "No resources found. MCP servers may still provide tools even if they have no resources.",
            ));
        }
        Ok(ToolOutput::ok(
            serde_json::to_string(&all).unwrap_or_default(),
        ))
    }
}

struct ReadResourceTool {
    connections: Vec<Arc<McpConnection>>,
}

#[async_trait]
impl Tool for ReadResourceTool {
    fn name(&self) -> &str {
        READ_NAME
    }

    fn description(&self) -> &str {
        "Read one resource from an MCP server by its URI (see ListMcpResourcesTool)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "The MCP server name" },
                "uri": { "type": "string", "description": "The resource URI to read" }
            },
            "required": ["server", "uri"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Mcp
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        #[derive(serde::Deserialize)]
        struct Args {
            server: String,
            uri: String,
        }
        let Args { server, uri } = parse_args(READ_NAME, args)?;
        let Some(connection) = self.connections.iter().find(|c| c.name() == server) else {
            return Ok(ToolOutput::error(format!(
                "Server \"{server}\" not found. Available servers: {}",
                server_names(&self.connections)
            )));
        };
        match connection.read_resource(&uri).await {
            Ok(result) => Ok(ToolOutput::ok(
                serde_json::to_string(&result).unwrap_or_default(),
            )),
            Err(err) if super::is_method_not_found(&err) => Ok(ToolOutput::error(format!(
                "Server \"{server}\" does not support resources"
            ))),
            Err(err) => Ok(ToolOutput::error(format!("{err:#}"))),
        }
    }
}
