//! A dev probe, not a test: runs a file of tool calls through the compat tools
//! and prints each result in the shape of `batch.py`, which does the same
//! against `claude mcp serve`, so the two can be diffed.
//!
//! ```text
//! WIZARD_COMPAT_CASES=cases.json cargo test --lib compat::probe -- --ignored --nocapture
//! ```
//!
//! `cases.json` is `{"files": {"rel/path": "content"}, "calls": [["Tool", {args}]]}`
//! and `{P}` in an argument is the scratch project directory.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use super::install_all;
use crate::tools::ToolContext;
use crate::tools::registry::ToolRegistry;

#[tokio::test]
#[ignore = "dev probe: set WIZARD_COMPAT_CASES to a cases file"]
async fn probe() {
    let Ok(cases) = std::env::var("WIZARD_COMPAT_CASES") else {
        return;
    };
    let cases: Value = serde_json::from_str(&std::fs::read_to_string(cases).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().display().to_string();
    if let Some(files) = cases["files"].as_object() {
        for (rel, content) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content.as_str().unwrap()).unwrap();
        }
    }
    let mut registry = ToolRegistry::with_native_tools();
    install_all(&mut registry, Arc::new(AtomicBool::new(false)));
    let ctx = ToolContext::new(dir.path());
    for call in cases["calls"].as_array().unwrap() {
        let name = call[0].as_str().unwrap();
        let args: Value =
            serde_json::from_str(&call[1].to_string().replace("{P}", &project)).unwrap();
        println!("### {name} {args}");
        match registry.execute(name, args, &ctx).await {
            Ok(out) => {
                println!("isError={}", if out.is_error { "True" } else { "None" });
                println!("{}", out.content.replace(&project, "$P"));
            }
            Err(err) => println!("TOOL ERROR {err}"),
        }
    }
}
