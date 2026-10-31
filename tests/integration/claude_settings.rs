//! Claude Code settings keys and flags that change what a `wizard -p` run
//! does, against a mock OpenAI server that records what it was sent and, when
//! told to, answers the first request with one tool call.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::Value;

struct Mock {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

fn chunk(delta: Value, finish: Option<&str>) -> String {
    let body = serde_json::json!({
        "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "fake-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!("data: {body}\n\n")
}

fn respond(mut stream: TcpStream, tool: Option<(String, String)>, seen: Arc<Mutex<Vec<String>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut headers = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        headers.push_str(&line);
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    if request_line.contains("/v1/messages") {
        seen.lock()
            .unwrap()
            .push(format!("{headers}\n{}", String::from_utf8_lossy(&body)));
        let json = r#"{"type":"error","error":{"type":"invalid_request_error","message":"mock"}}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
            json.len()
        );
        return;
    }
    if request_line.starts_with("GET") {
        let json = r#"{"object":"list","data":[{"id":"fake-model","object":"model","created":1}]}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
            json.len()
        );
        return;
    }
    let text = String::from_utf8_lossy(&body).to_string();
    seen.lock().unwrap().push(format!("{headers}\n{text}"));
    let has_tool_result = text.contains("\"role\":\"tool\"");
    let mut sse = chunk(serde_json::json!({"role": "assistant"}), None);
    match tool {
        Some((name, args)) if !has_tool_result => {
            sse.push_str(&chunk(
                serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                    "function": {"name": name, "arguments": args}}]}),
                None,
            ));
            sse.push_str(&chunk(serde_json::json!({}), Some("tool_calls")));
        }
        _ => {
            sse.push_str(&chunk(serde_json::json!({"content": "done"}), None));
            sse.push_str(&chunk(serde_json::json!({}), Some("stop")));
        }
    }
    sse.push_str("data: [DONE]\n\n");
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
        sse.len()
    );
}

impl Mock {
    fn start(tool: Option<(&str, &str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let tool = tool.map(|(name, args)| (name.to_string(), args.to_string()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (tool, seen) = (tool.clone(), Arc::clone(&record));
                std::thread::spawn(move || respond(stream, tool, seen));
            }
        });
        Self { port, seen }
    }

    /// The tool names of the first chat request.
    fn tool_names(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        let request = seen.first().expect("a chat request was made");
        let body: Value =
            serde_json::from_str(request.split_once("\n\n").map_or("", |(_, body)| body))
                .expect("json body");
        body["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct Home(PathBuf);

impl Home {
    fn new(mock: &Mock, kind: &str) -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wizard-cset-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(dir.join(".wizard")).expect("home");
        std::fs::create_dir_all(dir.join(".claude")).expect("claude");
        std::fs::write(
            dir.join(".wizard").join("config.toml"),
            format!(
                "model = \"fake-model\"\nactive_provider = \"fake\"\nmode = \"sovereign\"\n\n\
                 [[providers]]\nname = \"fake\"\nkind = \"{kind}\"\nbase_url = \"http://127.0.0.1:{}{}\"\n\
                 model = \"fake-model\"\napi_key = \"k\"\n\n[update]\nnotify = false\nauto = false\n",
                mock.port,
                if kind == "openai" { "/v1" } else { "" },
            ),
        )
        .expect("config");
        Self(dir)
    }

    fn settings(&self, json: &str) {
        std::fs::write(self.0.join(".claude").join("settings.json"), json).expect("settings");
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wizard"));
        command
            .args(args)
            .env("HOME", &self.0)
            .env("WIZARD_HOME", self.0.join(".wizard"))
            .env("CLAUDE_CONFIG_DIR", self.0.join(".claude"))
            .env_remove("WIZARD_MODEL")
            .current_dir(&self.0);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("binary runs")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn values(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('{'))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn tool_result(output: &Output) -> (bool, String) {
    let all = values(output);
    let result = all
        .iter()
        .find(|v| v["message"]["content"][0]["type"] == "tool_result")
        .unwrap_or_else(|| panic!("a tool_result: {}", String::from_utf8_lossy(&output.stdout)));
    let block = &result["message"]["content"][0];
    (
        block["is_error"].as_bool().unwrap_or(false),
        block["content"].to_string(),
    )
}

const STREAM: [&str; 4] = ["--output-format", "stream-json", "--verbose", "-p"];

fn run_stream(home: &Home, prompt: &str, extra: &[&str]) -> Output {
    let mut args: Vec<&str> = extra.to_vec();
    args.extend(STREAM);
    args.push(prompt);
    home.run(&args)
}

#[test]
fn tools_flag_narrows_the_tool_list() {
    let mock = Mock::start(None);
    let home = Home::new(&mock, "openai");
    run_stream(&home, "hi", &["--tools", "Read,Grep"]);
    let names = mock.tool_names();
    assert!(names.contains(&"read_file".to_string()), "{names:?}");
    assert!(names.contains(&"search_files".to_string()), "{names:?}");
    assert!(!names.contains(&"execute".to_string()), "{names:?}");
    assert!(!names.contains(&"write_file".to_string()), "{names:?}");

    let mock = Mock::start(None);
    let home = Home::new(&mock, "openai");
    run_stream(&home, "hi", &["--tools", ""]);
    assert!(mock.tool_names().is_empty());

    let mock = Mock::start(None);
    let home = Home::new(&mock, "openai");
    run_stream(&home, "hi", &["--tools", "default"]);
    assert!(mock.tool_names().contains(&"execute".to_string()));
}

#[test]
fn an_enabled_sandbox_stops_the_file_tools_on_denied_paths() {
    let secret = std::env::temp_dir().join(format!("wizard-cset-secret-{}", std::process::id()));
    std::fs::write(&secret, "hunter2").expect("secret");
    let args = format!(r#"{{"path":"{}"}}"#, secret.display());
    let mock = Mock::start(Some(("read_file", &args)));
    let home = Home::new(&mock, "openai");
    home.settings(&format!(
        r#"{{"sandbox":{{"enabled":true,"filesystem":{{"denyRead":["{}"]}}}}}}"#,
        secret.display()
    ));
    let out = run_stream(&home, "read it", &[]);
    let (is_error, text) = tool_result(&out);
    assert!(is_error, "{text}");
    assert!(text.contains("denied"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");

    let mock = Mock::start(Some(("read_file", &args)));
    let home = Home::new(&mock, "openai");
    home.settings(&format!(
        r#"{{"sandbox":{{"filesystem":{{"denyRead":["{}"]}}}}}}"#,
        secret.display()
    ));
    let (is_error, text) = tool_result(&run_stream(&home, "read it", &[]));
    assert!(!is_error && text.contains("hunter2"), "{text}");
    let _ = std::fs::remove_file(secret);
}

#[test]
fn claude_md_and_include_git_instructions_reach_the_prompt() {
    let mock = Mock::start(None);
    let home = Home::new(&mock, "openai");
    home.settings(
        r#"{"includeGitInstructions":false,"includeCoAuthoredBy":false,"language":"Dutch"}"#,
    );
    run_stream(&home, "hi", &[]);
    let seen = mock.seen.lock().unwrap();
    let request = seen.first().expect("request");
    assert!(request.contains("Respond in Dutch."), "{request}");
    assert!(request.contains("Do not commit, push"), "{request}");
    assert!(
        request.contains("Do not add attribution lines"),
        "{request}"
    );
}

#[test]
fn agent_in_settings_is_the_default_for_the_flag() {
    let mock = Mock::start(None);
    let home = Home::new(&mock, "openai");
    std::fs::create_dir_all(home.0.join(".claude").join("agents")).expect("agents");
    std::fs::write(
        home.0.join(".claude").join("agents").join("rev.md"),
        "---\nname: rev\ndescription: reviews code\n---\nYou review code harshly.\n",
    )
    .expect("agent");
    home.settings(r#"{"agent":"rev"}"#);
    run_stream(&home, "hi", &[]);
    let seen = mock.seen.lock().unwrap();
    assert!(seen[0].contains("You review code harshly."), "{}", seen[0]);
}

#[test]
fn setting_sources_hide_the_other_layers() {
    // The home directory is also the project, so this one file is both the
    // user layer and the project layer.
    for (sources, seen_in_prompt) in [("user", true), ("project", true), ("local", false)] {
        let mock = Mock::start(None);
        let home = Home::new(&mock, "openai");
        home.settings(r#"{"language":"Dutch"}"#);
        run_stream(&home, "hi", &["--setting-sources", sources]);
        assert_eq!(
            mock.seen.lock().unwrap()[0].contains("Respond in Dutch."),
            seen_in_prompt,
            "--setting-sources {sources}"
        );
    }
}

#[test]
fn betas_and_custom_headers_go_to_the_anthropic_provider() {
    let mock = Mock::start(None);
    let home = Home::new(&mock, "anthropic");
    home.run_env(
        &["--betas", "my-beta", "--output-format", "json", "-p", "hi"],
        &[
            ("ANTHROPIC_CUSTOM_HEADERS", "X-Team: blue"),
            ("CLAUDE_CODE_EXTRA_BODY", r#"{"top_k": 7}"#),
        ],
    );
    let seen = mock.seen.lock().unwrap();
    let request = seen
        .first()
        .expect("a request reached the mock")
        .to_ascii_lowercase();
    assert!(request.contains("anthropic-beta: my-beta"), "{request}");
    assert!(request.contains("x-team: blue"), "{request}");
    assert!(request.contains("\"top_k\":7"), "{request}");
}

#[test]
fn fast_mode_adds_the_speed_field_for_opus_only() {
    for (model, expect) in [("claude-opus-4-6", true), ("claude-sonnet-4-5", false)] {
        let mock = Mock::start(None);
        let home = Home::new(&mock, "anthropic");
        home.settings(r#"{"fastMode":true}"#);
        home.run(&["--model", model, "--output-format", "json", "-p", "hi"]);
        let seen = mock.seen.lock().unwrap();
        let request = seen.first().expect("a request").to_ascii_lowercase();
        assert_eq!(
            request.contains("\"speed\":\"fast\""),
            expect,
            "{model}: {request}"
        );
        assert_eq!(
            request.contains("fast-mode-2026-02-01"),
            true,
            "the beta header is on whenever fast mode is"
        );
    }
}

#[test]
fn the_permission_prompt_tool_answers_a_request_nobody_could() {
    let Some(python) = ["python3", "python"].into_iter().find(|name| {
        Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }) else {
        return;
    };
    let target = std::env::temp_dir().join(format!("wizard-cset-gate-{}", std::process::id()));
    let _ = std::fs::remove_file(&target);
    let args = format!(r#"{{"path":"{}","content":"x"}}"#, target.display());
    let mock = Mock::start(Some(("write_file", &args)));
    let home = Home::new(&mock, "openai");
    let server = home.0.join("gate.py");
    std::fs::write(
        &server,
        r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    method, ident = msg.get("method"), msg.get("id")
    if ident is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "gate", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "approve", "description": "decide",
                  "inputSchema": {"type": "object", "properties": {}}}]}
    else:
        asked = msg["params"]["arguments"]
        verdict = {"behavior": "allow"} if asked["tool_name"] == "Write" else {"behavior": "deny", "message": "no"}
        result = {"content": [{"type": "text", "text": json.dumps(verdict)}]}
    print(json.dumps({"jsonrpc": "2.0", "id": ident, "result": result}), flush=True)
"#,
    )
    .expect("gate server");
    std::fs::write(
        home.0.join(".wizard").join("mcp.toml"),
        format!(
            "[[server]]\nname = \"gate\"\ntransport = \"stdio\"\ncommand = \"{python}\"\nargs = [\"{}\"]\n",
            server.display()
        ),
    )
    .expect("mcp.toml");

    let without = run_stream(&home, "write it", &["--permission-mode", "default"]);
    let (is_error, text) = tool_result(&without);
    assert!(is_error, "nobody to ask means refused: {text}");
    assert!(!target.exists());

    let mock = Mock::start(Some(("write_file", &args)));
    let home2 = Home::new(&mock, "openai");
    std::fs::copy(server, home2.0.join("gate.py")).expect("copy");
    std::fs::copy(
        home.0.join(".wizard").join("mcp.toml"),
        home2.0.join(".wizard").join("mcp.toml"),
    )
    .expect("copy");
    let toml = std::fs::read_to_string(home2.0.join(".wizard").join("mcp.toml"))
        .expect("read")
        .replace(
            &home.0.display().to_string(),
            &home2.0.display().to_string(),
        );
    std::fs::write(home2.0.join(".wizard").join("mcp.toml"), toml).expect("write");
    let with = run_stream(
        &home2,
        "write it",
        &[
            "--permission-mode",
            "default",
            "--permission-prompt-tool",
            "mcp__gate__approve",
        ],
    );
    let (is_error, text) = tool_result(&with);
    assert!(!is_error, "the gate allowed Write: {text}");
    assert!(target.exists(), "{}", String::from_utf8_lossy(&with.stderr));
    let _ = std::fs::remove_file(target);
}
