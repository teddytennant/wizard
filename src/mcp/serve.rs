//! MCP *server*: exposes Wizard's native tools over stdio as a Model Context
//! Protocol server, so any MCP client — Claude Code, Cursor, another Wizard —
//! can call Wizard's tools. This is the inverse of the client in [`super`],
//! which consumes *other* servers' tools; here Wizard is the server.
//!
//! One synchronous request loop over newline-delimited JSON-RPC on
//! stdin/stdout, answering `initialize`, `tools/list`, `tools/call`, and
//! `ping`. There is no auth, no transport negotiation, and no streaming to
//! build: a stdio server reads one request, writes one response, in order.
//! See docs/mcp.md.

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::Config;
use crate::tools::registry::ToolRegistry;
use crate::tools::{ToolContext, ToolError, ToolOutput};

use super::{McpToolInfo, PROTOCOL_VERSION};

/// Serve Wizard's native tools over stdio until stdin reaches EOF (the client
/// closed the pipe). `scripted` also advertises agent-authored scripted tools
/// from `~/.wizard/tools/`.
pub async fn run(scripted: bool) -> Result<()> {
    let mut registry = ToolRegistry::with_native_tools();
    if scripted {
        // Best-effort: a missing or malformed tools dir must not stop the
        // server from serving the native tools it already has.
        match Config::scripted_tools_dir() {
            Ok(dir) => {
                if let Err(err) = registry.load_scripted(&dir) {
                    tracing::warn!("skipping scripted tools: {err:#}");
                }
            }
            Err(err) => tracing::warn!("no scripted tools dir: {err:#}"),
        }
    }
    // The one surface that composes its own registry instead of going through
    // `agent::build_tool_registry`, so it needs its own line or an MCP client
    // would see a different tool set than the agent does from the same
    // install. Unconditional, unlike `scripted`: a plugin was installed
    // deliberately and `--scripted` gates agent-authored tools, which is a
    // different question.
    crate::plugins::install_tools_into(&mut registry);
    let ctx = ToolContext::new(std::env::current_dir()?);

    let mut reader = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break; // EOF: the client closed its end.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                // A line we cannot parse has no id to echo; JSON-RPC says to
                // answer with a null-id parse error rather than go silent.
                let response = error_response(Value::Null, -32700, &format!("parse error: {err}"));
                write_message(&mut stdout, &response).await?;
                continue;
            }
        };
        if let Some(response) = handle(&registry, &ctx, &request).await {
            write_message(&mut stdout, &response).await?;
        }
    }
    Ok(())
}

/// Answer one JSON-RPC request. Returns `None` for a notification (no `id`),
/// which the protocol forbids answering.
async fn handle(registry: &ToolRegistry, ctx: &ToolContext, request: &Value) -> Option<Value> {
    // Absence of `id` marks a notification (e.g. `notifications/initialized`):
    // never answered.
    let id = request.get("id")?.clone();
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let result = match method {
        "initialize" => Ok(initialize_result()),
        "tools/list" => Ok(tools_list_result(registry)),
        "tools/call" => tools_call(registry, ctx, request.get("params")).await,
        "ping" => Ok(json!({})),
        other => Err(RpcError::method_not_found(other)),
    };

    Some(match result {
        Ok(value) => success_response(id, value),
        Err(err) => error_response(id, err.code, &err.message),
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "wizard", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn tools_list_result(registry: &ToolRegistry) -> Value {
    // The registry already renders each tool's wire spec (name, description,
    // JSON-Schema parameters); reshape those into MCP's `inputSchema` form.
    let tools: Vec<McpToolInfo> = registry
        .specs()
        .into_iter()
        .map(|spec| McpToolInfo {
            name: spec.function.name,
            description: Some(spec.function.description),
            input_schema: spec.function.parameters,
        })
        .collect();
    json!({ "tools": tools })
}

async fn tools_call(
    registry: &ToolRegistry,
    ctx: &ToolContext,
    params: Option<&Value>,
) -> Result<Value, RpcError> {
    let params = params.ok_or_else(|| RpcError::invalid_params("missing params"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("missing tool name"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match registry.execute(name, args, ctx).await {
        // A tool that ran carries its own success/failure in `isError`; a
        // non-zero exit or missing file is a normal result, not a protocol
        // error (mirrors how the client decodes remote tool results).
        Ok(output) => Ok(encode_output(output)),
        // A ToolError means the call could not be carried out at all (unknown
        // tool, unparseable args) — that is a JSON-RPC error.
        Err(ToolError::UnknownTool(_)) => {
            Err(RpcError::invalid_params(&format!("unknown tool: {name}")))
        }
        Err(err) => Err(RpcError::internal(&err.to_string())),
    }
}

/// Encode a [`ToolOutput`] as MCP content blocks: one text block for the
/// textual output, one `image` block per attached image.
fn encode_output(output: ToolOutput) -> Value {
    let mut content: Vec<Value> = Vec::new();
    // Emit a text block whenever there is text, or when there are no images to
    // carry the result — a client expects at least one block.
    if !output.content.is_empty() || output.images.is_empty() {
        content.push(json!({ "type": "text", "text": output.content }));
    }
    for image in output.images {
        content.push(json!({
            "type": "image",
            "data": image.b64,
            "mimeType": image.mime,
        }));
    }
    json!({ "content": content, "isError": output.is_error })
}

/// A JSON-RPC error to return to the client.
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
        }
    }
    fn invalid_params(message: &str) -> Self {
        Self {
            code: -32602,
            message: message.to_string(),
        }
    }
    fn internal(message: &str) -> Self {
        Self {
            code: -32603,
            message: message.to_string(),
        }
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Write one newline-delimited JSON-RPC message to stdout (mirrors the
/// client's [`super::write_line`] framing on the other side of the pipe).
async fn write_message(stdout: &mut tokio::io::Stdout, message: &Value) -> Result<()> {
    let mut payload = serde_json::to_vec(message)?;
    payload.push(b'\n');
    stdout.write_all(&payload).await?;
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ToolRegistry {
        ToolRegistry::with_native_tools()
    }

    #[tokio::test]
    async fn initialize_returns_protocol_and_server_info() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let resp = handle(&registry(), &ctx, &req).await.expect("answered");
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], "wizard");
        assert_eq!(resp["result"]["capabilities"]["tools"], json!({}));
    }

    #[tokio::test]
    async fn notification_is_not_answered() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let req = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&registry(), &ctx, &req).await.is_none());
    }

    #[tokio::test]
    async fn tools_list_advertises_native_tools_with_schemas() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let resp = handle(&registry(), &ctx, &req).await.expect("answered");
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        assert!(tools.iter().any(|t| t["name"] == "read_file"));
        for tool in tools {
            assert!(
                tool.get("inputSchema").is_some_and(|s| s.is_object()),
                "tool {} missing an object inputSchema",
                tool["name"]
            );
        }
    }

    #[tokio::test]
    async fn tools_call_dispatches_to_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("hello.txt"), "hi there").expect("write");
        let ctx = ToolContext::new(dir.path());
        let req = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "read_file", "arguments": { "path": "hello.txt" } }
        });
        let resp = handle(&registry(), &ctx, &req).await.expect("answered");
        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("hi there"), "got: {text}");
    }

    #[tokio::test]
    async fn unknown_tool_is_a_json_rpc_error() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let req = json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "does_not_exist", "arguments": {} }
        });
        let resp = handle(&registry(), &ctx, &req).await.expect("answered");
        assert_eq!(resp["error"]["code"], json!(-32602));
        assert!(resp.get("result").is_none());
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let req = json!({ "jsonrpc": "2.0", "id": 5, "method": "resources/list" });
        let resp = handle(&registry(), &ctx, &req).await.expect("answered");
        assert_eq!(resp["error"]["code"], json!(-32601));
    }
}
