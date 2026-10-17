// OWNER: input
//! Slash commands: the backend's list (from `Event::Commands`) merged with the ones openc
//! handles itself. A frontend command wins when both define a name, because it needs UI.

use agent_core::SlashCommand;

use crate::ui::composer::PItem;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Local {
    Model,
    Effort,
    Mode,
    Resume,
    New,
    Exit,
    Theme,
    Rail,
    Detail,
    Copy,
    Export,
    Diff,
    Search,
    Mouse,
    Redraw,
    Keys,
}

#[derive(Clone, Debug)]
pub struct Cmd {
    pub name: String,
    pub desc: String,
    pub hint: String,
    /// `skill`, `project`, `user`, `plugin`, or empty for built-ins.
    pub tag: String,
    pub local: Option<Local>,
    /// 0 for the command run most recently, `usize::MAX` for one never run here.
    pub recent: usize,
}

const FRONTEND: &[(&str, &str, &str, Local)] = &[
    ("model", "switch model", "", Local::Model),
    ("effort", "set reasoning effort", "", Local::Effort),
    ("mode", "switch permission mode", "", Local::Mode),
    ("resume", "pick a session to continue", "", Local::Resume),
    ("new", "start a new session", "", Local::New),
    ("clear", "start a new session", "", Local::New),
    ("theme", "switch theme", "", Local::Theme),
    ("rail", "toggle the right rail", "", Local::Rail),
    (
        "detail",
        "open every tool body and thought",
        "",
        Local::Detail,
    ),
    (
        "copy",
        "copy the last assistant message",
        "[n]",
        Local::Copy,
    ),
    (
        "export",
        "write the transcript as markdown",
        "[file]",
        Local::Export,
    ),
    (
        "diff",
        "diff viewer for files changed this session",
        "",
        Local::Diff,
    ),
    ("search", "search the transcript", "<text>", Local::Search),
    ("mouse", "toggle mouse capture", "[on|off]", Local::Mouse),
    ("redraw", "repaint the screen", "", Local::Redraw),
    ("keys", "show the key help", "", Local::Keys),
    ("exit", "quit openc", "", Local::Exit),
    ("quit", "quit openc", "", Local::Exit),
];

/// Names Claude Code ships itself, so they carry no source tag.
const BUILTIN: &[&str] = &[
    "compact",
    "context",
    "cost",
    "usage",
    "init",
    "review",
    "mcp",
    "agents",
    "hooks",
    "memory",
    "permissions",
    "pr-comments",
    "todos",
    "doctor",
    "status",
    "config",
    "add-dir",
    "output-style",
    "help",
    "login",
    "logout",
    "bug",
    "release-notes",
    "terminal-setup",
    "vim",
    "ide",
    "install-github-app",
    "security-review",
    "statusline",
];

/// Where a user-defined command or skill lives, by name: `project` for `<cwd>/.claude`,
/// `user` for `~/.claude`. The backend does not say, so this reads the directories.
pub fn scan_sources(cwd: &std::path::Path) -> std::collections::HashMap<String, &'static str> {
    let mut m = std::collections::HashMap::new();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let roots = [
        (home.map(|h| h.join(".claude")), "user"),
        (Some(cwd.join(".claude")), "project"),
    ];
    for (root, tag) in roots {
        let Some(root) = root else { continue };
        // commands/<name>.md and commands/<dir>/<name>.md (shown as `dir:name`).
        collect_md(&root.join("commands"), "", tag, &mut m, 0);
        // skills/<name>/SKILL.md
        if let Ok(rd) = std::fs::read_dir(root.join("skills")) {
            for e in rd.flatten() {
                if e.path().join("SKILL.md").is_file() {
                    m.insert(e.file_name().to_string_lossy().into_owned(), tag);
                }
            }
        }
    }
    m
}

fn collect_md(
    dir: &std::path::Path,
    prefix: &str,
    tag: &'static str,
    m: &mut std::collections::HashMap<String, &'static str>,
    depth: usize,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = e.path();
        if p.is_dir() && depth < 3 {
            collect_md(&p, &format!("{prefix}{name}:"), tag, m, depth + 1);
        } else if let Some(stem) = name.strip_suffix(".md") {
            m.insert(format!("{prefix}{stem}"), tag);
        }
    }
}

pub fn registry(backend: &[SlashCommand]) -> Vec<Cmd> {
    registry_with(backend, &Default::default(), &[])
}

/// `sources` from [`scan_sources`]; `recent` lists command names, oldest first.
pub fn registry_with(
    backend: &[SlashCommand],
    sources: &std::collections::HashMap<String, &'static str>,
    recent: &[String],
) -> Vec<Cmd> {
    let rank = |name: &str| {
        recent
            .iter()
            .rev()
            .position(|r| r == name)
            .unwrap_or(usize::MAX)
    };
    let mut out: Vec<Cmd> = FRONTEND
        .iter()
        .map(|(n, d, h, l)| Cmd {
            name: (*n).into(),
            desc: (*d).into(),
            hint: (*h).into(),
            tag: String::new(),
            local: Some(*l),
            recent: rank(n),
        })
        .collect();
    for c in backend {
        if out.iter().any(|o| o.name == c.name) {
            continue;
        }
        let tag = if let Some(t) = sources.get(&c.name) {
            t
        } else if c.name.contains(':') {
            "plugin"
        } else if BUILTIN.contains(&c.name.as_str()) {
            ""
        } else {
            "skill"
        };
        out.push(Cmd {
            name: c.name.clone(),
            desc: c.description.clone(),
            hint: c.input_hint.clone(),
            tag: tag.into(),
            local: None,
            recent: rank(&c.name),
        });
    }
    out
}

/// Popup rows for `/q`. Prefix matches first, then score; commands you ran lately win ties,
/// and with no query they come first.
pub fn match_items(cmds: &[Cmd], q: &str) -> Vec<PItem> {
    let mut scored: Vec<(i32, &Cmd, Vec<usize>)> = if q.is_empty() {
        cmds.iter().map(|c| (0, c, Vec::new())).collect()
    } else {
        cmds.iter()
            .filter_map(|c| tuikit::fuzzy::score(q, &c.name).map(|m| (m.score, c, m.indices)))
            .collect()
    };
    if q.is_empty() {
        scored.sort_by_key(|(_, c, _)| c.recent);
    } else {
        scored.sort_by(|a, b| {
            let pa = a.1.name.starts_with(q);
            let pb = b.1.name.starts_with(q);
            pb.cmp(&pa)
                .then(b.0.cmp(&a.0))
                .then(a.1.recent.cmp(&b.1.recent))
                .then(a.1.name.len().cmp(&b.1.name.len()))
        });
    }
    scored
        .into_iter()
        .take(60)
        .map(|(_, c, idx)| PItem {
            label: format!("/{}", c.name),
            hint: c.hint.clone(),
            desc: c.desc.clone(),
            tag: c.tag.clone(),
            insert: format!("/{}", c.name),
            indices: idx.into_iter().map(|i| i + 1).collect(),
        })
        .collect()
}

/// `/name args` -> `(name, args)`; `None` when the text is not a slash command.
pub fn parse(input: &str) -> Option<(&str, &str)> {
    let t = input.trim();
    let rest = t.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty() && !name.contains('/')).then_some((name, args.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_wins_and_backend_extras_are_tagged() {
        let be = vec![
            SlashCommand {
                name: "model".into(),
                description: "backend model".into(),
                input_hint: String::new(),
            },
            SlashCommand {
                name: "compact".into(),
                description: "summarize".into(),
                input_hint: String::new(),
            },
            SlashCommand {
                name: "gauntlet-loop".into(),
                description: "loop".into(),
                input_hint: String::new(),
            },
            SlashCommand {
                name: "wrangler:wrangler".into(),
                description: "x".into(),
                input_hint: String::new(),
            },
        ];
        let r = registry(&be);
        assert_eq!(r.iter().filter(|c| c.name == "model").count(), 1);
        assert_eq!(
            r.iter().find(|c| c.name == "model").unwrap().local,
            Some(Local::Model)
        );
        assert_eq!(r.iter().find(|c| c.name == "compact").unwrap().tag, "");
        assert_eq!(
            r.iter().find(|c| c.name == "gauntlet-loop").unwrap().tag,
            "skill"
        );
        assert_eq!(
            r.iter()
                .find(|c| c.name == "wrangler:wrangler")
                .unwrap()
                .tag,
            "plugin"
        );
    }

    #[test]
    fn user_and_project_commands_are_tagged_from_the_directories() {
        let root = std::env::temp_dir().join(format!("openc-cmds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let proj = root.join("proj");
        std::fs::create_dir_all(proj.join(".claude/commands/git")).unwrap();
        std::fs::write(proj.join(".claude/commands/deploy.md"), "x").unwrap();
        std::fs::write(proj.join(".claude/commands/git/sync.md"), "x").unwrap();
        std::fs::create_dir_all(proj.join(".claude/skills/pdf")).unwrap();
        std::fs::write(proj.join(".claude/skills/pdf/SKILL.md"), "x").unwrap();
        let m = scan_sources(&proj);
        assert_eq!(m.get("deploy"), Some(&"project"));
        assert_eq!(m.get("git:sync"), Some(&"project"));
        assert_eq!(m.get("pdf"), Some(&"project"));
        let be = |n: &str| SlashCommand {
            name: n.into(),
            description: "d".into(),
            input_hint: "<x>".into(),
        };
        let r = registry_with(&[be("deploy"), be("other")], &m, &[]);
        assert_eq!(
            r.iter().find(|c| c.name == "deploy").unwrap().tag,
            "project"
        );
        assert_eq!(r.iter().find(|c| c.name == "other").unwrap().tag, "skill");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recently_run_commands_come_first_and_win_ties() {
        let be = |n: &str| SlashCommand {
            name: n.into(),
            description: String::new(),
            input_hint: String::new(),
        };
        let recent = vec!["review".to_string(), "init".to_string()];
        let r = registry_with(
            &[be("init"), be("review"), be("rewrite")],
            &Default::default(),
            &recent,
        );
        let items = match_items(&r, "");
        assert_eq!(items[0].insert, "/init", "newest first");
        assert_eq!(items[1].insert, "/review");
        // Same prefix and score class: the one used lately sorts above the one never used.
        let items = match_items(&r, "re");
        let pos = |n: &str| items.iter().position(|i| i.insert == n).unwrap();
        assert!(pos("/review") < pos("/rewrite"));
    }

    #[test]
    fn matching_prefers_prefixes() {
        let r = registry(&[]);
        let items = match_items(&r, "re");
        assert!(items[0].insert.starts_with("/re"), "{:?}", items[0]);
    }

    #[test]
    fn parse_commands() {
        assert_eq!(parse("/model opus"), Some(("model", "opus")));
        assert_eq!(parse("  /exit "), Some(("exit", "")));
        assert_eq!(parse("/usr/bin/ls"), None);
        assert_eq!(parse("hello"), None);
    }
}
