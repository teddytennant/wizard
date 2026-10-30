//! A small Language Server Protocol client over a child process's stdio.
//!
//! One [`Server`] per (server, project root), started on first use and kept
//! until the session ends. It speaks just enough of the protocol for the `LSP`
//! tool: `initialize`, `didOpen`/`didChange`, requests with a deadline, and
//! answers to the few requests a server makes of its client so that it does
//! not stall waiting for us.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

/// How long `initialize` may take.
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the first request waits for a fresh server to finish indexing.
const SETTLE_LIMIT: Duration = Duration::from_secs(45);
/// A server that has been quiet this long is taken to be done indexing.
const SETTLE_QUIET: Duration = Duration::from_millis(1500);
/// How long one request may take once the server is ready.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A server to run for some file extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// File extension (with the dot) to LSP `languageId`.
    pub languages: Vec<(String, String)>,
}

impl ServerSpec {
    pub fn language_for(&self, extension: &str) -> Option<&str> {
        self.languages
            .iter()
            .find(|(ext, _)| ext.eq_ignore_ascii_case(extension))
            .map(|(_, id)| id.as_str())
    }
}

type Pending = Arc<std::sync::Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

/// What the reader has seen the server do, for deciding when it is ready.
#[derive(Default)]
struct Activity {
    /// Open `$/progress` tokens.
    active: HashSet<String>,
    last: Option<Instant>,
    ever_busy: bool,
    quiescent: bool,
}

/// A running language server.
pub struct Server {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicI64,
    activity: Arc<std::sync::Mutex<Activity>>,
    /// Open documents: uri to (version, text).
    open: Mutex<HashMap<String, (i64, String)>>,
    started: Instant,
    ready: Mutex<bool>,
    /// The end of what the server wrote to stderr, for saying why it died.
    stderr: Arc<std::sync::Mutex<String>>,
    _child: Child,
}

/// `file://` URI for an absolute path.
pub fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The path a `file://` URI names, percent escapes decoded.
pub fn uri_to_path(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Server {
    /// Start `spec` rooted at `root` and complete the `initialize` handshake.
    pub async fn start(spec: &ServerSpec, root: &Path) -> Result<Arc<Self>, String> {
        let mut child = Command::new(&spec.command)
            .args(&spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| format!("could not start {}: {err}", spec.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stdin = Arc::new(Mutex::new(stdin));
        let stderr_tail: Arc<std::sync::Mutex<String>> = Arc::default();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_tail)));
        }
        let pending: Pending = Arc::default();
        let activity: Arc<std::sync::Mutex<Activity>> = Arc::default();
        tokio::spawn(read_loop(
            BufReader::new(stdout),
            Arc::clone(&stdin),
            Arc::clone(&pending),
            Arc::clone(&activity),
        ));
        let server = Arc::new(Server {
            stdin,
            pending,
            next_id: AtomicI64::new(1),
            activity,
            open: Mutex::new(HashMap::new()),
            started: Instant::now(),
            ready: Mutex::new(false),
            stderr: stderr_tail,
            _child: child,
        });
        let root_uri = path_to_uri(root);
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".to_string());
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "wizard" },
            "rootUri": root_uri,
            "rootPath": root.to_string_lossy(),
            "workspaceFolders": [{ "uri": root_uri, "name": name }],
            "capabilities": {
                "general": { "positionEncodings": ["utf-16"] },
                "textDocument": {
                    "synchronization": { "didSave": false },
                    "definition": { "linkSupport": true },
                    "implementation": { "linkSupport": true },
                    "references": {},
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "callHierarchy": {},
                },
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true,
                    "symbol": {},
                },
                "window": { "workDoneProgress": true },
                "experimental": { "serverStatusNotification": true },
            },
        });
        server
            .request_within("initialize", params, INIT_TIMEOUT)
            .await
            .map_err(|err| server.explain(err))?;
        server.notify("initialized", json!({})).await?;
        Ok(server)
    }

    /// `err`, with the last of the server's stderr when it has said anything.
    fn explain(&self, err: String) -> String {
        let tail = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        let tail = tail.trim();
        if tail.is_empty() {
            err
        } else {
            format!("{err} (server said: {tail})")
        }
    }

    async fn send(&self, message: &Value) -> Result<(), String> {
        write_message(&self.stdin, message)
            .await
            .map_err(|err| format!("language server closed its input: {err}"))
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn request_within(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let sent = self
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        if let Err(err) = sent {
            self.pending.lock().unwrap().remove(&id);
            return Err(err);
        }
        match tokio::time::timeout(limit, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => {
                // Let the stderr reader catch up with whatever it said last.
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err(self.explain("the language server exited".to_string()))
            }
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!(
                    "{method} timed out after {}s",
                    limit.as_secs().max(1)
                ))
            }
        }
    }

    /// Send a request, after the server has had its first chance to index.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.settle().await;
        self.request_within(method, params, REQUEST_TIMEOUT).await
    }

    /// Wait, once, until the server stops reporting progress. A server that
    /// never reports any is given a moment and then used as it is.
    async fn settle(&self) {
        let mut ready = self.ready.lock().await;
        if *ready {
            return;
        }
        let deadline = Instant::now() + SETTLE_LIMIT;
        while Instant::now() < deadline {
            let done = {
                let activity = self.activity.lock().unwrap();
                if activity.quiescent {
                    true
                } else if !activity.active.is_empty() {
                    false
                } else {
                    let quiet_since = activity.last.unwrap_or(self.started);
                    quiet_since.elapsed() >= SETTLE_QUIET
                        && (activity.ever_busy || self.started.elapsed() >= SETTLE_QUIET)
                }
            };
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        *ready = true;
    }

    /// Make sure the server has the current text of `path`.
    pub async fn sync(&self, path: &Path, language: &str, text: String) -> Result<String, String> {
        let uri = path_to_uri(path);
        let mut open = self.open.lock().await;
        match open.get_mut(&uri) {
            Some((_, have)) if *have == text => {}
            Some((version, have)) => {
                *version += 1;
                *have = text.clone();
                let version = *version;
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }],
                    }),
                )
                .await?;
            }
            None => {
                open.insert(uri.clone(), (1, text.clone()));
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language,
                            "version": 1,
                            "text": text,
                        },
                    }),
                )
                .await?;
            }
        }
        Ok(uri)
    }
}

/// Keep the last few hundred bytes of the server's stderr.
async fn drain_stderr(stderr: tokio::process::ChildStderr, tail: Arc<std::sync::Mutex<String>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
        if !tail.is_empty() {
            tail.push(' ');
        }
        tail.push_str(line.trim());
        if tail.len() > 600 {
            let cut = tail.len() - 600;
            let at = (cut..tail.len())
                .find(|i| tail.is_char_boundary(*i))
                .unwrap_or(tail.len());
            *tail = tail[at..].to_string();
        }
    }
}

async fn write_message(stdin: &Mutex<ChildStdin>, message: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(message)?;
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    stdin.write_all(&body).await?;
    stdin.flush().await
}

async fn read_message<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim())
        {
            length = value.parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; length?];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// Route everything the server sends: answers to our requests, its own
/// requests (which get a plain reply), and notifications we track.
async fn read_loop<R: AsyncBufReadExt + Unpin>(
    mut reader: R,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    activity: Arc<std::sync::Mutex<Activity>>,
) {
    while let Some(message) = read_message(&mut reader).await {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                let result = match method {
                    "workspace/configuration" => {
                        let count = message
                            .pointer("/params/items")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; count])
                    }
                    "workspace/workspaceFolders" => Value::Null,
                    _ => Value::Null,
                };
                let _ = write_message(
                    &stdin,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                )
                .await;
            }
            (Some(method), None) => note(method, &message, &activity),
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let waiter = pending.lock().unwrap().remove(&id);
                if let Some(waiter) = waiter {
                    let answer = match message.get("error") {
                        Some(error) => Err(error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("language server error")
                            .to_string()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = waiter.send(answer);
                }
            }
            (None, None) => {}
        }
    }
    // The server is gone: nobody will answer what is still waiting.
    pending.lock().unwrap().clear();
}

fn note(method: &str, message: &Value, activity: &std::sync::Mutex<Activity>) {
    let mut activity = activity.lock().unwrap();
    match method {
        "$/progress" => {
            let token = message
                .pointer("/params/token")
                .map(Value::to_string)
                .unwrap_or_default();
            match message
                .pointer("/params/value/kind")
                .and_then(Value::as_str)
            {
                Some("begin") => {
                    activity.active.insert(token);
                    activity.ever_busy = true;
                }
                Some("end") => {
                    activity.active.remove(&token);
                }
                _ => {}
            }
            activity.last = Some(Instant::now());
        }
        "experimental/serverStatus" => {
            if message
                .pointer("/params/quiescent")
                .and_then(Value::as_bool)
                == Some(true)
            {
                activity.quiescent = true;
            }
            activity.last = Some(Instant::now());
        }
        _ => {}
    }
}

/// The project root a server should be started in: the closest directory
/// above `file` holding a build file, else `cwd`.
pub fn project_root(file: &Path, cwd: &Path) -> PathBuf {
    const MARKERS: [&str; 8] = [
        "Cargo.toml",
        "package.json",
        "go.mod",
        "pyproject.toml",
        "setup.py",
        "tsconfig.json",
        "compile_commands.json",
        ".git",
    ];
    let mut dir = file.parent();
    while let Some(candidate) = dir {
        if MARKERS.iter().any(|m| candidate.join(m).exists()) {
            return candidate.to_path_buf();
        }
        if candidate == cwd {
            break;
        }
        dir = candidate.parent();
    }
    cwd.to_path_buf()
}

/// Servers Wizard knows how to find on `PATH`.
pub fn builtin_servers() -> Vec<ServerSpec> {
    let spec = |name: &str, command: &str, args: &[&str], languages: &[(&str, &str)]| ServerSpec {
        name: name.to_string(),
        command: command.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        languages: languages
            .iter()
            .map(|(ext, id)| (ext.to_string(), id.to_string()))
            .collect(),
    };
    let python = [(".py", "python"), (".pyi", "python")];
    let web = [
        (".ts", "typescript"),
        (".tsx", "typescriptreact"),
        (".js", "javascript"),
        (".jsx", "javascriptreact"),
        (".mjs", "javascript"),
        (".cjs", "javascript"),
    ];
    vec![
        spec("rust-analyzer", "rust-analyzer", &[], &[(".rs", "rust")]),
        spec("pyright", "pyright-langserver", &["--stdio"], &python),
        spec(
            "basedpyright",
            "basedpyright-langserver",
            &["--stdio"],
            &python,
        ),
        spec("pylsp", "pylsp", &[], &python),
        spec(
            "typescript-language-server",
            "typescript-language-server",
            &["--stdio"],
            &web,
        ),
        spec("gopls", "gopls", &[], &[(".go", "go")]),
        spec(
            "clangd",
            "clangd",
            &[],
            &[
                (".c", "c"),
                (".h", "c"),
                (".cc", "cpp"),
                (".cpp", "cpp"),
                (".hpp", "cpp"),
            ],
        ),
    ]
}

/// Servers named in `.lsp.json`, in Claude Code's plugin format:
/// `{"name": {"command": "...", "args": [...], "extensionToLanguage": {".rs": "rust"}}}`.
pub fn configured_servers(text: &str) -> Vec<ServerSpec> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    map.into_iter()
        .filter_map(|(name, entry)| {
            let command = entry.get("command")?.as_str()?.to_string();
            let args = entry
                .get("args")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|a| a.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let languages: Vec<(String, String)> = entry
                .get("extensionToLanguage")?
                .as_object()?
                .iter()
                .filter_map(|(ext, id)| Some((ext.clone(), id.as_str()?.to_string())))
                .collect();
            Some(ServerSpec {
                name,
                command,
                args,
                languages,
            })
        })
        .collect()
}

/// A command that can be run: a path that exists, or a name found on `PATH`.
pub fn on_path(command: &str) -> bool {
    let direct = Path::new(command);
    if direct.components().count() > 1 {
        return direct.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(command).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip_odd_paths() {
        let path = Path::new("/tmp/a b/ü#1.rs");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///tmp/a%20b/%C3%BC%231.rs");
        assert_eq!(uri_to_path(&uri), "/tmp/a b/ü#1.rs");
    }

    #[test]
    fn a_config_in_the_plugin_format_names_servers_by_extension() {
        let servers = configured_servers(
            r#"{"zls":{"command":"zls","args":["--x"],"extensionToLanguage":{".zig":"zig"}},
               "broken":{"command":"x"}}"#,
        );
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "zls");
        assert_eq!(servers[0].args, ["--x"]);
        assert_eq!(servers[0].language_for(".zig"), Some("zig"));
        assert_eq!(servers[0].language_for(".rs"), None);
    }

    #[test]
    fn the_root_is_the_nearest_build_file() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("crates/a/src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("crates/a/Cargo.toml"), "").unwrap();
        assert_eq!(
            project_root(&sub.join("lib.rs"), dir.path()),
            dir.path().join("crates/a")
        );
        assert_eq!(
            project_root(&dir.path().join("x.rs"), dir.path()),
            dir.path()
        );
    }

    #[test]
    fn builtin_servers_cover_the_four_languages() {
        let servers = builtin_servers();
        for ext in [".rs", ".py", ".ts", ".go"] {
            assert!(
                servers.iter().any(|s| s.language_for(ext).is_some()),
                "{ext}"
            );
        }
    }
}
