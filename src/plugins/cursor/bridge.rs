//! The loopback MCP server a `cursor-agent` child calls Wizard's tools through.
//!
//! Cursor only lets a model call tools it knows how to run, and its own are
//! denied in the workspace config, so the only ones left are what an MCP server
//! offers. So Wizard offers its own: `tools/list` answers with the request's
//! tool specs, and a `tools/call` is held open until the agent loop has run the
//! tool itself and come back with the result. Nothing is executed here.
//!
//! Cursor puts no tool use id on a call, so each one gets an id here and the
//! caller hands results back by it. Plain HTTP on `127.0.0.1`, one JSON-RPC
//! message per POST, behind a bearer token only the child is given.
//!
//! Cursor gives up on a call after about a minute and the model then tries it
//! again, which would run a slow tool twice. So a call is held for
//! [`HOLD_LIMIT`] at most and then answered with a note that the tool is still
//! running; the bridge is [`expired`](Bridge::expired) from then on, because
//! the result the loop brings back has nowhere to go.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::llm::ToolSpec;

/// The name Cursor knows the server by.
pub const SERVER: &str = "wizard";

const HOLD_LIMIT: Duration = Duration::from_secs(50);

const STILL_RUNNING: &str = "This tool is still running. Do not call it again; its result will \
     be sent to you in the next message. End your turn now.";

/// What every connection to the server shares.
struct Shared {
    token: String,
    tools: Value,
    calls: mpsc::UnboundedSender<Call>,
    hold: Duration,
    expired: AtomicBool,
}

/// A `tools/call` waiting for its result.
pub struct Call {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    reply: oneshot::Sender<Value>,
}

impl Call {
    /// Hand the call's result to Cursor.
    pub fn answer(self, content: Vec<Value>) {
        let _ = self.reply.send(json!({"content": content}));
    }
}

pub struct Bridge {
    port: u16,
    shared: Arc<Shared>,
    pub calls: mpsc::UnboundedReceiver<Call>,
    server: JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Bridge {
    pub async fn start(tools: &[ToolSpec]) -> Result<Self> {
        Self::start_holding(tools, HOLD_LIMIT).await
    }

    async fn start_holding(tools: &[ToolSpec], hold: Duration) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("opening the tool bridge for Cursor")?;
        let port = listener.local_addr()?.port();
        let (sender, calls) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            token: uuid::Uuid::new_v4().simple().to_string(),
            tools: json!({"tools": tools.iter().map(describe).collect::<Vec<_>>()}),
            calls: sender,
            hold,
            expired: AtomicBool::new(false),
        });

        let server = tokio::spawn({
            let shared = shared.clone();
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        let _ = serve(stream, &shared).await;
                    });
                }
            }
        });
        Ok(Self {
            port,
            shared,
            calls,
            server,
        })
    }

    /// Whether a call outlasted [`HOLD_LIMIT`] and was answered without its
    /// result.
    pub fn expired(&self) -> bool {
        self.shared.expired.load(Ordering::Relaxed)
    }

    /// What goes in the workspace's `.cursor/mcp.json`.
    pub fn config(&self) -> Value {
        json!({"mcpServers": {SERVER: {
            "url": format!("http://127.0.0.1:{}/mcp", self.port),
            "headers": {"Authorization": format!("Bearer {}", self.shared.token)},
        }}})
    }
}

fn describe(tool: &ToolSpec) -> Value {
    json!({
        "name": tool.function.name,
        "description": tool.function.description,
        "inputSchema": tool.function.parameters,
    })
}

async fn serve(stream: TcpStream, shared: &Shared) -> Result<()> {
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
            "authorization" => authorized = value.trim() == format!("Bearer {}", shared.token),
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
                let outcome = handle(&message, shared).await;
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

async fn handle(message: &Value, shared: &Shared) -> Result<Value, Value> {
    let params = &message["params"];
    match message["method"].as_str() {
        Some("initialize") => Ok(json!({
            "protocolVersion": params["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER, "version": env!("CARGO_PKG_VERSION")},
        })),
        Some("tools/list") => Ok(shared.tools.clone()),
        Some("tools/call") => {
            let Some(name) = params["name"].as_str() else {
                return Err(json!({"code": -32602, "message": "the call names no tool"}));
            };
            let (reply, waiter) = oneshot::channel();
            let call = Call {
                id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                name: name.to_string(),
                arguments: params["arguments"].clone(),
                reply,
            };
            let dropped = || json!({"code": -32603, "message": "Wizard dropped the call"});
            shared.calls.send(call).map_err(|_| dropped())?;
            match tokio::time::timeout(shared.hold, waiter).await {
                Ok(result) => result.map_err(|_| dropped()),
                Err(_) => {
                    shared.expired.store(true, Ordering::Relaxed);
                    Ok(json!({"content": [{"type": "text", "text": STILL_RUNNING}]}))
                }
            }
        }
        Some("ping") => Ok(json!({})),
        _ => Err(json!({"code": -32601, "message": "method not found"})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(bridge: &Bridge) -> String {
        let config = bridge.config();
        config["mcpServers"][SERVER]["url"]
            .as_str()
            .expect("url")
            .to_string()
    }

    async fn post(url: &str, token: &str, body: Value) -> (u16, Value) {
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

    async fn bridge() -> Bridge {
        let tool = ToolSpec::function("read_file", "Read a file.", json!({"type": "object"}));
        Bridge::start(&[tool]).await.expect("starts")
    }

    #[tokio::test]
    async fn it_lists_the_requests_tools_to_whoever_holds_the_token() {
        let bridge = bridge().await;
        let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});

        let (status, reply) = post(&url(&bridge), &bridge.shared.token, list.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(
            reply["result"]["tools"],
            json!([{
                "name": "read_file",
                "description": "Read a file.",
                "inputSchema": {"type": "object"},
            }])
        );

        let (status, _) = post(&url(&bridge), "not-the-token", list).await;
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn a_call_is_held_until_the_loop_answers_it() {
        let mut bridge = bridge().await;
        let call = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "read_file", "arguments": {"path": "a"},
        }});

        let (url, token) = (url(&bridge), bridge.shared.token.clone());
        let (reply, ()) = tokio::join!(post(&url, &token, call), async {
            let waiting = bridge.calls.recv().await.expect("the call arrives");
            assert_eq!(waiting.name, "read_file");
            assert_eq!(waiting.arguments, json!({"path": "a"}));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            waiting.answer(vec![json!({"type": "text", "text": "run meanwhile"})]);
        });
        assert_eq!(reply.1["id"], 3);
        assert_eq!(reply.1["result"]["content"][0]["text"], "run meanwhile");
    }

    #[tokio::test]
    async fn a_call_held_too_long_is_told_to_wait_and_marks_the_bridge_expired() {
        let tool = ToolSpec::function("run", "Run it.", json!({"type": "object"}));
        let bridge = Bridge::start_holding(&[tool], Duration::from_millis(50))
            .await
            .expect("starts");
        let call = json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": "run", "arguments": {},
        }});
        assert!(!bridge.expired());

        let (_, reply) = post(&url(&bridge), &bridge.shared.token, call).await;

        assert_eq!(reply["result"]["content"][0]["text"], STILL_RUNNING);
        assert!(bridge.expired());
    }

    #[tokio::test]
    async fn a_notification_is_accepted_and_an_unknown_method_is_an_error() {
        let bridge = bridge().await;
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(post(&url(&bridge), &bridge.shared.token, note).await.0, 202);

        let probe = json!({"jsonrpc": "2.0", "id": "p", "method": "server/discover"});
        let (_, reply) = post(&url(&bridge), &bridge.shared.token, probe).await;
        assert_eq!(reply["error"]["code"], -32601);
    }
}
