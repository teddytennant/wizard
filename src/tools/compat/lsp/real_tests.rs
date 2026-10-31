//! The `LSP` tool against real language servers, on a project small enough to read.
//!
//! `WIZARD_REAL_LSP_PATH` is a `:`-separated list of directories holding the servers
//! (`pyright-langserver`, `rust-analyzer`, `gopls`, `typescript-language-server`). A test skips
//! when the server it needs is not there, so the plain suite never needs one installed. Set
//! `WIZARD_REAL_LSP_SHOW=1` to print every answer.

use std::path::Path;

use serde_json::json;

use super::*;

fn extra_path() -> Option<String> {
    std::env::var("WIZARD_REAL_LSP_PATH")
        .ok()
        .filter(|p| !p.is_empty())
}

/// Put the servers on PATH for this process; false when none were named.
fn servers_available() -> bool {
    let Some(extra) = extra_path() else {
        return false;
    };
    let path = std::env::var("PATH").unwrap_or_default();
    // SAFETY: set once before any server starts; the tests of this module are the only readers.
    unsafe { std::env::set_var("PATH", format!("{extra}:{path}")) };
    true
}

struct Probe<'a> {
    tool: &'a LspTool,
    ctx: &'a ToolContext,
    file: &'a str,
}

impl Probe<'_> {
    async fn ask(&self, operation: &str, line: u32, character: u32) -> String {
        let out = self
            .tool
            .execute(
                json!({ "operation": operation, "filePath": self.file, "line": line, "character": character }),
                self.ctx,
            )
            .await
            .unwrap();
        if std::env::var("WIZARD_REAL_LSP_SHOW").is_ok() {
            eprintln!(
                "--- {operation} {}:{line}:{character}\n{}",
                self.file, out.content
            );
        }
        out.content
    }
}

fn write(dir: &Path, name: &str, text: &str) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn pyright_answers_all_nine_operations() {
    if !servers_available() || !on_path("pyright-langserver") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "main.py",
        "class Greeter:\n    def greet(self, name):\n        return helper(name)\n\n\ndef helper(x):\n    return x\n\n\ndef main():\n    g = Greeter()\n    g.greet('a')\n    helper('b')\n",
    );
    let ctx = ToolContext::new(dir.path());
    let tool = LspTool::new();
    let p = Probe {
        tool: &tool,
        ctx: &ctx,
        file: "main.py",
    };

    let def = p.ask("goToDefinition", 3, 16).await;
    assert!(def.contains("main.py:6"), "{def}");
    let refs = p.ask("findReferences", 6, 5).await;
    assert!(refs.contains("Line 3:") && refs.contains("Line 13:"), "{refs}");
    let hover = p.ask("hover", 6, 5).await;
    assert!(hover.contains("helper"), "{hover}");
    let symbols = p.ask("documentSymbol", 1, 1).await;
    assert!(symbols.contains("Greeter") && symbols.contains("main"), "{symbols}");
    let workspace = p.ask("workspaceSymbol", 1, 1).await;
    assert!(!workspace.starts_with("Error"), "{workspace}");
    let implementation = p.ask("goToImplementation", 2, 9).await;
    let prepared = p.ask("prepareCallHierarchy", 6, 5).await;
    assert!(prepared.contains("helper"), "{prepared}");
    let incoming = p.ask("incomingCalls", 6, 5).await;
    assert!(incoming.contains("greet") && incoming.contains("main"), "{incoming}");
    let outgoing = p.ask("outgoingCalls", 10, 5).await;
    assert!(outgoing.contains("helper"), "{outgoing}");
    // pyright has no implementation provider; Claude Code words the refusal this way
    assert_eq!(
        implementation,
        "Error performing goToImplementation: LSP request 'textDocument/implementation' failed for server 'pyright': Unhandled method textDocument/implementation"
    );
    // positions with nothing under them
    assert!(
        p.ask("hover", 99, 1)
            .await
            .starts_with("No hover information available.")
    );
    assert!(
        p.ask("goToDefinition", 4, 1)
            .await
            .starts_with("No definition found.")
    );
    assert_eq!(
        p.ask("incomingCalls", 4, 1).await,
        "No call hierarchy item found at this position"
    );
}

#[tokio::test]
async fn rust_analyzer_answers_all_nine_operations() {
    if !servers_available() || !on_path("rust-analyzer") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        dir.path(),
        "src/lib.rs",
        "pub trait Shape {\n    fn area(&self) -> f64;\n}\n\npub struct Square(pub f64);\n\nimpl Shape for Square {\n    fn area(&self) -> f64 {\n        self.0 * self.0\n    }\n}\n\npub fn total(shapes: &[Square]) -> f64 {\n    shapes.iter().map(|s| s.area()).sum()\n}\n\npub fn report() -> f64 {\n    total(&[Square(2.0)])\n}\n",
    );
    let ctx = ToolContext::new(dir.path());
    let tool = LspTool::new();
    let p = Probe {
        tool: &tool,
        ctx: &ctx,
        file: "src/lib.rs",
    };

    let def = p.ask("goToDefinition", 18, 5).await;
    assert!(def.contains("src/lib.rs:13"), "{def}");
    let refs = p.ask("findReferences", 13, 8).await;
    assert!(refs.contains("Line 18:5"), "{refs}");
    let hover = p.ask("hover", 13, 8).await;
    assert!(hover.contains("total"), "{hover}");
    let symbols = p.ask("documentSymbol", 1, 1).await;
    assert!(symbols.contains("Shape") && symbols.contains("Square"), "{symbols}");
    let workspace = p.ask("workspaceSymbol", 1, 1).await;
    assert!(!workspace.starts_with("Error"), "{workspace}");
    let implementation = p.ask("goToImplementation", 2, 8).await;
    assert!(implementation.contains("src/lib.rs:8"), "{implementation}");
    let prepared = p.ask("prepareCallHierarchy", 13, 8).await;
    assert!(prepared.contains("total"), "{prepared}");
    let incoming = p.ask("incomingCalls", 13, 8).await;
    assert!(incoming.contains("report"), "{incoming}");
    let outgoing = p.ask("outgoingCalls", 17, 8).await;
    assert!(outgoing.contains("total"), "{outgoing}");
}

#[tokio::test]
async fn gopls_answers_all_nine_operations() {
    if !servers_available() || !on_path("gopls") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "go.mod", "module demo\n\ngo 1.21\n");
    write(
        dir.path(),
        "main.go",
        "package main\n\ntype Shape interface {\n\tArea() float64\n}\n\ntype Square struct{ side float64 }\n\nfunc (s Square) Area() float64 { return s.side * s.side }\n\nfunc total(shapes []Shape) float64 {\n\tsum := 0.0\n\tfor _, s := range shapes {\n\t\tsum += s.Area()\n\t}\n\treturn sum\n}\n\nfunc main() {\n\ttotal([]Shape{Square{2}})\n}\n",
    );
    let ctx = ToolContext::new(dir.path());
    let tool = LspTool::new();
    let p = Probe {
        tool: &tool,
        ctx: &ctx,
        file: "main.go",
    };

    let def = p.ask("goToDefinition", 20, 2).await;
    assert!(def.contains("main.go:11"), "{def}");
    let refs = p.ask("findReferences", 11, 6).await;
    assert!(refs.contains("Line 20:2"), "{refs}");
    let hover = p.ask("hover", 11, 6).await;
    assert!(hover.contains("total"), "{hover}");
    let symbols = p.ask("documentSymbol", 1, 1).await;
    assert!(symbols.contains("Square") && symbols.contains("main"), "{symbols}");
    let workspace = p.ask("workspaceSymbol", 1, 1).await;
    assert!(!workspace.starts_with("Error"), "{workspace}");
    let implementation = p.ask("goToImplementation", 4, 3).await;
    assert!(implementation.contains("main.go:9"), "{implementation}");
    let prepared = p.ask("prepareCallHierarchy", 11, 6).await;
    assert!(prepared.contains("total"), "{prepared}");
    let incoming = p.ask("incomingCalls", 11, 6).await;
    assert!(incoming.contains("main"), "{incoming}");
    let outgoing = p.ask("outgoingCalls", 19, 6).await;
    assert!(outgoing.contains("total"), "{outgoing}");
}

#[tokio::test]
async fn typescript_language_server_answers_all_nine_operations() {
    if !servers_available() || !on_path("typescript-language-server") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "tsconfig.json",
        "{\"compilerOptions\": {\"target\": \"es2020\", \"strict\": true}, \"include\": [\"*.ts\"]}\n",
    );
    write(
        dir.path(),
        "main.ts",
        "interface Shape {\n  area(): number;\n}\n\nclass Square implements Shape {\n  constructor(private side: number) {}\n  area(): number {\n    return this.side * this.side;\n  }\n}\n\nfunction total(shapes: Shape[]): number {\n  return shapes.reduce((sum, s) => sum + s.area(), 0);\n}\n\nfunction main(): number {\n  return total([new Square(2)]);\n}\n",
    );
    let ctx = ToolContext::new(dir.path());
    let tool = LspTool::new();
    let p = Probe {
        tool: &tool,
        ctx: &ctx,
        file: "main.ts",
    };

    let def = p.ask("goToDefinition", 17, 10).await;
    assert!(def.contains("main.ts:12"), "{def}");
    let refs = p.ask("findReferences", 12, 10).await;
    assert!(refs.contains("Line 17:"), "{refs}");
    let hover = p.ask("hover", 12, 10).await;
    assert!(hover.contains("total"), "{hover}");
    let symbols = p.ask("documentSymbol", 1, 1).await;
    assert!(symbols.contains("Square") && symbols.contains("main"), "{symbols}");
    let workspace = p.ask("workspaceSymbol", 1, 1).await;
    assert!(!workspace.starts_with("Error"), "{workspace}");
    let implementation = p.ask("goToImplementation", 2, 3).await;
    assert!(implementation.contains("main.ts:7"), "{implementation}");
    let prepared = p.ask("prepareCallHierarchy", 12, 10).await;
    assert!(prepared.contains("total"), "{prepared}");
    let incoming = p.ask("incomingCalls", 12, 10).await;
    assert!(incoming.contains("main"), "{incoming}");
    let outgoing = p.ask("outgoingCalls", 16, 10).await;
    assert!(outgoing.contains("total"), "{outgoing}");
}
