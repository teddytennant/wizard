//! `Workflow(<name>)` rules and the review prompt a workflow call raises.

use std::path::Path;

use serde_json::{Value, json};

use super::*;

fn policy(mode: PermissionMode, allow: &[&str], ask: &[&str], deny: &[&str]) -> Policy {
    let root = Path::new("/proj");
    let parse = |rules: &[&str]| {
        rules
            .iter()
            .map(|r| Rule::parse(r, root).unwrap().from_source("test"))
            .collect()
    };
    Policy {
        mode,
        allow: parse(allow),
        ask: parse(ask),
        deny: parse(deny),
        cwd: root.to_path_buf(),
        claude_defaults: mode == PermissionMode::Default,
        extra_dirs: Vec::new(),
    }
}

fn call(policy: &Policy, args: Value) -> Decision {
    policy.check("Workflow", ToolAccess::Execute, &args)
}

fn is_ask(d: &Decision) -> bool {
    matches!(d, Decision::Ask(_))
}

fn is_deny(d: &Decision) -> bool {
    matches!(d, Decision::Deny(_))
}

#[test]
fn a_workflow_is_reviewed_unless_a_rule_names_it() {
    let bare = policy(PermissionMode::Default, &[], &[], &[]);
    let named = json!({"name": "deep-research", "args": "q"});
    assert_eq!(
        call(&bare, named.clone()),
        Decision::Ask("Review dynamic workflow before running".to_string())
    );
    assert!(is_ask(&call(
        &bare,
        json!({"script": "export const meta = {}"})
    )));

    let by_name = policy(
        PermissionMode::Default,
        &["Workflow(deep-research)"],
        &[],
        &[],
    );
    assert_eq!(call(&by_name, named), Decision::Allow);
    assert!(is_ask(&call(&by_name, json!({"name": "batch"}))));
    // A rule on a name does not cover a file that merely carries that name.
    assert!(is_ask(&call(
        &by_name,
        json!({"name": "deep-research", "scriptPath": "/tmp/x.js"})
    )));

    let whole = policy(PermissionMode::Default, &["Workflow"], &[], &[]);
    assert_eq!(call(&whole, json!({"script": "x"})), Decision::Allow);
}

#[test]
fn workflow_deny_and_ask_rules_beat_an_allow_rule() {
    let denied = policy(
        PermissionMode::Default,
        &["Workflow"],
        &[],
        &["Workflow(batch)"],
    );
    assert_eq!(
        call(&denied, json!({"name": "batch"})),
        Decision::Deny("Workflow batch blocked by permission rules".to_string())
    );
    assert_eq!(
        call(&denied, json!({"name": "deep-research"})),
        Decision::Allow
    );

    let asked = policy(
        PermissionMode::Default,
        &["Workflow"],
        &["Workflow(batch)"],
        &[],
    );
    assert!(is_ask(&call(&asked, json!({"name": "batch"}))));
}

#[test]
fn a_workflow_runs_in_bypass_and_is_refused_in_dont_ask_and_plan() {
    let bypass = policy(PermissionMode::BypassPermissions, &[], &[], &[]);
    assert_eq!(call(&bypass, json!({"name": "x"})), Decision::Allow);
    let dont_ask = policy(PermissionMode::DontAsk, &[], &[], &[]);
    assert!(is_deny(&call(&dont_ask, json!({"name": "x"}))));
    let allowed = policy(PermissionMode::DontAsk, &["Workflow(x)"], &[], &[]);
    assert_eq!(call(&allowed, json!({"name": "x"})), Decision::Allow);
    let plan = policy(PermissionMode::Plan, &["Workflow"], &[], &[]);
    assert!(is_deny(&call(&plan, json!({"name": "x"}))));
}

#[test]
fn the_offer_for_a_workflow_is_a_rule_on_its_name() {
    let cwd = Path::new("/proj");
    let named = offer("Workflow", &json!({"name": "deep-research"}), cwd).unwrap();
    assert_eq!(named.rules, ["Workflow(deep-research)"]);
    assert!(named.label.contains("workflow deep-research"));
    assert!(offer("Workflow", &json!({"script": "x"}), cwd).is_none());
    assert!(
        offer(
            "Workflow",
            &json!({"name": "a", "scriptPath": "/x.js"}),
            cwd
        )
        .is_none()
    );
}
