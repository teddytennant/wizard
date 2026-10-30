//! `wizard -p` against a mock OpenAI server: the headless output follows
//! Claude Code's message shapes, and the session flags do what they say.
//!
//! The mock answers every chat request with a tool call to `execute` when the
//! conversation has no tool result yet and the prompt says `echo hi`, and with
//! `done` otherwise. It is just enough HTTP for the provider client.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

struct Mock {
    port: u16,
}

fn chunk(delta: Value, finish: Option<&str>) -> String {
    let body = serde_json::json!({
        "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "fake-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!("data: {body}\n\n")
}

fn respond(mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    if request_line.starts_with("GET") {
        let json = r#"{"object":"list","data":[{"id":"fake-model","object":"model","created":1}]}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
            json.len()
        );
        return;
    }
    let text = String::from_utf8_lossy(&body);
    let has_tool_result = text.contains("\"role\":\"tool\"");
    let wants_tool = text.contains("echo hi") && !has_tool_result;
    let mut sse = chunk(serde_json::json!({"role": "assistant"}), None);
    if wants_tool {
        sse.push_str(&chunk(
            serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                "function": {"name": "execute", "arguments": "{\"command\":\"echo hi\"}"}}]}),
            None,
        ));
        sse.push_str(&chunk(serde_json::json!({}), Some("tool_calls")));
    } else {
        sse.push_str(&chunk(serde_json::json!({"content": "done"}), None));
        sse.push_str(&chunk(serde_json::json!({}), Some("stop")));
    }
    sse.push_str(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"fake-model\",\
         \"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"total_tokens\":110}}\n\n\
         data: [DONE]\n\n",
    );
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
        sse.len()
    );
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || respond(stream));
            }
        });
        Self { port }
    }
}

struct Home(PathBuf);

impl Home {
    fn new(mock: &Mock) -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wizard-shapes-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(dir.join(".wizard")).expect("home");
        std::fs::write(
            dir.join(".wizard").join("config.toml"),
            format!(
                "model = \"fake-model\"\nactive_provider = \"fake\"\nmode = \"sovereign\"\n\n\
                 [[providers]]\nname = \"fake\"\nkind = \"openai\"\n\
                 base_url = \"http://127.0.0.1:{}/v1\"\nmodel = \"fake-model\"\n\
                 usd_per_mtok_in = 1.0\nusd_per_mtok_out = 2.0\n\n\
                 [update]\nnotify = false\nauto = false\n",
                mock.port
            ),
        )
        .expect("config");
        Self(dir)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wizard"))
            .args(args)
            .env("HOME", &self.0)
            .env("WIZARD_HOME", self.0.join(".wizard"))
            .env("CLAUDE_CONFIG_DIR", self.0.join(".claude"))
            .env_remove("WIZARD_MODEL")
            .current_dir(&self.0)
            .output()
            .expect("binary runs")
    }

    fn sessions(&self) -> PathBuf {
        self.0.join(".wizard").join("sessions")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn lines(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).expect("every line is JSON"))
        .collect()
}

fn kinds(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .map(|value| match value["type"].as_str().unwrap_or("?") {
            "system" => format!("system/{}", value["subtype"].as_str().unwrap_or("")),
            "stream_event" => format!("event/{}", value["event"]["type"].as_str().unwrap_or("")),
            other => other.to_string(),
        })
        .collect()
}

fn exists(dir: &Path, name: &str) -> bool {
    dir.join(name).exists()
}

#[test]
fn stream_json_has_claude_codes_message_shapes() {
    let mock = Mock::start();
    let home = Home::new(&mock);
    let out = home.run(&["-p", "echo hi", "--output-format", "stream-json", "--verbose"]);
    let values = lines(&out);
    let types = kinds(&values);
    assert_eq!(types.first().map(String::as_str), Some("system/init"), "{types:?}");
    assert!(
        types.windows(3).any(|w| w == ["assistant", "user", "assistant"]),
        "tool_use, tool_result, answer: {types:?}"
    );
    assert_eq!(types.last().map(String::as_str), Some("result"), "{types:?}");

    let tool_use = values
        .iter()
        .find(|v| v["message"]["content"][0]["type"] == "tool_use")
        .expect("a tool_use message");
    let id = &tool_use["message"]["content"][0]["id"];
    assert_eq!(tool_use["message"]["content"][0]["name"], "Bash");
    let tool_result = values
        .iter()
        .find(|v| v["message"]["content"][0]["type"] == "tool_result")
        .expect("a tool_result message");
    assert_eq!(&tool_result["message"]["content"][0]["tool_use_id"], id);

    let result = values.last().expect("result");
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["result"], "done");
    assert_eq!(result["num_turns"], 2);
    assert_eq!(result["usage"]["input_tokens"], 200);
    assert!(result["total_cost_usd"].as_f64().unwrap() > 0.0);
    assert_eq!(result["session_id"], values[0]["session_id"]);
    assert_eq!(result["reason"], "completed");
}

#[test]
fn json_is_the_result_object_with_wizards_keys() {
    let mock = Mock::start();
    let home = Home::new(&mock);
    let out = home.run(&["-p", "say hello", "--output-format", "json"]);
    let values = lines(&out);
    assert_eq!(values.len(), 1);
    let result = &values[0];
    for key in [
        "type", "subtype", "is_error", "duration_ms", "num_turns", "result", "session_id",
        "total_cost_usd", "usage", "modelUsage", "permission_denials", "terminal_reason", "uuid",
        "reason", "turns", "steps", "tool_calls", "errors", "text",
    ] {
        assert!(result.get(key).is_some(), "result lacks {key}: {result}");
    }
}

#[test]
fn partial_messages_add_the_stream_events() {
    let mock = Mock::start();
    let home = Home::new(&mock);
    let out = home.run(&[
        "-p",
        "say hello",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
    ]);
    let types = kinds(&lines(&out));
    for expected in [
        "event/message_start",
        "event/content_block_start",
        "event/content_block_delta",
        "event/content_block_stop",
        "event/message_delta",
        "event/message_stop",
    ] {
        assert!(types.iter().any(|t| t == expected), "no {expected} in {types:?}");
    }
}

#[test]
fn a_tiny_budget_stops_the_run_with_claudes_subtype() {
    let mock = Mock::start();
    let home = Home::new(&mock);
    let out = home.run(&[
        "-p",
        "echo hi",
        "--output-format",
        "json",
        "--max-budget-usd",
        "0.00001",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let result = &lines(&out)[0];
    assert_eq!(result["subtype"], "error_max_budget_usd");
    assert_eq!(result["terminal_reason"], "budget_exhausted");
    assert_eq!(result["is_error"], true);
}

#[test]
fn session_flags_name_fork_and_discard_sessions() {
    let mock = Mock::start();
    let home = Home::new(&mock);
    let first = home.run(&[
        "-p", "say hello", "--output-format", "json", "--session-id", "first-1", "--name", "my work",
    ]);
    assert_eq!(lines(&first)[0]["session_id"], "first-1");
    assert!(exists(&home.sessions(), "first-1.jsonl"));
    assert!(exists(&home.sessions(), "first-1.name"));

    let reuse = home.run(&["-p", "again", "--session-id", "first-1"]);
    assert_eq!(reuse.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&reuse.stderr).contains("already in use"));

    let forked = home.run(&[
        "-p", "more", "--output-format", "json", "--resume", "my work", "--fork-session",
        "--session-id", "second-1",
    ]);
    assert_eq!(lines(&forked)[0]["session_id"], "second-1");
    let original = std::fs::read_to_string(home.sessions().join("first-1.jsonl")).unwrap();
    let copy = std::fs::read_to_string(home.sessions().join("second-1.jsonl")).unwrap();
    assert!(copy.len() > original.len(), "the fork continued; the original did not");

    let gone = home.run(&[
        "-p", "bye", "--output-format", "json", "--no-session-persistence", "--session-id", "gone-1",
    ]);
    assert_eq!(lines(&gone)[0]["session_id"], "gone-1");
    assert!(!exists(&home.sessions(), "gone-1.jsonl"));
}

#[test]
fn json_schema_is_validated_and_reported() {
    let mock = Mock::start();
    let home = Home::new(&mock);
    let out = home.run(&[
        "-p",
        "say hello",
        "--output-format",
        "json",
        "--json-schema",
        r#"{"type":"object","required":["answer"]}"#,
    ]);
    // The mock answers "done", which is not JSON; two retries get the same.
    assert_eq!(out.status.code(), Some(1));
    let result = &lines(&out)[0];
    assert_eq!(result["subtype"], "error_max_structured_output_retries");
    assert_eq!(result["is_error"], true);
}

#[test]
fn stream_json_input_is_a_conversation() {
    let mock = Mock::start();
    let home = Home::new(&mock);
    let mut child = Command::new(env!("CARGO_BIN_EXE_wizard"))
        .args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"])
        .env("HOME", &home.0)
        .env("WIZARD_HOME", home.0.join(".wizard"))
        .current_dir(&home.0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(stdin, r#"{{"type":"control_request","request_id":"r1","request":{{"subtype":"initialize"}}}}"#).unwrap();
        writeln!(stdin, r#"{{"type":"user","message":{{"role":"user","content":"say hello"}}}}"#).unwrap();
        writeln!(stdin, r#"{{"type":"user","message":{{"role":"user","content":"say it again"}}}}"#).unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("exit");
    let types = kinds(&lines(&out));
    assert_eq!(types.first().map(String::as_str), Some("control_response"), "{types:?}");
    assert_eq!(types.iter().filter(|t| *t == "result").count(), 2, "{types:?}");
    assert_eq!(types.iter().filter(|t| *t == "system/init").count(), 2, "{types:?}");
}

#[test]
fn stream_json_input_needs_stream_json_output() {
    let mock = Mock::start();
    let home = Home::new(&mock);
    let mut child = Command::new(env!("CARGO_BIN_EXE_wizard"))
        .args(["--print", "--input-format", "stream-json", "--output-format", "json"])
        .env("HOME", &home.0)
        .env("WIZARD_HOME", home.0.join(".wizard"))
        .current_dir(&home.0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("exit");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("requires output-format=stream-json"));
}
