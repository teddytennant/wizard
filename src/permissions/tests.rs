//! The cases marked "oracle" were run against Claude Code 2.1.314 with a
//! mock API and the same rules, and Wizard's answer matches what it did.

use std::path::Path;

use serde_json::json;

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
    }
}

fn run(policy: &Policy, command: &str) -> Decision {
    policy.check(
        "execute",
        ToolAccess::Execute,
        &json!({ "command": command }),
    )
}

fn is_deny(d: &Decision) -> bool {
    matches!(d, Decision::Deny(_))
}

fn is_ask(d: &Decision) -> bool {
    matches!(d, Decision::Ask(_))
}

#[test]
fn modes_parse_in_every_spelling() {
    for text in ["acceptEdits", "accept-edits", "accept_edits", "ACCEPTEDITS"] {
        assert_eq!(
            text.parse::<PermissionMode>(),
            Ok(PermissionMode::AcceptEdits)
        );
    }
    assert_eq!("dontAsk".parse(), Ok(PermissionMode::DontAsk));
    assert_eq!(
        "bypassPermissions".parse(),
        Ok(PermissionMode::BypassPermissions)
    );
    assert!("yolo".parse::<PermissionMode>().is_err());
    assert_eq!(PermissionMode::DontAsk.as_str(), "dontAsk");
}

#[test]
fn no_rules_and_default_mode_changes_nothing() {
    let p = Policy::new("/proj".into());
    assert!(p.is_inert());
    assert_eq!(run(&p, "rm -rf x"), Decision::Allow);
}

#[test]
fn deny_beats_allow() {
    // oracle: p_deny_beats_allow
    let p = policy(PermissionMode::Default, &["Bash"], &[], &["Bash(touch:*)"]);
    assert!(is_deny(&run(&p, "touch a && echo hi")));
}

#[test]
fn deny_beats_ask_and_ask_beats_allow() {
    // oracle: p_deny_beats_ask, p_ask_beats_allow
    let p = policy(PermissionMode::Default, &["Bash"], &["Bash(touch:*)"], &[]);
    assert!(is_ask(&run(&p, "touch a")));
    let p = policy(
        PermissionMode::Default,
        &[],
        &["Bash(touch:*)"],
        &["Bash(touch:*)"],
    );
    assert!(is_deny(&run(&p, "touch a")));
}

#[test]
fn ask_rules_still_ask_in_bypass_mode() {
    // oracle: p_bypass_ask
    let p = policy(
        PermissionMode::BypassPermissions,
        &[],
        &["Bash(touch:*)"],
        &[],
    );
    assert!(is_ask(&run(&p, "touch a")));
}

#[test]
fn deny_rules_still_deny_in_bypass_mode() {
    // oracle: p_bypass_deny
    let p = policy(
        PermissionMode::BypassPermissions,
        &[],
        &[],
        &["Bash(touch:*)"],
    );
    assert!(is_deny(&run(&p, "touch a")));
}

#[test]
fn a_deny_on_any_subcommand_denies_the_compound() {
    // oracle: p_compound_deny
    let p = policy(PermissionMode::BypassPermissions, &[], &[], &["Bash(rm:*)"]);
    assert!(is_deny(&run(&p, "touch zz.tmp && rm /nonexistent-x")));
}

#[test]
fn dont_ask_needs_an_allow_rule_for_each_subcommand() {
    // oracle: p_dontask_plain, p_dontask_allow, p_compound_allow
    let none = policy(PermissionMode::DontAsk, &[], &[], &[]);
    assert!(is_deny(&run(&none, "touch a")));
    let p = policy(PermissionMode::DontAsk, &["Bash(touch:*)"], &[], &[]);
    assert_eq!(run(&p, "touch a && touch b"), Decision::Allow);
    assert!(is_deny(&run(&p, "touch a && rm -rf x")));
    assert!(is_deny(&run(&p, "touch a && echo $(rm x)")));
    assert!(
        is_deny(&run(&p, "touch a &&")),
        "unparseable is never allowed"
    );
}

#[test]
fn dont_ask_lets_read_only_tools_through() {
    let p = policy(PermissionMode::DontAsk, &[], &[], &[]);
    assert_eq!(
        p.check("read_file", ToolAccess::ReadOnly, &json!({"path": "a"})),
        Decision::Allow
    );
}

#[test]
fn accept_edits_allows_edits_and_asks_for_the_rest() {
    // oracle: p_acceptEdits_write, p_acceptEdits_bash
    let p = policy(PermissionMode::AcceptEdits, &[], &[], &[]);
    assert_eq!(
        p.check(
            "write_file",
            ToolAccess::Edit,
            &json!({"path": "a", "content": ""})
        ),
        Decision::Allow
    );
    assert!(is_ask(&run(&p, "touch a")));
    let p = policy(PermissionMode::AcceptEdits, &["Bash(touch:*)"], &[], &[]);
    assert_eq!(run(&p, "touch a"), Decision::Allow);
}

#[test]
fn plan_mode_is_read_only() {
    let p = policy(PermissionMode::Plan, &["Bash"], &[], &[]);
    assert!(
        is_deny(&run(&p, "touch a")),
        "even an allow rule cannot write in plan"
    );
    assert_eq!(
        p.check(
            "search_files",
            ToolAccess::ReadOnly,
            &json!({"pattern": "x"})
        ),
        Decision::Allow
    );
}

#[test]
fn exact_rule_does_not_cover_a_longer_command() {
    // oracle: p_allow_exact_nomatch
    let p = policy(PermissionMode::DontAsk, &["Bash(touch)"], &[], &[]);
    assert!(is_deny(&run(&p, "touch a")));
}

#[test]
fn read_deny_covers_the_path_and_edits_to_it() {
    // oracle: p_read_deny_glob, p_read_deny_other
    let p = policy(
        PermissionMode::BypassPermissions,
        &[],
        &[],
        &["Read(./secrets/**)"],
    );
    assert!(is_deny(&p.check(
        "read_file",
        ToolAccess::ReadOnly,
        &json!({"path": "secrets/a.txt"})
    )));
    assert_eq!(
        p.check(
            "read_file",
            ToolAccess::ReadOnly,
            &json!({"path": "src/a.txt"})
        ),
        Decision::Allow
    );
    assert!(is_deny(&p.check(
        "write_file",
        ToolAccess::Edit,
        &json!({"path": "secrets/a.txt", "content": ""})
    )));
}

#[test]
fn edit_deny_covers_every_editor() {
    // oracle: p_tool_alias_edit_covers_write was not denied for a bare
    // `deny: ["Edit"]` in the real binary because it removes the tool; the
    // call is refused either way.
    let p = policy(PermissionMode::BypassPermissions, &[], &[], &["Edit"]);
    for tool in ["write_file", "edit_file", "apply_patch"] {
        assert!(
            is_deny(&p.check(tool, ToolAccess::Edit, &json!({"path": "a"}))),
            "{tool}"
        );
    }
}

#[test]
fn mcp_rules_apply_to_namespaced_tools() {
    let p = policy(
        PermissionMode::Default,
        &[],
        &[],
        &["mcp__github__create_issue"],
    );
    assert!(is_deny(&p.check(
        "github__create_issue",
        ToolAccess::Execute,
        &json!({})
    )));
    assert_eq!(
        p.check("github__list_issues", ToolAccess::Execute, &json!({})),
        Decision::Allow
    );
}

#[test]
fn deny_message_names_the_command() {
    let p = policy(PermissionMode::Default, &[], &[], &["Bash"]);
    match run(&p, "ls") {
        Decision::Deny(msg) => assert_eq!(
            msg,
            "Permission to use Bash with command ls has been denied."
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn summary_lists_rules_with_their_source() {
    let p = policy(
        PermissionMode::DontAsk,
        &["Bash(ls:*)"],
        &[],
        &["Read(.env)"],
    );
    let text = p.summary();
    assert!(text.contains("mode: dontAsk"));
    assert!(text.contains("deny:\n  Read(.env)  (test)"));
    assert!(text.contains("allow:\n  Bash(ls:*)  (test)"));
}
