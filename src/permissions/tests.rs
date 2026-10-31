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
        claude_defaults: false,
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
fn dont_ask_lets_read_only_commands_ride_along() {
    // oracle: p_dontask_plain with `echo`, p_dontask_allow with `touch && echo`
    let none = policy(PermissionMode::DontAsk, &[], &[], &[]);
    assert_eq!(run(&none, "echo hi"), Decision::Allow);
    assert_eq!(run(&none, "ls | wc -l"), Decision::Allow);
    assert!(is_deny(&run(&none, "echo hi > out")));
    let p = policy(PermissionMode::DontAsk, &["Bash(touch:*)"], &[], &[]);
    assert_eq!(
        run(&p, "touch zz.tmp && echo hi-from-bash"),
        Decision::Allow
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
    assert!(is_ask(&run(&p, "git push")));
    let p = policy(PermissionMode::AcceptEdits, &["Bash(git push:*)"], &[], &[]);
    assert_eq!(run(&p, "git push"), Decision::Allow);
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

#[test]
fn accept_edits_runs_file_commands_inside_the_project() {
    // oracle: acceptEdits ran touch, mkdir, mv, cp, rm and sed -i in the
    // project; touch /tmp/x, git commit and `cd d && touch q` asked.
    let p = policy(PermissionMode::AcceptEdits, &[], &[], &[]);
    for yes in [
        "touch a",
        "mkdir -p a/b && echo ok",
        "mkdir d && mv a d/",
        "cp a b; rm b",
        "sed -i s/a/b/ a",
        "rmdir d",
    ] {
        assert_eq!(run(&p, yes), Decision::Allow, "{yes}");
    }
    for no in [
        "touch /tmp/outside",
        "rm -rf ../elsewhere",
        "git commit -m x",
        "cd sub && touch q",
        "touch a && curl example.com",
        "touch $HOME/x",
        "touch a &&",
    ] {
        assert!(is_ask(&run(&p, no)), "{no}");
    }
}

#[test]
fn accept_edits_asks_for_edits_outside_the_project() {
    // oracle: Write /tmp/outside-claude-write.txt asked, sub/new.txt ran
    let p = policy(PermissionMode::AcceptEdits, &[], &[], &[]);
    let write = |path: &str| {
        p.check(
            "write_file",
            ToolAccess::Edit,
            &json!({"path": path, "content": ""}),
        )
    };
    assert_eq!(write("sub/new.txt"), Decision::Allow);
    assert_eq!(write("/proj/a.txt"), Decision::Allow);
    assert!(is_ask(&write("/tmp/outside.txt")));
    assert!(is_ask(&write("../outside.txt")));
}

fn named_default(allow: &[&str]) -> Policy {
    Policy {
        claude_defaults: true,
        ..policy(PermissionMode::Default, allow, &[], &[])
    }
}

#[test]
fn a_named_default_mode_asks_before_writes_commands_and_the_web() {
    // oracle: --permission-mode default refused touch, curl, Write, Edit and
    // WebFetch; ls and Read ran.
    let p = named_default(&[]);
    assert!(!p.is_inert());
    assert!(is_ask(&run(&p, "touch a")));
    assert!(is_ask(&run(&p, "curl example.com")));
    assert_eq!(run(&p, "ls -la | wc -l"), Decision::Allow);
    assert!(is_ask(&p.check(
        "write_file",
        ToolAccess::Edit,
        &json!({"path": "a", "content": ""})
    )));
    assert!(is_ask(&p.check(
        "web_fetch",
        ToolAccess::Execute,
        &json!({"url": "https://example.com"})
    )));
    assert!(is_ask(&p.check(
        "github__list_issues",
        ToolAccess::Execute,
        &json!({})
    )));
    assert_eq!(
        p.check("read_file", ToolAccess::ReadOnly, &json!({"path": "a"})),
        Decision::Allow
    );
}

#[test]
fn a_named_default_mode_lets_bookkeeping_tools_through() {
    let p = named_default(&[]);
    for tool in [
        "todo",
        "interview",
        "spawn_subagent",
        "task_kill",
        "exit_plan",
    ] {
        assert_eq!(
            p.check(tool, ToolAccess::Execute, &json!({})),
            Decision::Allow,
            "{tool}"
        );
    }
}

#[test]
fn a_named_default_mode_honours_allow_rules() {
    let p = named_default(&[
        "Bash(git commit:*)",
        "Edit(src/**)",
        "WebFetch(domain:example.com)",
    ]);
    assert_eq!(run(&p, "git commit -m x"), Decision::Allow);
    assert!(is_ask(&run(&p, "git push")));
    assert_eq!(
        p.check(
            "write_file",
            ToolAccess::Edit,
            &json!({"path": "src/a.rs", "content": ""})
        ),
        Decision::Allow
    );
    assert_eq!(
        p.check(
            "web_fetch",
            ToolAccess::Execute,
            &json!({"url": "https://example.com/x"})
        ),
        Decision::Allow
    );
}

#[test]
fn the_alias_names_are_checked_like_wizards() {
    // The model calls `Bash` and `Write`, not `execute` and `write_file`.
    let p = policy(
        PermissionMode::DontAsk,
        &["Bash(ls:*)"],
        &[],
        &["Bash(rm:*)"],
    );
    let bash = |command: &str| p.check("Bash", ToolAccess::Execute, &json!({ "command": command }));
    assert_eq!(bash("ls -la"), Decision::Allow);
    assert!(is_deny(&bash("rm x")));
    assert!(is_deny(&bash("touch x")));
    let p = policy(PermissionMode::Default, &[], &[], &["Edit(secrets/**)"]);
    assert!(is_deny(&p.check(
        "Write",
        ToolAccess::Edit,
        &json!({"file_path": "secrets/a", "content": ""})
    )));
}

#[test]
fn refusals_use_the_reference_words() {
    // oracle: the tool_result text of each refusal under --permission-mode default
    let p = named_default(&[]);
    assert_eq!(
        p.refusal("write_file", &json!({"path": "main.rs", "content": ""})),
        "Claude requested permissions to write to /proj/main.rs, but you haven't granted it yet."
    );
    assert_eq!(
        p.refusal("web_fetch", &json!({"url": "https://example.com"})),
        "Claude requested permissions to use WebFetch, but you haven't granted it yet."
    );
    assert_eq!(
        p.refusal("execute", &json!({"command": "curl example.com"})),
        "This command requires approval"
    );
    assert_eq!(
        p.refusal("execute", &json!({"command": "mkdir d && mv x d/"})),
        "This Bash command contains multiple operations. The following parts require approval: mkdir d, mv x d/"
    );
    assert_eq!(
        p.refusal("execute", &json!({"command": "touch x.txt"})),
        "touch in '/proj/x.txt' needs approval. The path is inside the working directories for this session ('/proj'), and Claude Code asks before a shell command creates, changes or removes files there."
    );
}

#[test]
fn the_offer_names_a_rule_that_covers_what_was_asked() {
    let cwd = Path::new("/proj");
    let bash = |command: &str| offer("execute", &json!({ "command": command }), cwd);
    assert_eq!(
        bash("git commit -m x").unwrap().rules,
        ["Bash(git commit:*)"]
    );
    assert_eq!(
        bash("cargo test --lib").unwrap().rules,
        ["Bash(cargo test:*)"]
    );
    assert_eq!(bash("curl -s example.com").unwrap().rules, ["Bash(curl:*)"]);
    assert_eq!(
        bash("mkdir d && git add . && ls").unwrap().rules,
        ["Bash(mkdir:*)", "Bash(git add:*)"]
    );
    assert!(bash("echo 'oops").is_none());
    let fetch = offer("web_fetch", &json!({"url": "https://example.com/a?b"}), cwd).unwrap();
    assert_eq!(fetch.rules, ["WebFetch(domain:example.com)"]);
    let edit = offer("edit_file", &json!({"path": "a"}), cwd).unwrap();
    assert!(edit.rules.is_empty());
    assert_eq!(edit.scope, Scope::Session);
    assert_eq!(
        offer("github__create_issue", &json!({}), cwd)
            .unwrap()
            .rules,
        ["mcp__github__create_issue"]
    );
}

#[test]
fn an_offered_rule_then_allows_the_command() {
    let mut p = named_default(&[]);
    assert!(is_ask(&run(&p, "git commit -m x")));
    let offered = offer("execute", &json!({"command": "git commit -m x"}), &p.cwd).unwrap();
    for rule in &offered.rules {
        p.allow_rule(rule, "prompt");
    }
    assert_eq!(run(&p, "git commit -m y"), Decision::Allow);
    assert!(is_ask(&run(&p, "git push")));
}

#[test]
fn rejection_wording_matches_the_reference() {
    assert!(rejection(None).starts_with("The user doesn't want to proceed with this tool use."));
    assert!(rejection(None).ends_with("wait for the user to tell you how to proceed."));
    assert!(rejection(Some("use rg")).ends_with("the user said:\nuse rg"));
}

#[test]
fn plan_mode_runs_read_only_command_lines() {
    // oracle: `ls` ran under --permission-mode plan; `touch zz` did not
    assert!(is_read_only_command("ls -la | wc -l"));
    assert!(is_read_only_command("pwd && ls"));
    assert!(!is_read_only_command("touch zz"));
    assert!(!is_read_only_command("ls > out"));
    assert!(!is_read_only_command("ls &&"));
    assert!(!is_read_only_command(""));
}

#[test]
fn the_summary_says_when_the_mode_asks() {
    assert!(
        named_default(&[])
            .summary()
            .contains("asks before file edits")
    );
    assert!(
        !policy(PermissionMode::Default, &[], &[], &[])
            .summary()
            .contains("asks before")
    );
}

fn claude_file_settings() -> crate::config::claude::ClaudeSettings {
    let mut settings = crate::config::claude::ClaudeSettings::default();
    let root = Path::new("/proj");
    settings.deny = vec![
        Rule::parse("Bash(rm:*)", root)
            .unwrap()
            .from_source("project settings (.claude/settings.json)"),
    ];
    settings.default_mode = Some(PermissionMode::DontAsk);
    settings
}

#[test]
fn nothing_is_checked_unless_permissions_were_asked_for() {
    let config = crate::config::Config::default();
    let policy = build(&config, &claude_file_settings(), Path::new("/proj"));
    assert!(policy.is_inert());
    assert_eq!(run(&policy, "rm -rf build"), Decision::Allow);
    assert_eq!(
        policy.check("write_file", ToolAccess::Edit, &json!({"path": "/proj/a"})),
        Decision::Allow
    );
}

#[test]
fn a_flag_a_wizard_config_or_a_wizard_settings_file_turns_them_on() {
    let root = Path::new("/proj");
    let mut config = crate::config::Config::default();
    config.run_permissions.deny = vec!["Bash(rm:*)".to_string()];
    let policy = build(
        &config,
        &crate::config::claude::ClaudeSettings::default(),
        root,
    );
    assert!(is_deny(&run(&policy, "rm -rf build")));

    let mut config = crate::config::Config::default();
    config.permissions.mode = Some(PermissionMode::DontAsk);
    let policy = build(
        &config,
        &crate::config::claude::ClaudeSettings::default(),
        root,
    );
    assert!(is_deny(&run(&policy, "cargo build")));

    let mut settings = claude_file_settings();
    settings.permissions_opt_in = true;
    let policy = build(&crate::config::Config::default(), &settings, root);
    assert!(is_deny(&run(&policy, "rm -rf build")));
}
