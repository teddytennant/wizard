//! A real workflow through the built binary: `wizard -p` against a mock OpenAI
//! server. The mock answers the main loop with a `Workflow` tool call, and the
//! workflow's subagents with canned text and a `StructuredOutput` call, so the
//! whole path runs: tool, QuickJS script, `agent()`, subagent loop, result.
#![cfg(feature = "plugin-js")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};

const SCRIPT: &str = "export const meta = {name: 'e2e', description: 'end to end', phases: [{title: 'Go'}]}
phase('Go')
const rs = await parallel(['alpha', 'beta'].map(x => () => agent('say ' + x, {label: x})))
const s = await agent('STRUCT give a number', {schema: {type: 'object', properties: {n: {type: 'integer'}}, required: ['n']}})
return {rs, n: s.n + 1}
";

struct Mock {
    port: u16,
}

fn chunk(delta: Value, finish: Option<&str>) -> String {
    let body = json!({
        "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "fake-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!("data: {body}\n\n")
}

fn tool_call(name: &str, arguments: &Value) -> String {
    let mut sse = chunk(json!({"role": "assistant"}), None);
    sse.push_str(&chunk(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}}]}),
        None,
    ));
    sse.push_str(&chunk(json!({}), Some("tool_calls")));
    sse
}

fn text(content: &str) -> String {
    let mut sse = chunk(json!({"role": "assistant"}), None);
    sse.push_str(&chunk(json!({"content": content}), None));
    sse.push_str(&chunk(json!({}), Some("stop")));
    sse
}

fn answer(request: &str) -> String {
    let has_tool_result = request.contains("\"role\":\"tool\"");
    if request.contains("computed at runtime by a workflow script") {
        if request.contains("STRUCT") && !has_tool_result {
            return tool_call("StructuredOutput", &json!({"n": 41}));
        }
        if request.contains("STRUCT") {
            return text("ok");
        }
        if request.contains("say alpha") {
            return text("ALPHA-DONE");
        }
        return text("BETA-DONE");
    }
    if !has_tool_result {
        return tool_call("Workflow", &json!({"script": SCRIPT}));
    }
    if request.contains("ALPHA-DONE") && request.contains("BETA-DONE") && request.contains("42") {
        return text("the workflow returned alpha, beta and 42");
    }
    text("the workflow result was missing something")
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
    let mut sse = answer(&String::from_utf8_lossy(&body));
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
        let dir = std::env::temp_dir().join(format!("wizard-workflow-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".wizard")).expect("home");
        std::fs::write(
            dir.join(".wizard").join("config.toml"),
            format!(
                "model = \"fake-model\"\nactive_provider = \"fake\"\nmode = \"sovereign\"\n\n\
                 [[providers]]\nname = \"fake\"\nkind = \"openai\"\n\
                 base_url = \"http://127.0.0.1:{}/v1\"\nmodel = \"fake-model\"\n\n\
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
            .env_remove("CLAUDE_CODE_DISABLE_WORKFLOWS")
            .current_dir(&self.0)
            .output()
            .expect("binary runs")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_workflow_runs_end_to_end_and_leaves_a_resumable_record() {
    let mock = Mock::start();
    let home = Home::new(&mock);
    let out = home.run(&[
        "-p",
        "run the workflow",
        "--output-format",
        "stream-json",
        "--verbose",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let values: Vec<Value> = stdout
        .lines()
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    let result = values
        .iter()
        .find(|v| v["type"] == "result")
        .unwrap_or_else(|| {
            panic!(
                "no result line: {stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    assert_eq!(
        result["result"], "the workflow returned alpha, beta and 42",
        "{result}"
    );

    let tool_use = values
        .iter()
        .flat_map(|v| {
            v["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .find(|b| b["type"] == "tool_use" && b["name"] == "Workflow")
        .expect("the main loop called Workflow");
    assert!(
        tool_use["input"]["script"]
            .as_str()
            .unwrap()
            .contains("export const meta")
    );

    // The run is on disk: record, script, one journal line per agent.
    let runs = home.0.join(".wizard/workflows/runs");
    let run = std::fs::read_dir(&runs)
        .expect("runs dir")
        .next()
        .expect("one run")
        .unwrap()
        .path();
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(run.join("run.json")).unwrap()).unwrap();
    assert_eq!(record["status"], "completed");
    assert_eq!(record["name"], "e2e");
    assert_eq!(record["agents"], 3);
    assert_eq!(record["result"]["n"], 42);
    let journal = std::fs::read_to_string(run.join("journal.jsonl")).unwrap();
    assert_eq!(journal.lines().count(), 3, "{journal}");
    assert!(run.join("script.js").exists());
}
