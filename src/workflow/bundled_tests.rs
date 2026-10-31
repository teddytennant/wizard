//! The workflows that ship in the binary: they parse, pass the same checks a
//! model-written script does, show up next to saved ones, and run end to end
//! against scripted agents.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::engine::{AgentOutcome, AgentRequest, AgentRunner, Engine, Limits, Quiet};
use super::live::Cancel;
use super::script;
use super::store;
use super::vm;

#[test]
fn every_bundled_script_parses_and_names_itself() {
    for (file, text) in store::BUNDLED {
        let parsed = script::parse(text).unwrap_or_else(|why| panic!("{file}: {why}"));
        assert_eq!(&parsed.meta.name, file, "meta.name matches the file");
        assert!(
            parsed.meta.when_to_use.is_some(),
            "{file} says when to use it"
        );
        assert!(!parsed.meta.phases.is_empty(), "{file} lists its phases");
        assert_eq!(script::forbidden_in_body(&parsed.body), None, "{file}");
    }
}

#[tokio::test]
async fn every_bundled_script_compiles() {
    for (file, text) in store::BUNDLED {
        let parsed = script::parse(text).unwrap();
        vm::check_syntax(&parsed.body)
            .await
            .unwrap_or_else(|why| panic!("{file}: {why}"));
    }
}

#[test]
fn bundled_workflows_are_listed_and_a_file_of_the_same_name_wins() {
    let tmp = std::env::temp_dir().join(format!("wizard-bundled-{}", uuid::Uuid::new_v4()));
    let dir = tmp.join(".wizard").join("workflows");
    std::fs::create_dir_all(&dir).unwrap();
    let (found, problems) = store::discover(&tmp, None);
    assert!(problems.is_empty(), "{problems:?}");
    for name in ["deep-research", "code-review", "batch"] {
        let one = found.iter().find(|n| n.name == name).expect(name);
        assert_eq!(one.origin, store::Origin::Bundled);
        assert!(one.read().unwrap().contains("export const meta"));
    }

    std::fs::write(
        dir.join("batch.js"),
        "export const meta = {name: 'batch', description: 'mine'}\nreturn 1\n",
    )
    .unwrap();
    let mine = store::find_named(&tmp, None, "batch").unwrap();
    assert_eq!(mine.origin, store::Origin::Project);
    assert_eq!(mine.meta.description, "mine");
    let _ = std::fs::remove_dir_all(&tmp);
}

fn has_property(request: &AgentRequest, name: &str) -> bool {
    request
        .schema
        .as_ref()
        .and_then(|schema| schema.get("properties"))
        .is_some_and(|properties| properties.get(name).is_some())
}

/// Answers each agent by what its label and schema ask for.
#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<(String, Option<String>)>>,
}

impl Scripted {
    fn labels(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(label, _)| label.clone())
            .collect()
    }
}

#[async_trait]
impl AgentRunner for Scripted {
    async fn run(&self, request: AgentRequest) -> Result<AgentOutcome, String> {
        self.calls
            .lock()
            .unwrap()
            .push((request.label.clone(), request.phase.clone()));
        let label = request.label.as_str();
        let value = if label == "scope" {
            json!({"angles": [
                {"label": "mechanism", "query": "how it works"},
                {"label": "evidence", "query": "numbers"},
                {"label": "limits", "query": "criticism"}]})
        } else if let Some(angle) = label.strip_prefix("search:") {
            // Every angle returns the shared page, so it must be fetched once.
            json!({"results": [
                {"url": "https://www.example.com/shared/", "title": "shared", "relevance": "high"},
                {"url": format!("https://example.org/{angle}"), "title": angle, "relevance": "medium"}]})
        } else if label.starts_with("fetch:") {
            json!({"sourceQuality": "primary", "claims": [
                {"claim": format!("claim from {label}"), "quote": "a \"quoted\" line", "importance": "central"}]})
        } else if label.starts_with("verify:") && has_property(&request, "isReal") {
            // Findings about tests do not survive their skeptics.
            json!({"isReal": !label.contains("tests"), "evidence": "traced it"})
        } else if label.starts_with("verify:") {
            // The claim from the shared page is refuted by every voter.
            let refuted = label.contains("shared");
            json!({"refuted": refuted, "evidence": "looked it up", "confidence": "high"})
        } else if label == "plan" {
            json!({"units": [
                {"title": "core", "files": ["a.rs", "b.rs"], "change": "rename"},
                {"title": "cli", "files": ["c.rs"], "change": "rename"},
                {"title": "dup", "files": ["a.rs"], "change": "rename"}],
                "verify": "cargo check"})
        } else if label.starts_with("edit:") {
            json!({"changed": ["x.rs"], "summary": "renamed", "checked": true,
                "worktree": format!("/tmp/wt-{label}")})
        } else if label.starts_with("review:") && has_property(&request, "acceptable") {
            json!({"acceptable": !label.contains("cli"), "issues": ["left a call site"]})
        } else if label.starts_with("review:") {
            let dimension = label.trim_start_matches("review:");
            json!({"findings": [
                {"file": "src/a.rs", "line": 3, "title": format!("bug in {dimension}"),
                 "explanation": "because", "severity": if dimension == "security" { "high" } else { "low" }}]})
        } else if label == "synthesize" {
            json!({"summary": "done", "findings": [
                {"finding": "f", "confidence": "high", "sources": ["https://example.org/x"]}]})
        } else {
            return Ok(AgentOutcome::Failed(format!("unexpected agent {label}")));
        };
        Ok(AgentOutcome::Done(value))
    }
}

async fn run_bundled(name: &str, args: Value, runner: Arc<Scripted>) -> Result<Value, String> {
    let (_, text) = store::BUNDLED.iter().find(|(n, _)| *n == name).unwrap();
    let parsed = script::parse(text).unwrap();
    let engine = Arc::new(Engine::new(
        "wf_00000000-000".to_string(),
        runner,
        Limits {
            max_agents: 500,
            concurrency: 8,
        },
        Arc::new(Quiet),
        Cancel::new(),
    ));
    let resolver: vm::Resolver = Arc::new(|_| Err("no nested workflows".to_string()));
    vm::run(engine, resolver, &parsed.body, &args).await
}

#[tokio::test]
async fn deep_research_runs_scope_search_fetch_verify_synthesize() {
    let runner = Arc::new(Scripted::default());
    let out = run_bundled(
        "deep-research",
        json!("why is the sky blue"),
        runner.clone(),
    )
    .await
    .unwrap();

    assert_eq!(out["question"], "why is the sky blue");
    assert_eq!(out["summary"], "done");
    // The page every angle found is fetched once: 1 shared + 3 own.
    let labels = runner.labels();
    assert_eq!(
        labels.iter().filter(|l| l.starts_with("fetch:")).count(),
        4,
        "{labels:?}"
    );
    assert_eq!(out["stats"]["urlDupes"], 2);
    // Three voters per claim, and the shared page's claim is refuted by all.
    assert_eq!(
        labels.iter().filter(|l| l.starts_with("verify:")).count(),
        12
    );
    assert_eq!(out["stats"]["refuted"], 1);
    assert_eq!(out["stats"]["confirmed"], 3);
    assert_eq!(out["refuted"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn deep_research_without_a_question_says_how_to_pass_one() {
    let runner = Arc::new(Scripted::default());
    let out = run_bundled("deep-research", Value::Null, runner.clone())
        .await
        .unwrap();
    assert!(out["error"].as_str().unwrap().contains("args"));
    assert!(runner.labels().is_empty());
}

#[tokio::test]
async fn code_review_verifies_each_finding_and_drops_the_refuted() {
    let runner = Arc::new(Scripted::default());
    let out = run_bundled("code-review", json!("PR 12"), runner.clone())
        .await
        .unwrap();

    let labels = runner.labels();
    assert_eq!(
        labels.iter().filter(|l| l.starts_with("review:")).count(),
        5
    );
    // Two skeptics for each of the five findings.
    assert_eq!(
        labels.iter().filter(|l| l.starts_with("verify:")).count(),
        10
    );
    assert_eq!(out["target"], "PR 12");
    let confirmed = out["confirmed"].as_array().unwrap();
    assert_eq!(confirmed.len(), 4, "{out}");
    assert_eq!(confirmed[0]["dimension"], "security", "worst first");
    assert!(
        confirmed
            .iter()
            .all(|f| f["dimension"] != "tests" && f["votes"] == "2/2")
    );
    assert_eq!(out["stats"]["refuted"], 1);
}

#[tokio::test]
async fn batch_plans_units_edits_them_in_worktrees_and_reviews_each() {
    let runner = Arc::new(Scripted::default());
    let out = run_bundled("batch", json!("rename Foo to Bar"), runner.clone())
        .await
        .unwrap();

    // The unit that only repeats a claimed file is dropped before any edit.
    let labels = runner.labels();
    assert_eq!(
        labels.iter().filter(|l| l.starts_with("edit:")).count(),
        2,
        "{labels:?}"
    );
    assert_eq!(out["stats"]["planned"], 2);
    let accepted = out["accepted"].as_array().unwrap();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0]["unit"], "core");
    assert_eq!(accepted[0]["worktree"], "/tmp/wt-edit:core");
    let needs = out["needsWork"].as_array().unwrap();
    assert_eq!(needs[0]["unit"], "cli");
    assert_eq!(needs[0]["issues"][0], "left a call site");
}

#[tokio::test]
async fn batch_without_an_instruction_asks_for_one() {
    let runner = Arc::new(Scripted::default());
    let out = run_bundled("batch", json!({}), runner.clone())
        .await
        .unwrap();
    assert!(out["error"].as_str().unwrap().contains("instruction"));
    assert!(runner.labels().is_empty());
}
