//! What wizard keeps on disk that Codex's `/mcp`, `/hooks` and `/debug-config` show: the MCP
//! servers in `~/.wizard/mcp.toml`, the lifecycle hooks in `hooks.toml`, and the config files in
//! play. ACP reports none of it, so these read the same files wizard does. The files are flat
//! enough (`[[server]]` and `[[hooks]]` tables of `key = value`) for a small line reader.

use std::path::{Path, PathBuf};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub pairs: Vec<(String, String)>,
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// The `[[name]]` tables of a file, each as its key/value pairs. Values lose their quotes;
/// arrays and inline tables stay as written.
pub fn array_tables(text: &str, name: &str) -> Vec<Table> {
    let header = format!("[[{name}]]");
    let mut out: Vec<Table> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            open = t == header;
            if open {
                out.push(Table::default());
            }
            continue;
        }
        if !open || t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(v);
            if let Some(cur) = out.last_mut() {
                cur.pairs.push((k.trim().to_string(), v.to_string()));
            }
        }
    }
    out
}

/// `~/.wizard/mcp.toml` servers.
pub fn mcp_servers(home: &str) -> Vec<Table> {
    let p = PathBuf::from(home).join(".wizard/mcp.toml");
    std::fs::read_to_string(p)
        .map(|t| array_tables(&t, "server"))
        .unwrap_or_default()
}

fn dim() -> Style {
    Style::default().dim()
}

/// The `/mcp` output after the echoed command: `🔌  MCP Tools` then each server.
pub fn mcp_lines(servers: &[Table], verbose: bool) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(vec![
        Span::raw("🔌  "),
        Span::styled("MCP Tools", Style::default().bold()),
    ])];
    out.push(Line::default());
    if servers.is_empty() {
        out.push(Line::from("  • No MCP servers configured."));
        out.push(Line::from(
            "    • Declare them in ~/.wizard/mcp.toml as [[server]] tables.",
        ));
        return out;
    }
    for s in servers {
        out.push(Line::from(format!(
            "  • {}",
            s.get("name").unwrap_or("(unnamed)")
        )));
        out.push(Line::from(format!(
            "    • Transport: {}",
            s.get("transport").unwrap_or("stdio")
        )));
        if verbose {
            for key in ["command", "args", "url"] {
                if let Some(v) = s.get(key) {
                    out.push(Line::from(format!("    • {}: {v}", capitalise(key))));
                }
            }
        }
        out.push(Line::from(
            "    • Tools: not listed over ACP; /doctor checks the connection",
        ));
    }
    out
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// Codex's event names, wizard's ids, and the one-line descriptions of the events screen.
pub const HOOK_EVENTS: [(&str, &str, &str); 11] = [
    ("PreToolUse", "pre_tool_use", "Before a tool executes"),
    (
        "PermissionRequest",
        "permission_request",
        "When permission is requested",
    ),
    ("PostToolUse", "post_tool_use", "After a tool executes"),
    ("PreCompact", "pre_compact", "Before context compaction"),
    ("PostCompact", "post_compact", "After context compaction"),
    ("SessionStart", "session_start", "When a new session starts"),
    ("SessionEnd", "session_end", "Right before a session ends"),
    (
        "UserPromptSubmit",
        "user_prompt_submit",
        "When the user submits a prompt",
    ),
    (
        "SubagentStart",
        "subagent_start",
        "When a subagent is created",
    ),
    (
        "SubagentStop",
        "subagent_stop",
        "Right before a subagent ends its turn",
    ),
    ("Stop", "stop", "Right before Wizard ends its turn"),
];

/// Hooks from `~/.wizard/hooks.toml` and, in the project, `.wizard/hooks.toml`. The project file
/// only counts as installed: wizard runs it once the project is trusted, which a file cannot say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookCounts {
    /// Per event id: `(installed, active)`.
    pub per_event: Vec<(String, usize, usize)>,
}

pub fn hook_tables(home: Option<&str>, cwd: &Path) -> (Vec<Table>, Vec<Table>) {
    let read = |p: PathBuf| {
        std::fs::read_to_string(p)
            .map(|t| array_tables(&t, "hooks"))
            .unwrap_or_default()
    };
    let global = home
        .map(|h| read(PathBuf::from(h).join(".wizard/hooks.toml")))
        .unwrap_or_default();
    let project = read(cwd.join(".wizard/hooks.toml"));
    (global, project)
}

pub fn hook_counts(global: &[Table], project: &[Table]) -> Vec<(&'static str, usize, usize)> {
    HOOK_EVENTS
        .iter()
        .map(|(name, id, _)| {
            let g = global.iter().filter(|t| t.get("event") == Some(id)).count();
            let p = project
                .iter()
                .filter(|t| t.get("event") == Some(id))
                .count();
            (*name, g + p, g)
        })
        .collect()
}

/// The `/hooks` events screen as history lines (the header, the table, no hint).
pub fn hooks_lines(global: &[Table], project: &[Table]) -> Vec<Line<'static>> {
    let counts = hook_counts(global, project);
    let mut out = vec![
        Line::from(Span::styled("Hooks", Style::default().bold())),
        Line::from(Span::styled(
            "Lifecycle hooks from config and enabled plugins.",
            dim(),
        )),
        Line::default(),
        Line::from(format!(
            "  {:<22}{:<12}{:<12}{}",
            "Event", "Installed", "Active", "Description"
        )),
    ];
    for ((name, installed, active), (_, _, desc)) in counts.iter().zip(HOOK_EVENTS.iter()) {
        out.push(Line::from(vec![
            Span::raw(format!("  {name:<22}")),
            Span::styled(format!("{installed:<12}{active:<12}{desc}"), dim()),
        ]));
    }
    out
}

/// One hook as the detail rows show it.
pub fn hook_detail(t: &Table, source: &str) -> Vec<String> {
    let mut rows = vec![format!("Event: {}", t.get("event").unwrap_or("?"))];
    if let Some(m) = t.get("matcher") {
        rows.push(format!("Matcher: {m}"));
    }
    rows.push(format!("Source: {source}"));
    rows.push(format!("Command: {}", t.get("command").unwrap_or("?")));
    if let Some(s) = t.get("timeout_secs") {
        rows.push(format!("Timeout: {s}s"));
    }
    rows
}

/// `/debug-config`: the config files wizard reads, lowest precedence first.
pub fn debug_config_lines(home: Option<&str>, cwd: &Path) -> Vec<Line<'static>> {
    let user = home.map(|h| PathBuf::from(h).join(".wizard/config.toml"));
    let project = cwd.join(".wizard/config.toml");
    let state = |p: &Path| if p.is_file() { "enabled" } else { "not found" };
    let show = |p: &Path| match home {
        Some(h) if p.starts_with(h) => format!("~{}", &p.display().to_string()[h.len()..]),
        _ => p.display().to_string(),
    };
    let mut out = vec![Line::from(Span::styled(
        "Config layer stack (lowest precedence first):",
        Style::default().bold(),
    ))];
    let mut n = 1;
    if let Some(u) = &user {
        out.push(Line::from(format!(
            "  {n}. user ({}) ({})",
            show(u),
            state(u)
        )));
        n += 1;
    }
    out.push(Line::from(format!(
        "  {n}. project ({}) ({})",
        show(&project),
        state(&project)
    )));
    out.push(Line::default());
    out.push(Line::from(Span::styled(
        "Requirements:",
        Style::default().bold(),
    )));
    out.push(Line::from(Span::styled("  <none>", dim())));
    out.push(Line::default());
    out.push(Line::from(Span::styled(
        "[codexw]:",
        Style::default().bold(),
    )));
    let cfg = home.map(crate::config::path);
    let theme = cfg
        .as_ref()
        .and_then(|p| crate::config::get(p, "theme"))
        .unwrap_or_else(|| "<unset>".into());
    out.push(Line::from(format!("  - theme = {theme}")));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    const MCP: &str = "# comment\n[[server]]\nname = \"playwright\"\ntransport = \"stdio\"\ncommand = \"npx\"\nargs = [\"-y\", \"@playwright/mcp@latest\"]\n\n[[server]]\nname = \"docs\"\nurl = \"https://x.test/mcp\"\ntransport = \"http\"\n";

    #[test]
    fn tables_read_name_and_values() {
        let t = array_tables(MCP, "server");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].get("name"), Some("playwright"));
        assert_eq!(
            t[0].get("args"),
            Some("[\"-y\", \"@playwright/mcp@latest\"]")
        );
        assert_eq!(t[1].get("url"), Some("https://x.test/mcp"));
        assert!(array_tables(MCP, "hooks").is_empty());
    }

    #[test]
    fn mcp_output_says_what_it_cannot_list() {
        let rows = text(&mcp_lines(&array_tables(MCP, "server"), false));
        assert_eq!(rows[0], "🔌  MCP Tools");
        assert_eq!(rows[2], "  • playwright");
        assert_eq!(rows[3], "    • Transport: stdio");
        assert!(rows[4].starts_with("    • Tools: not listed over ACP"));
        let v = text(&mcp_lines(&array_tables(MCP, "server"), true));
        assert!(v.contains(&"    • Command: npx".to_string()));
        let none = text(&mcp_lines(&[], false));
        assert_eq!(none[2], "  • No MCP servers configured.");
    }

    #[test]
    fn hooks_screen_counts_installed_and_active_per_event() {
        let g = array_tables(
            "[[hooks]]\nevent = \"pre_tool_use\"\ncommand = \"a\"\n[[hooks]]\nevent = \"stop\"\ncommand = \"b\"\n",
            "hooks",
        );
        let p = array_tables(
            "[[hooks]]\nevent = \"pre_tool_use\"\ncommand = \"c\"\n",
            "hooks",
        );
        let rows = text(&hooks_lines(&g, &p));
        assert_eq!(
            rows[3],
            "  Event                 Installed   Active      Description"
        );
        assert_eq!(
            rows[4],
            "  PreToolUse            2           1           Before a tool executes"
        );
        assert_eq!(
            rows[14],
            "  Stop                  1           1           Right before Wizard ends its turn"
        );
        assert_eq!(rows.len(), 15);
    }

    #[test]
    fn empty_hooks_screen_matches_the_capture_rows() {
        // reference/codex/120x36/popups-07-hooks
        let rows = text(&hooks_lines(&[], &[]));
        assert_eq!(rows[0], "Hooks");
        assert_eq!(
            rows[4],
            "  PreToolUse            0           0           Before a tool executes"
        );
        assert_eq!(
            rows[5],
            "  PermissionRequest     0           0           When permission is requested"
        );
    }

    #[test]
    fn debug_config_lists_layers() {
        let rows = text(&debug_config_lines(
            Some("/home/u"),
            Path::new("/work/proj"),
        ));
        assert_eq!(rows[0], "Config layer stack (lowest precedence first):");
        assert!(rows[1].starts_with("  1. user (~/.wizard/config.toml) ("));
        assert!(rows[2].starts_with("  2. project (/work/proj/.wizard/config.toml) ("));
        assert!(rows.contains(&"Requirements:".to_string()));
    }
}
