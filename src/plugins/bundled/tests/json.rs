//! `json_query`, the first tool written in JavaScript.
//!
//! Every test here goes through the real kernel, the real bundled loader and
//! the real `WizardHost` — the same fixtures the Lua plugins use, which is the
//! claim the backend is making: a plugin's language is not visible from the
//! outside. There is no `ScriptedHost` counterpart because this plugin runs no
//! programs and opens no sockets; the only host call it makes is
//! `wizard.fs.read`, and that is arrangeable by writing a file.
//!
//! Two things are asserted here that no Lua plugin could be. The confinement
//! test drives `wizard.fs`'s project-root rule from a plugin that declared no
//! capabilities, which is what `manifest.toml` claims is enough. And the round
//! trip preserves empty arrays and empty objects, which is the reason this
//! plugin is JavaScript rather than Lua at all.

use serde_json::json;

use crate::kernel::testing::TempDir;
use crate::tools::{ToolAccess, ToolKind};

use super::{bundled_kernel, call};

/// A temp directory holding one JSON file, and the kernel rooted at it.
///
/// The kernel's project root is the confinement `wizard.fs` applies, so the
/// fixture and the sandbox are the same directory by construction rather than
/// by a test remembering to line them up.
async fn fixture(name: &str, file: &str, contents: &str) -> (TempDir, crate::kernel::Kernel) {
    let dir = TempDir::new(name);
    std::fs::write(dir.path.join(file), contents).expect("the fixture file");
    let kernel = bundled_kernel(&dir.path).await;
    (dir, kernel)
}

const PACKAGE_JSON: &str = r#"{
  "name": "demo",
  "version": "1.2.3",
  "keywords": [],
  "scripts": { "build": "tsc", "test": "vitest" },
  "dependencies": { "left-pad": "^1.3.0", "chalk": "^5.0.0" },
  "contributors": [
    { "name": "Ada", "email": "ada@example.com" },
    { "name": "Grace", "email": "grace@example.com" }
  ],
  "config": {}
}"#;

#[tokio::test]
async fn the_tool_declares_the_shape_the_model_is_told_about() {
    let dir = TempDir::new("json-shape");
    let kernel = bundled_kernel(&dir.path).await;
    let tool = kernel.tool("json_query").expect("registered");

    assert_eq!(tool.access(), ToolAccess::ReadOnly);
    assert_eq!(tool.kind(), ToolKind::Scripted);
    let schema = tool.parameters();
    assert_eq!(schema["type"], json!("object"));
    for key in ["path", "text", "query"] {
        assert_eq!(
            schema["properties"][key]["type"],
            json!("string"),
            "the '{key}' argument"
        );
    }
    assert!(
        tool.description().contains("query"),
        "the description has to tell the model the syntax: {}",
        tool.description()
    );
}

#[tokio::test]
async fn a_dotted_path_selects_one_value() {
    let (dir, kernel) = fixture("json-dotted", "package.json", PACKAGE_JSON).await;
    let out = call(
        &kernel,
        "json_query",
        json!({ "path": "package.json", "query": "dependencies.chalk" }),
        &dir.path,
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("\"^5.0.0\""), "{}", out.content);
    // The header names the query and the shape, so a model reading a wall of
    // JSON knows what it asked for and what came back.
    assert!(
        out.content.starts_with("dependencies.chalk (string)"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn an_index_and_a_negative_index_both_work() {
    let (dir, kernel) = fixture("json-index", "package.json", PACKAGE_JSON).await;
    let first = call(
        &kernel,
        "json_query",
        json!({ "path": "package.json", "query": "contributors[0].name" }),
        &dir.path,
    )
    .await;
    assert!(first.content.contains("Ada"), "{}", first.content);

    let last = call(
        &kernel,
        "json_query",
        json!({ "path": "package.json", "query": "contributors[-1].name" }),
        &dir.path,
    )
    .await;
    assert!(last.content.contains("Grace"), "{}", last.content);
}

#[tokio::test]
async fn a_wildcard_fans_out_and_the_rest_of_the_path_applies_to_each_branch() {
    let (dir, kernel) = fixture("json-wildcard", "package.json", PACKAGE_JSON).await;
    let out = call(
        &kernel,
        "json_query",
        json!({ "path": "package.json", "query": "contributors[*].email" }),
        &dir.path,
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("2 matches"), "{}", out.content);
    assert!(out.content.contains("ada@example.com"), "{}", out.content);
    assert!(out.content.contains("grace@example.com"), "{}", out.content);

    // `.*` over an object walks its values, which is how "every script" is
    // asked for.
    let scripts = call(
        &kernel,
        "json_query",
        json!({ "path": "package.json", "query": "scripts.*" }),
        &dir.path,
    )
    .await;
    assert!(scripts.content.contains("tsc"), "{}", scripts.content);
    assert!(scripts.content.contains("vitest"), "{}", scripts.content);
}

#[tokio::test]
async fn a_key_that_is_not_there_is_an_answer_rather_than_an_error() {
    // Deliberately not `is_error`. The document parsed and the query parsed;
    // what happened is that the key is absent, which is the fact the model
    // asked for and can act on.
    let (dir, kernel) = fixture("json-missing", "package.json", PACKAGE_JSON).await;
    let out = call(
        &kernel,
        "json_query",
        json!({ "path": "package.json", "query": "dependencies.nope" }),
        &dir.path,
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "No match for 'dependencies.nope'.");
}

#[tokio::test]
async fn a_quoted_segment_reaches_a_key_with_a_dot_in_it() {
    let (dir, kernel) = fixture(
        "json-quoted",
        "tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@app/*": ["src/*"] } } }"#,
    )
    .await;
    let out = call(
        &kernel,
        "json_query",
        json!({ "path": "tsconfig.json", "query": "compilerOptions.paths[\"@app/*\"]" }),
        &dir.path,
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("src/*"), "{}", out.content);
}

#[tokio::test]
async fn a_prototype_key_answers_about_the_document_and_not_about_object_prototype() {
    // `"constructor" in node` is true for every object in JavaScript, so a
    // walk written with `in` rather than `Object.hasOwn` would answer this
    // with a function and then fail to serialise it.
    let (dir, kernel) = fixture("json-proto", "doc.json", r#"{ "a": 1 }"#).await;
    let out = call(
        &kernel,
        "json_query",
        json!({ "path": "doc.json", "query": "constructor" }),
        &dir.path,
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "No match for 'constructor'.");
}

#[tokio::test]
async fn inline_text_is_queried_without_touching_a_file() {
    let dir = TempDir::new("json-inline");
    let kernel = bundled_kernel(&dir.path).await;
    let out = call(
        &kernel,
        "json_query",
        json!({ "text": r#"{"items":[{"id":7}]}"#, "query": "items[0].id" }),
        &dir.path,
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains('7'), "{}", out.content);
}

#[tokio::test]
async fn an_empty_query_returns_the_whole_document_pretty_printed() {
    let (dir, kernel) = fixture("json-whole", "doc.json", r#"{"a":{"b":[1,2]}}"#).await;
    let out = call(
        &kernel,
        "json_query",
        json!({ "path": "doc.json" }),
        &dir.path,
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    let parsed: serde_json::Value = serde_json::from_str(&out.content).expect("valid JSON");
    assert_eq!(parsed, json!({"a": {"b": [1, 2]}}));
    assert!(
        out.content.contains('\n'),
        "pretty-printed: {}",
        out.content
    );
}

#[tokio::test]
async fn an_empty_array_stays_an_array_and_an_empty_object_stays_an_object() {
    // The whole reason this plugin is JavaScript. `docs/plugins.md` records
    // that a Lua plugin cannot write an empty JSON array at all and that mlua
    // serialises an empty table as `[]`, so a Lua `json_query` would hand back
    // `{"keywords": {}, "config": []}` — or one of the two, depending on which
    // way the guess fell — and quietly corrupt the document it was asked to
    // read.
    let (dir, kernel) = fixture("json-empties", "package.json", PACKAGE_JSON).await;

    let array = call(
        &kernel,
        "json_query",
        json!({ "path": "package.json", "query": "keywords" }),
        &dir.path,
    )
    .await;
    assert!(array.content.contains("(array of 0)"), "{}", array.content);
    assert!(
        array.content.trim_end().ends_with("[]"),
        "{}",
        array.content
    );

    let object = call(
        &kernel,
        "json_query",
        json!({ "path": "package.json", "query": "config" }),
        &dir.path,
    )
    .await;
    assert!(
        object.content.contains("(object with 0 keys)"),
        "{}",
        object.content
    );
    assert!(
        object.content.trim_end().ends_with("{}"),
        "{}",
        object.content
    );
}

#[tokio::test]
async fn a_document_that_is_not_json_is_a_soft_failure_naming_the_reason() {
    let (dir, kernel) = fixture("json-bad", "broken.json", "{ not json at all").await;
    let out = call(
        &kernel,
        "json_query",
        json!({ "path": "broken.json", "query": "a" }),
        &dir.path,
    )
    .await;
    assert!(out.is_error, "{}", out.content);
    // The model needs the parser's own complaint, not "tool failed".
    assert!(
        out.content.to_lowercase().contains("json")
            || out.content.to_lowercase().contains("unexpected"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn a_malformed_query_says_so_rather_than_returning_nothing() {
    let (dir, kernel) = fixture("json-badquery", "doc.json", r#"{"a":1}"#).await;
    for (query, needle) in [("a[", "unclosed"), ("a[b]", "not an index")] {
        let out = call(
            &kernel,
            "json_query",
            json!({ "path": "doc.json", "query": query }),
            &dir.path,
        )
        .await;
        assert!(out.is_error, "{query}: {}", out.content);
        assert!(out.content.contains(needle), "{query}: {}", out.content);
    }
}

#[tokio::test]
async fn neither_a_path_nor_text_is_a_soft_failure() {
    let dir = TempDir::new("json-neither");
    let kernel = bundled_kernel(&dir.path).await;
    let out = call(&kernel, "json_query", json!({ "query": "a" }), &dir.path).await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("'path' or 'text'"), "{}", out.content);
}

#[tokio::test]
async fn a_plugin_that_declared_no_capabilities_cannot_read_outside_the_project() {
    // The manifest's claim, driven from the far end. `json` declares nothing,
    // so `wizard.fs.read` is confined to the kernel's project root — and the
    // refusal has to reach the model as the tool's own bad news rather than as
    // a broken tool, because "that file is outside the project" is something
    // the model can act on.
    let (dir, kernel) = fixture("json-confined", "inside.json", r#"{"ok":true}"#).await;

    let inside = call(
        &kernel,
        "json_query",
        json!({ "path": "inside.json", "query": "ok" }),
        &dir.path,
    )
    .await;
    assert!(!inside.is_error, "{}", inside.content);

    for path in ["/etc/hostname", "../outside.json", "~/.ssh/config"] {
        let out = call(
            &kernel,
            "json_query",
            json!({ "path": path, "query": "a" }),
            &dir.path,
        )
        .await;
        assert!(out.is_error, "{path}: {}", out.content);
        assert!(
            out.content.contains("outside the project") || out.content.contains("climbs out"),
            "{path}: {}",
            out.content
        );
    }
}

#[tokio::test]
async fn a_wildcard_that_matches_too_much_says_so_rather_than_returning_it_all() {
    let dir = TempDir::new("json-flood");
    let items: Vec<serde_json::Value> = (0..600).map(|n| json!({ "id": n })).collect();
    std::fs::write(
        dir.path.join("big.json"),
        serde_json::to_string(&json!({ "items": items })).expect("json"),
    )
    .expect("the fixture file");
    let kernel = bundled_kernel(&dir.path).await;

    let out = call(
        &kernel,
        "json_query",
        json!({ "path": "big.json", "query": "items[*].id" }),
        &dir.path,
    )
    .await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("Narrow the query"), "{}", out.content);
}
