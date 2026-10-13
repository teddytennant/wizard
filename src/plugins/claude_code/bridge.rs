//! The loopback MCP server a `claude` child calls Wizard's tools through.
//!
//! Claude Code only lets a model call tools it knows how to run, and with
//! `--tools ""` the only ones left are whatever an MCP server offers. So Wizard
//! offers its own: `tools/list` answers with the request's tool specs, and a
//! `tools/call` is held open until the agent loop has run the tool itself and
//! come back with the result. Nothing is executed here.
//!
//! It is plain HTTP on `127.0.0.1`, one JSON-RPC message per POST, behind a
//! bearer token only the child is given.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::llm::ToolSpec;

/// The name Claude Code knows the server by; its tools reach the model as
/// `mcp__wizard__<name>`.
pub const SERVER: &str = "wizard";
pub const TOOL_PREFIX: &str = "mcp__wizard__";

#[derive(Default)]
struct Calls {
    /// Results the agent loop has produced that Claude Code has not asked for
    /// yet. It dispatches a turn's calls one at a time.
    ready: HashMap<String, Value>,
    waiting: HashMap<String, oneshot::Sender<Value>>,
}

pub struct Bridge {
    port: u16,
    token: String,
    calls: Arc<Mutex<Calls>>,
    server: JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Bridge {
    pub async fn start(tools: &[ToolSpec]) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("opening the tool bridge for Claude Code")?;
        let port = listener.local_addr()?.port();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let calls = Arc::new(Mutex::new(Calls::default()));
        let tools = Arc::new(json!({"tools": tools.iter().map(describe).collect::<Vec<_>>()}));

        let server = tokio::spawn({
            let (token, calls) = (token.clone(), calls.clone());
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (token, calls, tools) = (token.clone(), calls.clone(), tools.clone());
                    tokio::spawn(async move {
                        let _ = serve(stream, &token, &calls, &tools).await;
                    });
                }
            }
        });
        Ok(Self {
            port,
            token,
            calls,
            server,
        })
    }

    /// What `--mcp-config` is given.
    pub fn config(&self) -> Value {
        json!({"mcpServers": {SERVER: {
            "type": "http",
            "url": format!("http://127.0.0.1:{}/mcp", self.port),
            "headers": {"Authorization": format!("Bearer {}", self.token)},
        }}})
    }

    /// Hand the result of tool call `id` to Claude Code, now or when it asks.
    pub fn answer(&self, id: &str, content: Vec<Value>) {
        let result = json!({"content": content});
        let mut calls = self.calls.lock().expect("bridge lock");
        match calls.waiting.remove(id) {
            Some(waiter) => {
                let _ = waiter.send(result);
            }
            None => {
                calls.ready.insert(id.to_string(), result);
            }
        }
    }
}

fn describe(tool: &ToolSpec) -> Value {
    json!({
        "name": tool.function.name,
        "description": tool.function.description,
        "inputSchema": tool.function.parameters,
    })
}

async fn serve(stream: TcpStream, token: &str, calls: &Mutex<Calls>, tools: &Value) -> Result<()> {
    let mut stream = BufReader::new(stream);
    let mut request_line = String::new();
    stream.read_line(&mut request_line).await?;
    let (mut length, mut authorized) = (0usize, false);
    loop {
        let mut header = String::new();
        if stream.read_line(&mut header).await? == 0 || header.trim().is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap_or(0),
            "authorization" => authorized = value.trim() == format!("Bearer {token}"),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;

    let (status, reply) = if !authorized {
        ("403 Forbidden", None)
    } else if !request_line.starts_with("POST ") {
        // No server-to-client stream is offered.
        ("405 Method Not Allowed", None)
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(message) if message.get("id").is_some() => {
                let outcome = handle(&message, calls, tools).await;
                let mut reply = json!({"jsonrpc": "2.0", "id": message["id"]});
                match outcome {
                    Ok(result) => reply["result"] = result,
                    Err(error) => reply["error"] = error,
                }
                ("200 OK", Some(reply.to_string()))
            }
            Ok(_) => ("202 Accepted", None),
            Err(_) => ("400 Bad Request", None),
        }
    };
    let reply = reply.unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        reply.len()
    );
    let stream = stream.get_mut();
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(reply.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn handle(message: &Value, calls: &Mutex<Calls>, tools: &Value) -> Result<Value, Value> {
    let params = &message["params"];
    match message["method"].as_str() {
        Some("initialize") => Ok(json!({
            "protocolVersion": params["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER, "version": env!("CARGO_PKG_VERSION")},
        })),
        Some("tools/list") => Ok(tools.clone()),
        Some("tools/call") => {
            let Some(id) = params["_meta"]["claudecode/toolUseId"].as_str() else {
                return Err(json!({"code": -32602, "message": "the call names no tool use id"}));
            };
            let waiter = {
                let mut calls = calls.lock().expect("bridge lock");
                match calls.ready.remove(id) {
                    Some(result) => return Ok(result),
                    None => {
                        let (sender, waiter) = oneshot::channel();
                        calls.waiting.insert(id.to_string(), sender);
                        waiter
                    }
                }
            };
            waiter
                .await
                .map_err(|_| json!({"code": -32603, "message": "Wizard dropped the call"}))
        }
        Some("ping") => Ok(json!({})),
        _ => Err(json!({"code": -32601, "message": "method not found"})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn post(bridge: &Bridge, token: &str, body: Value) -> (u16, Value) {
        let config = bridge.config();
        let url = config["mcpServers"][SERVER]["url"].as_str().expect("url");
        let response = reqwest::Client::new()
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .body(body.to_string())
            .send()
            .await
            .expect("the bridge answers");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    fn call(id: u64, tool_use: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "read_file",
            "arguments": {"path": "a"},
            "_meta": {"claudecode/toolUseId": tool_use},
        }})
    }

    async fn bridge() -> Bridge {
        let tool = ToolSpec::function("read_file", "Read a file.", json!({"type": "object"}));
        Bridge::start(&[tool]).await.expect("starts")
    }

    #[tokio::test]
    async fn it_lists_the_requests_tools_to_whoever_holds_the_token() {
        let bridge = bridge().await;
        let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});

        let (status, reply) = post(&bridge, &bridge.token, list.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(
            reply["result"]["tools"],
            json!([{
                "name": "read_file",
                "description": "Read a file.",
                "inputSchema": {"type": "object"},
            }])
        );

        let (status, _) = post(&bridge, "not-the-token", list).await;
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn a_call_waits_for_the_result_whichever_comes_first() {
        let bridge = bridge().await;
        let text = |text: &str| vec![json!({"type": "text", "text": text})];

        bridge.answer("early", text("already run"));
        let (_, reply) = post(&bridge, &bridge.token, call(2, "early")).await;
        assert_eq!(reply["result"]["content"][0]["text"], "already run");

        let (reply, ()) = tokio::join!(post(&bridge, &bridge.token, call(3, "late")), async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            bridge.answer("late", text("run meanwhile"));
        });
        assert_eq!(reply.1["id"], 3);
        assert_eq!(reply.1["result"]["content"][0]["text"], "run meanwhile");
    }

    #[tokio::test]
    async fn a_notification_is_accepted_and_an_unknown_method_is_an_error() {
        let bridge = bridge().await;
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(post(&bridge, &bridge.token, note).await.0, 202);

        let probe = json!({"jsonrpc": "2.0", "id": "p", "method": "server/discover"});
        let (_, reply) = post(&bridge, &bridge.token, probe).await;
        assert_eq!(reply["error"]["code"], -32601);
    }
}
