//! `LSP`, and `EnterPlanMode`.
//!
//! `LSP` runs a real language server. The first call for a file starts the
//! server for its extension (rust-analyzer, pyright, typescript-language-server,
//! gopls or clangd when they are on `PATH`, or whatever `.lsp.json` names),
//! waits for it to finish indexing, and keeps it for the rest of the session.
//! Answers are worded the way Claude Code's tool words them. With no server
//! for the file type it says so.
//!
//! `EnterPlanMode` flips the same flag `/plan` does; the turn loop re-composes
//! the system prompt before the next completion.

mod client;
mod format;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};

pub(super) use self::client::on_path;
use self::client::{ServerSpec, builtin_servers, configured_servers, project_root};
use super::{invalid, str_arg};
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, resolve_path};

pub fn tools(plan_mode: Arc<AtomicBool>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(LspTool::new()),
        Arc::new(EnterPlanModeTool { plan_mode }),
    ]
}

const OPERATIONS: &[&str] = &[
    "goToDefinition",
    "findReferences",
    "hover",
    "documentSymbol",
    "workspaceSymbol",
    "goToImplementation",
    "prepareCallHierarchy",
    "incomingCalls",
    "outgoingCalls",
];

/// Files past this size are not sent to a server.
const MAX_FILE_BYTES: u64 = 10_000_000;

/// `LSP`.
pub struct LspTool {
    /// Servers to choose from instead of `.lsp.json` and the built-in list.
    fixed: Option<Vec<ServerSpec>>,
    running: tokio::sync::Mutex<HashMap<(String, PathBuf), Arc<client::Server>>>,
}

impl LspTool {
    pub fn new() -> Self {
        Self {
            fixed: None,
            running: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    fn with_servers(servers: Vec<ServerSpec>) -> Self {
        Self {
            fixed: Some(servers),
            ..Self::new()
        }
    }

    /// The servers to try for a file in `cwd`, configured ones first.
    fn candidates(&self, cwd: &Path) -> Vec<ServerSpec> {
        if let Some(fixed) = &self.fixed {
            return fixed.clone();
        }
        let mut out = Vec::new();
        let mut files = vec![cwd.join(".lsp.json")];
        if let Some(home) = dirs::home_dir() {
            files.push(home.join(".wizard").join("lsp.json"));
        }
        for file in files {
            if let Ok(text) = std::fs::read_to_string(file) {
                out.extend(configured_servers(&text));
            }
        }
        out.extend(builtin_servers());
        out
    }

    /// The server for `path`, started if it is not running yet. The `Err` is
    /// the sentence the model reads.
    async fn server_for(
        &self,
        operation: &str,
        path: &Path,
        cwd: &Path,
    ) -> Result<(Arc<client::Server>, String), String> {
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let candidates = self.candidates(cwd);
        let spec = candidates
            .iter()
            .find(|spec| spec.language_for(&extension).is_some() && on_path(&spec.command));
        let Some(spec) = spec else {
            let wanted: Vec<&str> = candidates
                .iter()
                .filter(|spec| spec.language_for(&extension).is_some())
                .map(|spec| spec.command.as_str())
                .collect();
            let hint = if wanted.is_empty() {
                "name one in .lsp.json".to_string()
            } else {
                format!(
                    "install {} or name another in .lsp.json",
                    wanted.join(" or ")
                )
            };
            return Err(format!(
                "No LSP server available for file type: {extension} ({hint})"
            ));
        };
        let language = spec
            .language_for(&extension)
            .unwrap_or("plaintext")
            .to_string();
        let root = project_root(path, cwd);
        let key = (spec.name.clone(), root.clone());
        let mut running = self.running.lock().await;
        if let Some(server) = running.get(&key) {
            return Ok((Arc::clone(server), language));
        }
        let server = client::Server::start(spec, &root)
            .await
            .map_err(|err| format!("Error performing {operation}: {err}"))?;
        running.insert(key, Arc::clone(&server));
        Ok((server, language))
    }

    async fn answer(&self, operation: &str, path: &Path, args: &Value, cwd: &Path) -> String {
        let (server, language) = match self.server_for(operation, path, cwd).await {
            Ok(found) => found,
            Err(message) => return message,
        };
        let fail = |err: &dyn std::fmt::Display| format!("Error performing {operation}: {err}");

        let metadata = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(err) => return fail(&err),
        };
        if metadata.len() > MAX_FILE_BYTES {
            return format!(
                "File too large for LSP analysis ({}MB exceeds 10MB limit)",
                metadata.len().div_ceil(1_000_000)
            );
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => return fail(&err),
        };
        let uri = match server.sync(path, &language, text).await {
            Ok(uri) => uri,
            Err(err) => return fail(&err),
        };

        let line = args.get("line").and_then(Value::as_i64).unwrap_or(1) - 1;
        let character = args.get("character").and_then(Value::as_i64).unwrap_or(1) - 1;
        let position = json!({ "line": line, "character": character });
        let at = json!({ "textDocument": { "uri": uri }, "position": position });
        let document = json!({ "textDocument": { "uri": uri } });
        let result = match operation {
            "goToDefinition" => server.request("textDocument/definition", at).await,
            "findReferences" => {
                let params = json!({
                    "textDocument": { "uri": uri },
                    "position": position,
                    "context": { "includeDeclaration": true },
                });
                server.request("textDocument/references", params).await
            }
            "hover" => server.request("textDocument/hover", at).await,
            "documentSymbol" => {
                server
                    .request("textDocument/documentSymbol", document)
                    .await
            }
            "workspaceSymbol" => {
                server
                    .request("workspace/symbol", json!({ "query": "" }))
                    .await
            }
            "goToImplementation" => server.request("textDocument/implementation", at).await,
            "prepareCallHierarchy" => {
                server
                    .request("textDocument/prepareCallHierarchy", at)
                    .await
            }
            _ => {
                let prepared = server
                    .request("textDocument/prepareCallHierarchy", at)
                    .await;
                match prepared {
                    Ok(items) => match items.as_array().and_then(|list| list.first()) {
                        Some(item) => {
                            let method = if operation == "incomingCalls" {
                                "callHierarchy/incomingCalls"
                            } else {
                                "callHierarchy/outgoingCalls"
                            };
                            server.request(method, json!({ "item": item })).await
                        }
                        None => {
                            return "No call hierarchy item found at this position".to_string();
                        }
                    },
                    Err(err) => Err(err),
                }
            }
        };
        let result = match result {
            Ok(result) => result,
            Err(err) => return fail(&err),
        };
        match operation {
            "goToDefinition" => format::definition(&result, cwd),
            "findReferences" => format::references(&result, cwd),
            "hover" => format::hover(&result),
            "documentSymbol" => format::document_symbols(&result, cwd),
            "workspaceSymbol" => format::workspace_symbols(&result, cwd),
            "goToImplementation" => format::implementation(&result, cwd),
            "prepareCallHierarchy" => format::prepare_call_hierarchy(&result, cwd),
            "incomingCalls" => format::incoming_calls(&result, cwd),
            _ => format::outgoing_calls(&result, cwd),
        }
    }
}

impl Default for LspTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "LSP"
    }

    fn description(&self) -> &str {
        "Interact with Language Server Protocol (LSP) servers to get code intelligence features.\n\n\
         Supported operations:\n\
         - goToDefinition: Find where a symbol is defined\n\
         - findReferences: Find all references to a symbol\n\
         - hover: Get hover information (documentation, type info) for a symbol\n\
         - documentSymbol: Get all symbols (functions, classes, variables) in a document\n\
         - workspaceSymbol: Search for symbols across the entire workspace\n\
         - goToImplementation: Find implementations of an interface or abstract method\n\
         - prepareCallHierarchy: Get call hierarchy item at a position (functions/methods)\n\
         - incomingCalls: Find all functions/methods that call the function at a position\n\
         - outgoingCalls: Find all functions/methods called by the function at a position\n\n\
         All operations require:\n\
         - filePath: The file to operate on\n\
         - line: The line number (1-based, as shown in editors)\n\
         - character: The character offset (1-based, as shown in editors)\n\n\
         Note: LSP servers must be configured for the file type. If no server is available, a \
         message says so. Wizard starts rust-analyzer, pyright, typescript-language-server, \
         gopls or clangd when they are on PATH, and any server named in .lsp.json."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": OPERATIONS, "description": "The LSP operation to perform" },
                "filePath": { "type": "string", "description": "The absolute or relative path to the file" },
                "line": { "type": "integer", "minimum": 1, "description": "The line number (1-based, as shown in editors)" },
                "character": { "type": "integer", "minimum": 1, "description": "The character offset (1-based, as shown in editors)" }
            },
            "required": ["operation", "filePath", "line", "character"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(operation) = str_arg(&args, "operation") else {
            return Ok(invalid(self.name(), "operation is required"));
        };
        if !OPERATIONS.contains(&operation) {
            return Ok(invalid(
                self.name(),
                format!("operation must be one of {}", OPERATIONS.join(", ")),
            ));
        }
        let Some(file) = str_arg(&args, "filePath") else {
            return Ok(invalid(self.name(), "filePath is required"));
        };
        for key in ["line", "character"] {
            if args.get(key).and_then(Value::as_i64).is_none_or(|n| n < 1) {
                return Ok(invalid(
                    self.name(),
                    format!("{key} must be a positive integer"),
                ));
            }
        }
        let path = resolve_path(ctx, file);
        match std::fs::metadata(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(format!("File does not exist: {file}")));
            }
            Err(err) => {
                return Ok(ToolOutput::error(format!(
                    "Cannot access file: {file}. {err}"
                )));
            }
            Ok(meta) if !meta.is_file() => {
                return Ok(ToolOutput::error(format!("Path is not a file: {file}")));
            }
            Ok(_) => {}
        }
        Ok(ToolOutput::ok(
            self.answer(operation, &path, &args, &ctx.cwd).await,
        ))
    }
}

/// `EnterPlanMode`.
pub struct EnterPlanModeTool {
    plan_mode: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self) -> &str {
        "Switch to plan mode for a task that needs design before code: only read-only tools run \
         until you present the plan with ExitPlanMode and it is approved."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    /// Read-only so the plan-mode gate lets it through when already planning.
    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if self.plan_mode.swap(true, Ordering::SeqCst) {
            return Ok(ToolOutput::ok(
                "Already in plan mode. Finish with ExitPlanMode when the plan is ready.",
            ));
        }
        Ok(ToolOutput::ok(
            "Entered plan mode. Explore with read-only tools, then present the plan with \
             ExitPlanMode. Nothing is edited or run until it is approved.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lsp_says_when_no_server_covers_the_file_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}").unwrap();
        let ctx = ToolContext::new(dir.path());
        let tool = LspTool::with_servers(Vec::new());
        let out = tool
            .execute(
                json!({ "operation": "hover", "filePath": "a.rs", "line": 1, "character": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content
                .starts_with("No LSP server available for file type: .rs"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn lsp_checks_its_input_like_claude_code() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        let ctx = ToolContext::new(dir.path());
        let tool = LspTool::with_servers(Vec::new());
        let run = |operation: &str, file: &str, line: i64| {
            let args =
                json!({ "operation": operation, "filePath": file, "line": line, "character": 1 });
            let tool = &tool;
            let ctx = &ctx;
            async move { tool.execute(args, ctx).await.unwrap() }
        };
        let bad = run("rename", "a.rs", 1).await;
        assert!(bad.content.contains("operation must be one of"));
        let zero = run("hover", "a.rs", 0).await;
        assert!(zero.content.contains("line must be a positive integer"));
        let missing = run("hover", "nope.rs", 1).await;
        assert_eq!(missing.content, "File does not exist: nope.rs");
        assert!(missing.is_error);
        let directory = run("hover", ".", 1).await;
        assert_eq!(directory.content, "Path is not a file: .");
    }

    /// A server that answers from a script, so the client's framing, handshake,
    /// document sync and formatting run end to end without a real language server.
    #[cfg(unix)]
    const FAKE_SERVER: &str = r#"
import json, sys

def read():
    n = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        if line.lower().startswith(b'content-length:'):
            n = int(line.split(b':')[1])
    return json.loads(sys.stdin.buffer.read(n))

def send(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(b'Content-Length: %d\r\n\r\n' % len(body) + body)
    sys.stdout.buffer.flush()

docs = {}
while True:
    msg = read()
    if msg is None:
        break
    method = msg.get('method')
    if method == 'initialize':
        send({'jsonrpc': '2.0', 'id': msg['id'], 'result': {'capabilities': {}}})
    elif method == 'textDocument/didOpen':
        d = msg['params']['textDocument']
        docs[d['uri']] = d['text']
    elif method == 'textDocument/didChange':
        docs[msg['params']['textDocument']['uri']] = msg['params']['contentChanges'][0]['text']
    elif method == 'textDocument/definition':
        uri = msg['params']['textDocument']['uri']
        send({'jsonrpc': '2.0', 'id': msg['id'], 'result': [{'uri': uri, 'range': {'start': {'line': 2, 'character': 4}, 'end': {'line': 2, 'character': 9}}}]})
    elif method == 'textDocument/hover':
        text = docs.get(msg['params']['textDocument']['uri'], '')
        send({'jsonrpc': '2.0', 'id': msg['id'], 'result': {'contents': {'kind': 'plaintext', 'value': 'doc has %d chars' % len(text)}}})
    elif 'id' in msg:
        send({'jsonrpc': '2.0', 'id': msg['id'], 'result': None})
"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn lsp_talks_to_a_server_and_words_the_answers() {
        if !on_path("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fake_server.py"), FAKE_SERVER).unwrap();
        std::fs::write(dir.path().join("a.zz"), "one\ntwo\nthree\n").unwrap();
        let spec = ServerSpec {
            name: "fake".to_string(),
            command: "python3".to_string(),
            args: vec![dir.path().join("fake_server.py").display().to_string()],
            languages: vec![(".zz".to_string(), "zz".to_string())],
        };
        let tool = LspTool::with_servers(vec![spec]);
        let ctx = ToolContext::new(dir.path());
        let call = |operation: &str| json!({ "operation": operation, "filePath": "a.zz", "line": 1, "character": 1 });
        let definition = tool.execute(call("goToDefinition"), &ctx).await.unwrap();
        assert_eq!(definition.content, "Defined in a.zz:3:5");
        let hover = tool.execute(call("hover"), &ctx).await.unwrap();
        assert_eq!(hover.content, "doc has 14 chars");
        // an edit on disk reaches the server before the next question
        std::fs::write(dir.path().join("a.zz"), "changed\n").unwrap();
        let again = tool.execute(call("hover"), &ctx).await.unwrap();
        assert_eq!(again.content, "doc has 8 chars");
        let empty = tool.execute(call("findReferences"), &ctx).await.unwrap();
        assert!(empty.content.starts_with("No references found."));
        assert_eq!(
            tool.running.lock().await.len(),
            1,
            "one server for all calls"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lsp_reports_a_server_that_will_not_start() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.zz"), "x").unwrap();
        let script = dir.path().join("exit_now.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spec = ServerSpec {
            name: "dead".to_string(),
            command: script.display().to_string(),
            args: Vec::new(),
            languages: vec![(".zz".to_string(), "zz".to_string())],
        };
        let tool = LspTool::with_servers(vec![spec]);
        let ctx = ToolContext::new(dir.path());
        let out = tool
            .execute(
                json!({ "operation": "hover", "filePath": "a.zz", "line": 1, "character": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content.starts_with("Error performing hover:"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn enter_plan_mode_sets_the_shared_flag_once() {
        let flag = Arc::new(AtomicBool::new(false));
        let tool = EnterPlanModeTool {
            plan_mode: Arc::clone(&flag),
        };
        let ctx = ToolContext::new(std::env::temp_dir());
        let first = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(first.content.starts_with("Entered plan mode."));
        assert!(flag.load(Ordering::SeqCst));
        let second = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(second.content.starts_with("Already in plan mode."));
    }
}
