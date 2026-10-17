// OWNER: dialogs
//! What the status, MCP and skills dialogs can learn from `~/.wizard` without asking the
//! backend: wizard speaks no ACP method for these, so the files are the source of truth.

use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct McpServer {
    pub name: String,
    pub transport: String,
    /// The command line or URL it is started from.
    pub target: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    let v = v.split(" #").next().unwrap_or(v).trim();
    v.trim_matches('"').trim_matches('\'').to_string()
}

/// `[[server]]` blocks of `mcp.toml`: `name`, `transport`, `command` + `args` or `url`, and an
/// optional `enabled = false`. Anything else in a block is ignored.
pub fn parse_mcp(text: &str) -> Vec<McpServer> {
    let mut out: Vec<McpServer> = Vec::new();
    let mut cur: Option<(McpServer, String, String)> = None; // server, command, args
    let flush = |cur: &mut Option<(McpServer, String, String)>, out: &mut Vec<McpServer>| {
        if let Some((mut s, cmd, args)) = cur.take() {
            if s.target.is_empty() {
                s.target = format!("{cmd} {args}").trim().to_string();
            }
            if !s.name.is_empty() {
                out.push(s);
            }
        }
    };
    let mut in_server_table = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('#') || l.is_empty() {
            continue;
        }
        if l == "[[server]]" {
            flush(&mut cur, &mut out);
            cur = Some((
                McpServer {
                    name: String::new(),
                    transport: "stdio".into(),
                    target: String::new(),
                    enabled: true,
                },
                String::new(),
                String::new(),
            ));
            in_server_table = true;
            continue;
        }
        if l.starts_with('[') {
            // `[server.env]` and friends: keys below belong to a sub-table.
            in_server_table = false;
            continue;
        }
        let (Some((s, cmd, args)), true) = (cur.as_mut(), in_server_table) else {
            continue;
        };
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        match k.trim() {
            "name" => s.name = unquote(v),
            "transport" => s.transport = unquote(v),
            "command" => *cmd = unquote(v),
            "url" => s.target = redact_url(&unquote(v)),
            "enabled" => s.enabled = unquote(v) != "false",
            "args" => {
                let inner = v.trim().trim_start_matches('[').trim_end_matches(']');
                let parts: Vec<String> = inner
                    .split(',')
                    .map(unquote)
                    .filter(|a| !a.is_empty())
                    .collect();
                *args = redact_args(&parts).join(" ");
            }
            _ => {}
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// A server's address as the dialog shows it: no credentials before the host and nothing after
/// the path, since tokens ride in `user:pass@` and in `?key=...`.
fn redact_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").map_or(("", url), |(s, r)| (s, r));
    let rest_start = if scheme.is_empty() {
        0
    } else {
        scheme.len() + 3
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host_at = rest[..authority_end].rfind('@').map_or(0, |i| i + 1);
    let cut = rest.find(['?', '#']).unwrap_or(rest.len());
    let mut out = String::new();
    out.push_str(&url[..rest_start]);
    out.push_str(&rest[host_at..cut]);
    if cut < rest.len() {
        out.push_str("?…");
    }
    out
}

/// Words in a flag or key that say its value is a credential.
const SECRET_WORDS: [&str; 7] = [
    "token", "key", "secret", "password", "passwd", "auth", "bearer",
];

fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|w| n.contains(w))
}

/// Launch arguments with credential values replaced by `…`: the value of `--token x`, of
/// `--api-key=x` and of `KEY=x`, and anything shaped like a well known token.
fn redact_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for a in args {
        if std::mem::take(&mut mask_next) {
            out.push("…".into());
            continue;
        }
        let known = ["sk-", "ghp_", "github_pat_", "xoxb-", "xoxp-", "AKIA"]
            .iter()
            .any(|p| a.starts_with(p));
        if known {
            out.push("…".into());
        } else if let Some((k, _)) = a.split_once('=').filter(|(k, _)| is_secret_name(k)) {
            out.push(format!("{k}=…"));
        } else {
            mask_next = a.starts_with('-') && !a.contains('=') && is_secret_name(a);
            out.push(a.clone());
        }
    }
    out
}

pub fn load_mcp(dir: &Path) -> Vec<McpServer> {
    std::fs::read_to_string(dir.join("mcp.toml"))
        .map(|t| parse_mcp(&t))
        .unwrap_or_default()
}

/// Front matter of a `SKILL.md`: `name` and `description`, where the description may be a plain
/// value or a `>`/`|` block.
pub fn parse_skill(text: &str, fallback_name: &str) -> Option<Skill> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut name = String::new();
    let mut desc = String::new();
    let mut in_desc = false;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if in_desc && (line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty()) {
            desc.push(' ');
            desc.push_str(line.trim());
            continue;
        }
        in_desc = false;
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "name" => name = unquote(v),
                "description" => {
                    let v = v.trim();
                    if matches!(v, ">" | "|" | ">-" | "|-" | "") {
                        in_desc = true;
                    } else {
                        desc = unquote(v);
                    }
                }
                _ => {}
            }
        }
    }
    let name = if name.is_empty() {
        fallback_name.to_string()
    } else {
        name
    };
    Some(Skill {
        name,
        description: desc.split_whitespace().collect::<Vec<_>>().join(" "),
    })
}

/// Directory names under `plugins/`, sorted.
pub fn load_plugins(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir.join("plugins"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|n| n.to_lowercase());
    v
}

/// Every `skills/*/SKILL.md` under `dir`, by name.
pub fn load_skills(dir: &Path) -> Result<Vec<Skill>, String> {
    let root = dir.join("skills");
    let rd = match std::fs::read_dir(&root) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut v = Vec::new();
    for e in rd.flatten() {
        let p = e.path().join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let dirname = e.file_name().to_string_lossy().to_string();
        if let Some(s) = parse_skill(&text, &dirname) {
            v.push(s);
        }
    }
    v.sort_by_key(|s| s.name.to_lowercase());
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_addresses_and_arguments_hide_credentials() {
        let t = "[[server]]\nname = \"x\"\ntransport = \"http\"\nurl = \"https://me:hunter2@mcp.example.com/v1?key=SECRET#frag\"\n\n[[server]]\nname = \"y\"\ncommand = \"npx\"\nargs = [\"-y\", \"srv\", \"--token\", \"abc123\", \"--api-key=zzz\", \"API_KEY=qq\", \"sk-live-1\", \"--port\", \"80\"]\n";
        let v = parse_mcp(t);
        assert_eq!(v[0].target, "https://mcp.example.com/v1?…");
        let shown = format!("{v:?}");
        for s in ["hunter2", "SECRET", "abc123", "zzz", "qq", "sk-live-1"] {
            assert!(!shown.contains(s), "{s} is on screen: {shown}");
        }
        assert!(
            v[1].target.contains("--token …") && v[1].target.contains("--port 80"),
            "{}",
            v[1].target
        );
        assert_eq!(
            redact_url("http://localhost:8080/sse"),
            "http://localhost:8080/sse"
        );
        assert_eq!(redact_url("stdio"), "stdio");
    }

    #[test]
    fn parses_stdio_and_http_servers() {
        let t = r#"
# comment
[[server]]
name = "playwright"
transport = "stdio"
command = "npx"
args = ["-y", "@playwright/mcp@latest"]

[server.env]
KEY = "x"

[[server]]
name = "docs"
transport = "http"
url = "https://example.com/mcp"
enabled = false
"#;
        let s = parse_mcp(t);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "playwright");
        assert_eq!(s[0].target, "npx -y @playwright/mcp@latest");
        assert!(s[0].enabled);
        assert_eq!(s[1].transport, "http");
        assert_eq!(s[1].target, "https://example.com/mcp");
        assert!(!s[1].enabled);
    }

    #[test]
    fn skill_front_matter_with_block_description() {
        let t = "---\nname: gauntlet\ndescription: >\n  Run a builder and critic\n  against a bar.\n---\n# body\n";
        let s = parse_skill(t, "dir").unwrap();
        assert_eq!(s.name, "gauntlet");
        assert_eq!(s.description, "Run a builder and critic against a bar.");
        assert!(parse_skill("no front matter", "x").is_none());
        let s = parse_skill("---\ndescription: \"One line\"\n---\n", "fallback").unwrap();
        assert_eq!(
            (s.name.as_str(), s.description.as_str()),
            ("fallback", "One line")
        );
    }
}
