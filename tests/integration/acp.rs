//! Integration test for `wizard acp`: its stdout is the JSON-RPC transport,
//! so nothing else may ever be written there.
//!
//! The ACP server frames newline-delimited JSON-RPC on stdout (`wizard::plugins::acp`
//! hands `tokio::io::stdout()` straight to the protocol crate). Every surface
//! shares one agent-construction path (`agent::build_headless_agent_*`), and
//! that path used to `println!` two different things: the "using the JSON tool
//! protocol" notice, and the local-server progress reporter's off-terminal
//! fallback lines. Either one lands between two JSON-RPC frames and the
//! editor's parser gives up on the connection.
//!
//! The test drives a real `wizard acp` process through `initialize` and
//! `session/new` against a fake Ollama server whose answers force *both* of
//! those code paths: a model that is not pulled yet (progress lines) which
//! advertises no `tools` capability (the JSON-protocol notice). Every byte on
//! stdout must still be JSON-RPC.
//!
//! The whole file needs `provider-ollama`: the fake backend is served to a
//! `kind = "ollama"` entry, and without the plugin that kind resolves to
//! nothing, so the session fails at `build()` and never reaches the transport
//! this is watching. That degrade is asserted in
//! `plugins::a_kind_is_installed_exactly_when_its_plugin_is_compiled_in`.
//!
//! And it needs `acp`, because the server is a plugin too: without it the
//! subprocess this drives prints one sentence about the missing feature and
//! exits, so every assertion below would be about the wrong program. That
//! degrade has its own assertion in
//! `plugins::an_entrypoint_is_registered_exactly_when_its_plugin_is_compiled_in`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long the test waits for the two JSON-RPC responses. Generous: the
/// child builds a whole agent (tool registry, skills, session) on the way.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Temp dir removed on drop. Serves as both fake `$HOME` and project root.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wizard-acp-itest-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The child process, killed on drop so a failing assertion never leaks a
/// `wizard acp` that is still holding a pipe open.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A fake Ollama server on loopback, answering the three endpoints
/// `build_headless_agent` hits before the agent exists.
///
/// The answers are chosen to make the startup path as loud as it can be:
/// `/api/tags` reports nothing installed (so the configured tag is "pulled",
/// which drives the progress reporter) and `/api/show` advertises no
/// capabilities (so the tool-protocol probe comes back false).
fn spawn_fake_ollama() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            std::thread::spawn(move || serve_connection(stream));
        }
    });
    port
}

/// Answer requests on one keep-alive connection until the client hangs up.
fn serve_connection(stream: TcpStream) {
    let mut writer = stream.try_clone().expect("clone socket");
    let mut reader = BufReader::new(stream);
    loop {
        let mut request_line = String::new();
        match reader.read_line(&mut request_line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        // Drain the body so the next request on this connection starts at a
        // request line rather than in the middle of JSON.
        if let Some(len) = headers.get("content-length").and_then(|v| v.parse().ok()) {
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
        }

        let path = request_line.split_whitespace().nth(1).unwrap_or("");
        let body = match path {
            // Nothing installed: the configured tag has to be pulled.
            "/api/tags" => "{\"models\":[]}".to_string(),
            // NDJSON pull transcript: two milestones then success. Every
            // non-success line is a status line on the progress reporter.
            "/api/pull" => "{\"status\":\"pulling manifest\"}\n\
                            {\"status\":\"verifying sha256 digest\"}\n\
                            {\"status\":\"success\"}\n"
                .to_string(),
            // No `tools` capability: the agent falls back to the JSON tool
            // protocol and says so.
            "/api/show" => "{\"capabilities\":[]}".to_string(),
            _ => String::new(),
        };
        let status = if body.is_empty() {
            "404 Not Found"
        } else {
            "200 OK"
        };
        let response = format!(
            "HTTP/1.1 {status}\r\n\
             content-type: application/json\r\n\
             content-length: {}\r\n\
             \r\n{body}",
            body.len()
        );
        if writer.write_all(response.as_bytes()).is_err() || writer.flush().is_err() {
            return;
        }
    }
}

/// Point the fake home's config at the fake Ollama server.
fn write_config(home: &Path, port: u16) {
    let dir = home.join(".wizard");
    std::fs::create_dir_all(&dir).expect("create .wizard dir");
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[[providers]]\n\
             name = \"fake\"\n\
             kind = \"ollama\"\n\
             base_url = \"http://127.0.0.1:{port}\"\n\
             model = \"fake-model:test\"\n"
        ),
    )
    .expect("write config.toml");
}

#[test]
fn acp_writes_nothing_to_stdout_that_is_not_json_rpc() {
    let home = TempDir::new();
    let port = spawn_fake_ollama();
    write_config(&home.0, port);

    let mut child = Command::new(env!("CARGO_BIN_EXE_wizard"))
        .arg("acp")
        .env("HOME", &home.0)
        .env_remove("WIZARD_MODEL")
        .env_remove("WIZARD_OLLAMA_HOST")
        .env_remove("WIZARD_LLAMACPP_HOST")
        .env_remove("WIZARD_GGUF_PATH")
        .env_remove("WIZARD_SYSTEM_PROMPT")
        .env_remove("WIZARD_HARNESS_DIR")
        .current_dir(&home.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("wizard acp starts");

    // Read stdout on its own thread: the child stays alive (its stdin is
    // still open) so nothing here may block on EOF.
    let stdout = child.stdout.take().expect("piped stdout");
    let (lines_tx, lines_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if lines_tx.send(line).is_err() {
                return;
            }
        }
    });
    // Killed on drop from here on, however the assertions below go.
    let mut server = Server(child);

    let cwd = home.0.display().to_string();
    let mut stdin = server.0.stdin.take().expect("piped stdin");
    stdin
        .write_all(
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\
                  \"params\":{{\"protocolVersion\":1,\"clientCapabilities\":{{}}}}}}\n\
                 {{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\
                  \"params\":{{\"cwd\":\"{cwd}\",\"mcpServers\":[]}}}}\n"
            )
            .as_bytes(),
        )
        .expect("write requests");
    stdin.flush().expect("flush requests");

    // Collect until both responses have landed. A bare line is not a
    // response, so a corrupting `println!` shows up here as an extra entry
    // rather than as a timeout.
    let mut lines = Vec::new();
    let mut replies = HashMap::new();
    while replies.len() < 2 {
        let line = lines_rx.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|err| {
            panic!(
                "no reply from `wizard acp` ({err}); stdout so far:\n{}",
                lines.join("\n")
            )
        });
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
            && let Some(id) = value.get("id").and_then(serde_json::Value::as_u64)
            && (value.get("result").is_some() || value.get("error").is_some())
        {
            replies.insert(id, value);
        }
        lines.push(line);
    }

    // Every line is a JSON-RPC frame: parseable, an object, `jsonrpc: "2.0"`.
    for line in &lines {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("stdout carried a non-JSON line ({err}): {line:?}"));
        assert_eq!(
            value.get("jsonrpc").and_then(serde_json::Value::as_str),
            Some("2.0"),
            "stdout carried JSON that is not a JSON-RPC frame: {line}"
        );
    }

    // And the run really did reach the agent build, so the two `println!`s
    // this test exists to catch were both on the path it just walked.
    let session = replies.get(&2).expect("session/new answered");
    assert!(
        session.get("result").is_some(),
        "session/new must succeed against the fake provider, got: {session}"
    );

    // The notices did not vanish — they moved to stderr.
    let _ = server.0.kill();
    let mut stderr = String::new();
    if let Some(mut pipe) = server.0.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    assert!(
        stderr.contains("using the JSON tool protocol"),
        "the tool-protocol notice belongs on stderr, not stdout:\n{stderr}"
    );
}
