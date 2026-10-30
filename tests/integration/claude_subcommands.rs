//! `wizard` spelled like `claude`: the subcommands Claude Code has, run against
//! the built binary with `HOME` (and so `~/.wizard` and `~/.claude.json`) in a
//! throwaway directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wizard-claude-sub-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).expect("create temp dir");
        Self(dir)
    }

    fn project(&self) -> PathBuf {
        self.0.join("project")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wizard(home: &Home, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wizard"))
        .args(args)
        .env("HOME", &home.0)
        .env("WIZARD_HOME", home.0.join(".wizard"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("WIZARD_TRUST_PROJECT")
        .current_dir(home.project())
        .output()
        .expect("binary runs")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn mcp_add_get_and_remove_round_trip_through_every_scope() {
    let home = Home::new("mcp");
    for (scope, name) in [("local", "l"), ("user", "u"), ("project", "p")] {
        let out = wizard(
            &home,
            &[
                "mcp", "add", "-s", scope, name, "-e", "K=v", "--", "echo", "hi",
            ],
        );
        assert!(out.status.success(), "{scope}: {}", text(&out));
        assert!(
            text(&out).contains(&format!("to {scope} config")),
            "{}",
            text(&out)
        );
    }
    assert!(read(&home.project().join(".mcp.json")).contains("\"command\": \"echo\""));
    assert!(read(&home.0.join(".wizard/mcp.toml")).contains("name = \"u\""));
    assert!(read(&home.0.join(".wizard/local-mcp.json")).contains("\"l\""));

    let dup = wizard(&home, &["mcp", "add", "l", "echo"]);
    assert_eq!(dup.status.code(), Some(1));
    assert!(text(&dup).contains("already exists in local config"));

    let get = wizard(&home, &["mcp", "get", "u"]);
    assert!(get.status.success());
    let shown = text(&get);
    assert!(shown.contains("Scope: User config"), "{shown}");
    assert!(shown.contains("K=v"), "{shown}");

    for name in ["l", "u", "p"] {
        let gone = wizard(&home, &["mcp", "remove", name]);
        assert!(gone.status.success(), "{}", text(&gone));
    }
    let missing = wizard(&home, &["mcp", "remove", "l"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing).contains("No MCP server named \"l\""));
}

#[test]
fn servers_in_claude_json_are_listed_without_an_import() {
    let home = Home::new("claudejson");
    let project = std::fs::canonicalize(home.project()).unwrap();
    std::fs::write(
        home.0.join(".claude.json"),
        format!(
            r#"{{"mcpServers":{{"from-user":{{"command":"echo"}}}},
                "projects":{{"{}":{{"mcpServers":{{"from-local":{{"command":"echo"}}}}}}}}}}"#,
            project.display()
        ),
    )
    .unwrap();
    let out = wizard(&home, &["mcp", "get", "from-local"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("Claude Code local config"),
        "{}",
        text(&out)
    );
    let out = wizard(&home, &["mcp", "get", "from-user"]);
    assert!(
        text(&out).contains("Claude Code user config"),
        "{}",
        text(&out)
    );
}

#[test]
fn auth_status_is_json_and_exits_one_when_signed_out() {
    let home = Home::new("auth");
    let out = wizard(&home, &["auth", "status"]);
    assert_eq!(out.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    assert_eq!(json["loggedIn"], false);
    let text_out = wizard(&home, &["auth", "status", "--text"]);
    assert!(text(&text_out).contains("Not logged in"));
}

#[test]
fn plugin_init_then_validate_and_marketplace_list_work() {
    let home = Home::new("plugin");
    let init = wizard(&home, &["plugin", "init", "demo"]);
    assert!(init.status.success(), "{}", text(&init));
    let valid = wizard(&home, &["plugin", "validate", "demo"]);
    assert!(valid.status.success(), "{}", text(&valid));
    assert!(text(&valid).contains("demo: valid"));
    let bad = wizard(&home, &["plugin", "validate", "nowhere"]);
    assert_eq!(bad.status.code(), Some(1));
    let list = wizard(&home, &["plugin", "list"]);
    assert!(
        text(&list).contains("No plugins installed"),
        "{}",
        text(&list)
    );
}

#[test]
fn subcommands_without_a_counterpart_explain_and_exit_one() {
    let home = Home::new("stubs");
    for (args, word) in [
        (vec!["attach", "x"], "wizard agents"),
        (vec!["ultrareview", "12", "--json"], "/review"),
        (vec!["auto-mode", "defaults"], "permission classifier"),
        (vec!["import", "codex"], "no codex importer"),
    ] {
        let out = wizard(&home, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", text(&out));
        assert!(text(&out).contains(word), "{args:?}: {}", text(&out));
    }
}

#[test]
fn agents_json_prints_an_array_and_dash_v_is_the_version() {
    let home = Home::new("agents");
    let out = wizard(&home, &["agents", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    assert!(json.is_array());
    let version = wizard(&home, &["-v"]);
    assert!(text(&version).starts_with("wizard "), "{}", text(&version));
}

#[test]
fn purge_dry_run_lists_a_projects_conversations_and_deletes_nothing() {
    let home = Home::new("purge");
    let sessions = home.0.join(".wizard/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let project = std::fs::canonicalize(home.project()).unwrap();
    let mine = sessions.join("2026-01-01T00-00-00.jsonl");
    let other = sessions.join("2026-01-02T00-00-00.jsonl");
    std::fs::write(
        &mine,
        format!(
            "{{\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"{}\",\"version\":1}}\n",
            project.display()
        ),
    )
    .unwrap();
    std::fs::write(
        &other,
        "{\"timestamp\":\"2026-01-02T00:00:00Z\",\"cwd\":\"/elsewhere\",\"version\":1}\n",
    )
    .unwrap();
    let dry = wizard(&home, &["purge", "--dry-run"]);
    assert!(dry.status.success(), "{}", text(&dry));
    assert!(text(&dry).contains("2026-01-01T00-00-00"));
    assert!(!text(&dry).contains("2026-01-02T00-00-00"));
    assert!(mine.exists());

    let real = wizard(&home, &["purge", "-y"]);
    assert!(real.status.success(), "{}", text(&real));
    assert!(!mine.exists() && other.exists());
}
