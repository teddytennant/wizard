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
use std::sync::{Arc, Mutex, mpsc};
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
    spawn_recording_ollama().0
}

/// The request bodies of every `/api/chat` call, i.e. every model turn.
type Chats = Arc<Mutex<Vec<String>>>;

/// [`spawn_fake_ollama`], also answering `/api/chat` with a one-line reply and
/// keeping each chat request body, so a test can tell whether a prompt reached
/// the model.
fn spawn_recording_ollama() -> (u16, Chats) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let chats = Chats::default();
    let recorded = Arc::clone(&chats);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let chats = Arc::clone(&recorded);
            std::thread::spawn(move || serve_connection(stream, chats));
        }
    });
    (port, chats)
}

/// Answer requests on one keep-alive connection until the client hangs up.
fn serve_connection(stream: TcpStream, chats: Chats) {
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
        let mut body = Vec::new();
        if let Some(len) = headers.get("content-length").and_then(|v| v.parse().ok()) {
            body = vec![0u8; len];
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
            // A model turn: one line of text, then done.
            "/api/chat" => {
                chats
                    .lock()
                    .expect("chats lock")
                    .push(String::from_utf8_lossy(&body).into_owned());
                "{\"message\":{\"role\":\"assistant\",\"content\":\"the model answered\"},\
                  \"done\":false}\n\
                 {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\
                  \"done_reason\":\"stop\"}\n"
                    .to_string()
            }
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

/// Send one JSON-RPC request and wait for the reply with its id, skipping
/// notifications and any other reply in between.
fn request(
    stdin: &mut impl Write,
    lines: &mpsc::Receiver<String>,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let frame = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    writeln!(stdin, "{frame}").expect("write request");
    stdin.flush().expect("flush request");
    loop {
        let line = lines
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|err| panic!("no reply to {method} ({err})"));
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
            return value;
        }
    }
}

/// The option with `id` out of a `configOptions` array.
fn option<'a>(options: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    options
        .as_array()
        .and_then(|options| options.iter().find(|option| option["id"] == id))
        .unwrap_or_else(|| panic!("no {id} option in {options}"))
}

/// A client picks the model, effort, and mode per session through config
/// options, and a session it only opened to read them leaves no file behind
/// when the server is stopped the way Zeron stops it (SIGTERM).
#[cfg(unix)]
#[test]
fn acp_sessions_offer_model_options_and_probes_leave_no_file() {
    let home = TempDir::new();
    let port = spawn_fake_ollama();
    write_config(&home.0, port);

    let mut child = Command::new(env!("CARGO_BIN_EXE_wizard"))
        .arg("acp")
        .env("HOME", &home.0)
        .env_remove("WIZARD_HOME")
        .env_remove("WIZARD_MODEL")
        .env_remove("WIZARD_OLLAMA_HOST")
        .env_remove("WIZARD_LLAMACPP_HOST")
        .env_remove("WIZARD_GGUF_PATH")
        .env_remove("WIZARD_SYSTEM_PROMPT")
        .env_remove("WIZARD_HARNESS_DIR")
        .current_dir(&home.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("wizard acp starts");
    let stdout = child.stdout.take().expect("piped stdout");
    let (lines_tx, lines) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if lines_tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut server = Server(child);
    let mut stdin = server.0.stdin.take().expect("piped stdin");
    let cwd = home.0.display().to_string();

    request(
        &mut stdin,
        &lines,
        1,
        "initialize",
        serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let session = request(
        &mut stdin,
        &lines,
        2,
        "session/new",
        serde_json::json!({"cwd": cwd, "mcpServers": []}),
    );
    let result = &session["result"];
    let session_id = result["sessionId"]
        .as_str()
        .expect("a session id")
        .to_string();
    let options = &result["configOptions"];
    let model = option(options, "model");
    assert_eq!(model["category"], "model");
    assert_eq!(model["currentValue"], "fake/fake-model:test");
    assert_eq!(model["options"][0]["value"], "fake/fake-model:test");
    let effort = option(options, "thought_level");
    assert_eq!(effort["category"], "thought_level");
    assert_eq!(effort["currentValue"], "default");
    assert_eq!(option(options, "wizard_mode")["currentValue"], "genie");

    let sessions = home.0.join(".wizard").join("sessions");
    let files = || {
        std::fs::read_dir(&sessions)
            .map(|dir| dir.flatten().count())
            .unwrap_or(0)
    };
    assert_eq!(files(), 1, "session/new creates its file");

    // Effort applies in place; the answer carries the new state.
    let set = request(
        &mut stdin,
        &lines,
        3,
        "session/set_config_option",
        serde_json::json!({"sessionId": session_id, "configId": "thought_level", "value": "high"}),
    );
    assert_eq!(
        option(&set["result"]["configOptions"], "thought_level")["currentValue"],
        "high"
    );

    // A model on a configured provider rebuilds the session's agent onto it,
    // even one the provider never listed.
    let set = request(
        &mut stdin,
        &lines,
        4,
        "session/set_config_option",
        serde_json::json!({"sessionId": session_id, "configId": "model", "value": "fake/other-model:test"}),
    );
    let model = option(&set["result"]["configOptions"], "model");
    assert_eq!(model["currentValue"], "fake/other-model:test");
    assert_eq!(
        option(&set["result"]["configOptions"], "thought_level")["currentValue"],
        "high",
        "a model switch keeps the effort"
    );

    // A provider that is not configured is refused.
    let refused = request(
        &mut stdin,
        &lines,
        5,
        "session/set_config_option",
        serde_json::json!({"sessionId": session_id, "configId": "model", "value": "nowhere/gpt-5"}),
    );
    assert!(refused.get("error").is_some(), "{refused}");

    // Stopped the way process supervisors stop it: the session nothing was
    // said in is removed on the way out.
    // SAFETY: signalling our own child by pid.
    unsafe { libc::kill(server.0.id() as i32, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if server.0.try_wait().expect("wait").is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "wizard acp did not exit on SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(files(), 0, "an unprompted session leaves no file");
}

/// A session that cannot start says why. The agent build refusing is the
/// common way `session/new` fails — a provider whose sign-in is gone — and
/// the client has only the error it is sent: a bare "Internal error" left
/// Wizard GUI with nothing to show but the code.
#[cfg(feature = "provider-xai")]
#[test]
fn acp_session_new_failure_carries_the_reason() {
    let home = TempDir::new();
    let dir = home.0.join(".wizard");
    std::fs::create_dir_all(&dir).expect("create .wizard dir");
    // An account sign-in with no `xai_oauth.json` beside it: signed out.
    std::fs::write(
        dir.join("config.toml"),
        "[[providers]]\n\
         name = \"xai-oauth\"\n\
         kind = \"xaioauth\"\n\
         base_url = \"https://api.x.ai/v1\"\n\
         model = \"grok-4.6\"\n",
    )
    .expect("write config.toml");

    let mut child = Command::new(env!("CARGO_BIN_EXE_wizard"))
        .arg("acp")
        .env("HOME", &home.0)
        .env_remove("WIZARD_HOME")
        .env_remove("WIZARD_MODEL")
        .env_remove("WIZARD_SYSTEM_PROMPT")
        .env_remove("WIZARD_HARNESS_DIR")
        .current_dir(&home.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("wizard acp starts");
    let stdout = child.stdout.take().expect("piped stdout");
    let (lines_tx, lines) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if lines_tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut server = Server(child);
    let mut stdin = server.0.stdin.take().expect("piped stdin");
    let cwd = home.0.display().to_string();

    request(
        &mut stdin,
        &lines,
        1,
        "initialize",
        serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let session = request(
        &mut stdin,
        &lines,
        2,
        "session/new",
        serde_json::json!({"cwd": cwd, "mcpServers": []}),
    );
    let error = session
        .get("error")
        .unwrap_or_else(|| panic!("session/new must fail with no xAI sign-in, got: {session}"));
    assert_eq!(error["code"], -32603, "{error}");
    let detail = error["data"].as_str().unwrap_or_default();
    assert!(
        detail.contains("not signed in to xAI") && detail.contains("wizard --login xai"),
        "the error should say what is wrong and how to fix it, got: {error}"
    );
}

/// A `wizard acp` on a fake Ollama that records model turns, initialized and
/// with one session open.
struct AcpSession {
    // Field order is drop order: stdin closes before the server is killed.
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
    session_id: String,
    /// The `initialize` reply.
    initialized: serde_json::Value,
    chats: Chats,
    next_id: u64,
    _server: Server,
    _home: TempDir,
}

impl AcpSession {
    fn start(tag: &str) -> Self {
        let home = TempDir(
            std::env::temp_dir().join(format!("wizard-acp-itest-{tag}-{}", std::process::id())),
        );
        std::fs::create_dir_all(&home.0).expect("create temp dir");
        let (port, chats) = spawn_recording_ollama();
        write_config(&home.0, port);
        let mut child = Command::new(env!("CARGO_BIN_EXE_wizard"))
            .arg("acp")
            .env("HOME", &home.0)
            .env_remove("WIZARD_HOME")
            .env_remove("WIZARD_MODEL")
            .env_remove("WIZARD_OLLAMA_HOST")
            .env_remove("WIZARD_LLAMACPP_HOST")
            .env_remove("WIZARD_GGUF_PATH")
            .env_remove("WIZARD_SYSTEM_PROMPT")
            .env_remove("WIZARD_HARNESS_DIR")
            .current_dir(&home.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("wizard acp starts");
        let stdout = child.stdout.take().expect("piped stdout");
        let (lines_tx, lines) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if lines_tx.send(line).is_err() {
                    return;
                }
            }
        });
        let mut server = Server(child);
        let stdin = server.0.stdin.take().expect("piped stdin");
        let mut acp = Self {
            stdin,
            lines,
            session_id: String::new(),
            initialized: serde_json::Value::Null,
            chats,
            next_id: 1,
            _server: server,
            _home: home,
        };
        acp.initialized = acp
            .call(
                "initialize",
                serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
            )
            .0;
        let cwd = acp._home.0.display().to_string();
        let (reply, _) = acp.call(
            "session/new",
            serde_json::json!({"cwd": cwd, "mcpServers": []}),
        );
        acp.session_id = reply["result"]["sessionId"]
            .as_str()
            .expect("a session id")
            .to_string();
        acp
    }

    /// Send a request; return its reply and the notifications before it.
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        let id = self.next_id;
        self.next_id += 1;
        let frame =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{frame}").expect("write request");
        self.stdin.flush().expect("flush request");
        let mut notifications = Vec::new();
        loop {
            let value = self.next_frame(method);
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return (value, notifications);
            }
            notifications.push(value);
        }
    }

    fn next_frame(&self, waiting_on: &str) -> serde_json::Value {
        let line = self
            .lines
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|err| panic!("nothing from wizard acp during {waiting_on} ({err})"));
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("non-JSON line ({err}): {line}"))
    }

    /// The next `session/update` of `kind`, skipping any other frame.
    fn update(&self, kind: &str) -> serde_json::Value {
        loop {
            let value = self.next_frame(kind);
            let update = &value["params"]["update"];
            if update["sessionUpdate"] == kind {
                return update.clone();
            }
        }
    }

    /// Prompt with `text`; return the stop reason and the updates it sent.
    fn prompt(&mut self, text: &str) -> (String, Vec<serde_json::Value>) {
        let params = serde_json::json!({
            "sessionId": self.session_id,
            "prompt": [{"type": "text", "text": text}],
        });
        let (reply, notifications) = self.call("session/prompt", params);
        let stop = reply["result"]["stopReason"]
            .as_str()
            .unwrap_or_else(|| panic!("{text} was answered with {reply}"))
            .to_string();
        let updates = notifications
            .into_iter()
            .map(|value| value["params"]["update"].clone())
            .collect();
        (stop, updates)
    }

    fn model_turns(&self) -> Vec<String> {
        self.chats.lock().expect("chats lock").clone()
    }
}

/// The text of every `agent_message_chunk` in `updates`, joined.
fn said(updates: &[serde_json::Value]) -> String {
    updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|update| update["content"]["text"].as_str())
        .collect()
}

/// A new session tells the client which slash commands it runs, and leaves
/// out the ones that need the terminal.
#[test]
fn acp_advertises_the_commands_it_runs() {
    let acp = AcpSession::start("advertise");
    let update = acp.update("available_commands_update");
    let commands = update["availableCommands"]
        .as_array()
        .expect("a command list");
    let names: Vec<&str> = commands
        .iter()
        .filter_map(|command| command["name"].as_str())
        .collect();
    for expected in [
        "model", "mode", "effort", "plan", "compact", "diff", "usage", "memory", "ultra", "fusion",
        "rewind", "todos", "agents", "provider", "help", "ui",
    ] {
        assert!(
            names.contains(&expected),
            "/{expected} not advertised: {names:?}"
        );
    }
    for terminal_only in ["vim", "view", "settings", "dashboard", "quit", "resume"] {
        assert!(
            !names.contains(&terminal_only),
            "/{terminal_only} advertised: {names:?}"
        );
    }
    let effort = commands
        .iter()
        .find(|command| command["name"] == "effort")
        .expect("effort");
    assert_eq!(effort["input"]["hint"], "[low|medium|high|xhigh|default]");
    assert!(
        effort["description"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
    );
}

/// `/ui` is how a full look gets back to the house TUI: it saves `[ui] skin`
/// without a model turn, and the `wizard` that started the look reads it when
/// the look quits.
#[test]
fn acp_ui_saves_the_look_wizard_starts_in() {
    let mut acp = AcpSession::start("ui");
    let (stop, updates) = acp.prompt("/ui pi");
    assert_eq!(stop, "end_turn");
    let saved = said(&updates);
    assert!(saved.contains("[ui] skin = \"pi\""), "{saved}");
    let config = std::fs::read_to_string(acp._home.0.join(".wizard/config.toml"))
        .expect("the config is still there");
    assert!(config.contains("skin = \"pi\""), "{config}");

    let (_, updates) = acp.prompt("/ui");
    let listing = said(&updates);
    assert!(listing.contains("● pi"), "{listing}");
    assert!(
        acp.model_turns().is_empty(),
        "a command is not a model turn"
    );
}

/// `/effort high` changes the session's effort through the command path and
/// tells the client's picker, without a model turn.
#[test]
fn acp_effort_command_updates_the_session_and_its_picker() {
    let mut acp = AcpSession::start("effort");
    let (stop, updates) = acp.prompt("/effort high");
    assert_eq!(stop, "end_turn");
    assert_eq!(said(&updates), "reasoning effort: high");
    let options = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "config_option_update")
        .map(|update| &update["configOptions"])
        .unwrap_or_else(|| panic!("no config_option_update in {updates:?}"));
    assert_eq!(option(options, "thought_level")["currentValue"], "high");
    assert_eq!(
        option(options, "model")["currentValue"],
        "fake/fake-model:test"
    );

    // The session holds it: the next option read starts from `high`.
    let session_id = acp.session_id.clone();
    let (set, _) = acp.call(
        "session/set_config_option",
        serde_json::json!({"sessionId": session_id, "configId": "wizard_mode", "value": "sovereign"}),
    );
    assert_eq!(
        option(&set["result"]["configOptions"], "thought_level")["currentValue"],
        "high"
    );
    assert!(
        acp.model_turns().is_empty(),
        "a command is not a model turn"
    );
}

/// `/help` and `/usage` answer with text from the command code, not the
/// model.
#[test]
fn acp_help_and_usage_answer_without_a_model_call() {
    let mut acp = AcpSession::start("help");
    let (stop, updates) = acp.prompt("/help");
    assert_eq!(stop, "end_turn");
    let help = said(&updates);
    assert!(help.starts_with("commands:"), "{help}");
    assert!(help.contains("/effort"), "{help}");
    assert!(help.contains("not available over ACP"), "{help}");
    assert!(
        !updates
            .iter()
            .any(|update| update["sessionUpdate"] == "config_option_update"),
        "/help changes nothing: {updates:?}"
    );

    let (stop, updates) = acp.prompt("/usage");
    assert_eq!(stop, "end_turn");
    let usage = said(&updates);
    // A fresh home has no sign-in, and the fake provider is keyed, so the
    // answer is how to sign in plus the session's own token rollup.
    assert!(usage.starts_with("no subscription signed in"), "{usage}");
    assert!(usage.contains("/login xai"), "{usage}");
    assert!(usage.contains("session usage:"), "{usage}");

    assert!(
        acp.model_turns().is_empty(),
        "no /api/chat call: {:?}",
        acp.model_turns()
    );
}

/// A slash word this build does not know is an ordinary prompt.
#[test]
fn acp_unknown_slash_command_goes_to_the_model() {
    let mut acp = AcpSession::start("unknown");
    let (stop, updates) = acp.prompt("/frobnicate the widget");
    assert_eq!(stop, "end_turn");
    assert!(said(&updates).contains("the model answered"), "{updates:?}");
    let turns = acp.model_turns();
    assert!(
        turns
            .iter()
            .any(|body| body.contains("/frobnicate the widget")),
        "the prompt reached the model as typed: {turns:?}"
    );
}

/// A client with no terminal to run `wizard --login` in signs in over ACP:
/// `initialize` lists the methods, and `api-key` makes the key's provider the
/// active one, with the key in `credentials.toml` and not in `config.toml`.
#[cfg(feature = "provider-openai")]
#[test]
fn acp_api_key_sign_in_makes_its_provider_active() {
    let mut acp = AcpSession::start("auth");
    let methods: Vec<String> = acp.initialized["result"]["authMethods"]
        .as_array()
        .expect("initialize lists auth methods")
        .iter()
        .filter_map(|method| method["id"].as_str().map(str::to_string))
        .collect();
    assert!(methods.iter().any(|id| id == "api-key"), "{methods:?}");

    let (reply, _) = acp.call(
        "authenticate",
        serde_json::json!({
            "methodId": "api-key",
            "_meta": {
                "provider": "openai",
                "apiKey": "sk-itest-5f3a",
                "model": "grok-4.6",
                "baseUrl": "http://127.0.0.1:9/v1",
            },
        }),
    );
    assert!(reply.get("error").is_none(), "{reply}");
    let dir = acp._home.0.join(".wizard");
    let config = std::fs::read_to_string(dir.join("config.toml")).expect("config.toml");
    assert!(config.contains("active_provider = \"openai\""), "{config}");
    assert!(config.contains("http://127.0.0.1:9/v1"), "{config}");
    assert!(
        !config.contains("sk-itest-5f3a"),
        "the key leaked into {config}"
    );
    let credentials =
        std::fs::read_to_string(dir.join("credentials.toml")).expect("credentials.toml");
    assert!(credentials.contains("sk-itest-5f3a"), "{credentials}");

    let (reply, _) = acp.call(
        "authenticate",
        serde_json::json!({ "methodId": "no-such-method" }),
    );
    assert!(reply.get("error").is_some(), "{reply}");
}
