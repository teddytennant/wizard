//! One permission rule: `Tool`, `Tool(specifier)`, `mcp__server__tool`.

use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;

use super::shell;
use crate::hooks::toolmap::{self, ToolClass};

/// A parsed rule. `raw` is the text as written, for messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub raw: String,
    /// Tool name as written in the rule (`Bash`, `Read`, `mcp__github__*`).
    pub tool: String,
    /// The text inside the parentheses, if any (`*` already folded away).
    pub spec: Option<String>,
    /// Directory a `/path` pattern is anchored to; the project root for
    /// project, local and command-line rules.
    pub root: PathBuf,
    /// Where the rule came from, for `/permissions` ("user settings",
    /// "--allowedTools", ...).
    pub source: String,
}

impl Rule {
    /// Parse `Tool` or `Tool(specifier)`. Text after the closing parenthesis
    /// is an error, and so is an empty tool name.
    pub fn parse(raw: &str, root: &Path) -> Result<Rule, String> {
        let raw = raw.trim();
        let (tool, spec) = match raw.find('(') {
            None => (raw, None),
            Some(open) => {
                if !raw.ends_with(')') {
                    return Err(format!("`{raw}`: text after the closing parenthesis"));
                }
                let inner = &raw[open + 1..raw.len() - 1];
                (raw[..open].trim(), Some(inner.to_string()))
            }
        };
        if tool.is_empty() {
            return Err(format!("`{raw}`: missing tool name"));
        }
        let spec = spec.filter(|s| !s.trim().is_empty() && s.trim() != "*");
        Ok(Rule {
            raw: raw.to_string(),
            tool: tool.to_string(),
            spec,
            root: root.to_path_buf(),
            source: String::new(),
        })
    }

    /// The same rule, labelled with where it came from.
    pub fn from_source(mut self, source: &str) -> Rule {
        self.source = source.to_string();
        self
    }

    /// True when the rule names this tool, ignoring any specifier.
    pub fn names(&self, tool: &str) -> bool {
        let claude = toolmap::claude_name(tool);
        if self.tool == claude || self.tool == tool {
            return true;
        }
        match (self.tool.as_str(), toolmap::class(tool)) {
            ("Edit", ToolClass::Edit) | ("Read", ToolClass::Read) => return true,
            _ => {}
        }
        if let Some(rest) = self.tool.strip_prefix("mcp__")
            && !rest.contains('*')
            && !rest.contains("__")
        {
            // `mcp__server` covers every tool of that server.
            return claude.starts_with(&format!("{}__", self.tool));
        }
        if self.tool.contains('*') {
            return GlobBuilder::new(&self.tool)
                .literal_separator(false)
                .build()
                .map(|glob| {
                    let glob = glob.compile_matcher();
                    glob.is_match(&claude) || glob.is_match(tool)
                })
                .unwrap_or(false);
        }
        false
    }

    /// True when the rule has no specifier, so it covers every call to the
    /// tool.
    pub fn is_whole_tool(&self) -> bool {
        self.spec.is_none()
    }

    /// True when `self` covers this call, specifier included. For shell
    /// commands this asks about one subcommand at a time; see
    /// [`Rule::matches_command`].
    pub fn matches_call(&self, tool: &str, args: &Value, cwd: &Path, widen: bool) -> bool {
        if !self.names(tool) {
            return false;
        }
        let Some(spec) = self.spec.as_deref() else {
            return true;
        };
        match toolmap::class(tool) {
            ToolClass::Shell => args
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| self.matches_command(command, widen)),
            ToolClass::Read | ToolClass::Edit => toolmap::paths(tool, args)
                .iter()
                .any(|path| path_matches(spec, &self.root, cwd, path, widen)),
            ToolClass::Web => match spec.strip_prefix("domain:") {
                Some(domain) => args
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(host_of)
                    .is_some_and(|host| domain_matches(domain, &host)),
                None => false,
            },
            ToolClass::Mcp | ToolClass::Other => match toolmap::claude_name(tool).as_str() {
                "Agent" => ["agent_type", "agent", "name", "type"]
                    .iter()
                    .filter_map(|key| args.get(*key).and_then(Value::as_str))
                    .any(|name| name == spec),
                _ => false,
            },
        }
    }

    /// Match the specifier against one shell command (already a single
    /// subcommand when used for allow decisions). For deny and ask (`widen`)
    /// leading assignments and wrappers are looked past and every piece of a
    /// compound command is tried.
    pub fn matches_command(&self, command: &str, widen: bool) -> bool {
        let Some(spec) = self.spec.as_deref() else {
            return true;
        };
        let candidates: Vec<String> = if widen {
            let mut all = vec![command.trim().to_string()];
            for sub in shell::split(command).commands {
                all.push(shell::strip_prefixes(&sub));
                all.push(sub);
            }
            all
        } else {
            vec![command.trim().to_string()]
        };
        candidates
            .iter()
            .any(|candidate| command_pattern_matches(spec, candidate))
    }
}

/// `prefix:*`, `a * b`, or an exact command.
fn command_pattern_matches(pattern: &str, command: &str) -> bool {
    let pattern = pattern.trim();
    if let Some(prefix) = pattern.strip_suffix(":*") {
        let prefix = prefix.trim_end();
        return command == prefix
            || command
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace));
    }
    if !pattern.contains('*') {
        return command == pattern;
    }
    // A trailing ` *` also matches the bare command.
    if let Some(head) = pattern.strip_suffix(" *")
        && !head.contains('*')
        && command == head
    {
        return true;
    }
    wildcard(pattern, command)
}

/// `*` matches any run of characters, including none; everything else is
/// literal.
fn wildcard(pattern: &str, text: &str) -> bool {
    let pieces: Vec<&str> = pattern.split('*').collect();
    let mut rest = text;
    for (i, piece) in pieces.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(piece) {
                Some(after) => rest = after,
                None => return false,
            }
        } else if i == pieces.len() - 1 {
            return rest.ends_with(piece);
        } else {
            match rest.find(piece) {
                Some(at) => rest = &rest[at + piece.len()..],
                None => return false,
            }
        }
    }
    rest.is_empty()
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?;
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

fn domain_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return host.ends_with(&format!(".{suffix}"));
    }
    pattern == host
}

/// Make `path` absolute against `cwd` and fold `.` and `..` without touching
/// the filesystem.
pub fn absolutize(path: &str, cwd: &Path) -> PathBuf {
    let path = match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    };
    let joined = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Gitignore-flavored path patterns with Claude Code's anchors:
/// `//abs`, `~/home`, `/root-relative`, `./cwd-relative`, bare `name`.
///
/// A pattern also covers everything beneath a directory it names. With
/// `widen` (deny and ask) a relative pattern is additionally tried at any
/// depth, so a restriction on `secrets/**` also catches `app/secrets/x`.
fn path_matches(pattern: &str, root: &Path, cwd: &Path, path: &str, widen: bool) -> bool {
    let target = absolutize(path, cwd);
    let mut candidates = vec![target.clone()];
    if let Ok(real) = std::fs::canonicalize(&target)
        && widen
    {
        candidates.push(real);
    }
    let anchored = if let Some(abs) = pattern.strip_prefix("//") {
        Some(format!("/{abs}"))
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        Some(
            dirs::home_dir()
                .unwrap_or_default()
                .join(rest)
                .to_string_lossy()
                .into_owned(),
        )
    } else if let Some(rel) = pattern.strip_prefix('/') {
        Some(root.join(rel).to_string_lossy().into_owned())
    } else {
        pattern
            .strip_prefix("./")
            .map(|rel| cwd.join(rel).to_string_lossy().into_owned())
    };
    let mut globs: Vec<String> = Vec::new();
    match anchored {
        Some(abs) => globs.push(abs),
        None => {
            if pattern.contains('/') {
                globs.push(cwd.join(pattern).to_string_lossy().into_owned());
                if widen {
                    globs.push(format!("**/{pattern}"));
                }
            } else {
                // A bare name matches at any depth under the anchor.
                globs.push(format!("{}/**/{pattern}", cwd.display()));
                globs.push(cwd.join(pattern).to_string_lossy().into_owned());
            }
        }
    }
    globs.iter().any(|glob| {
        let Some(matcher) = compile(glob) else {
            return false;
        };
        candidates.iter().any(|candidate| {
            candidate
                .ancestors()
                .take_while(|a| a.parent().is_some())
                .any(|ancestor| matcher.is_match(ancestor))
        })
    })
}

fn compile(glob: &str) -> Option<GlobMatcher> {
    GlobBuilder::new(glob)
        .literal_separator(true)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(raw: &str) -> Rule {
        Rule::parse(raw, Path::new("/proj")).expect("valid rule")
    }

    fn bash(rule_text: &str, command: &str, widen: bool) -> bool {
        rule(rule_text).matches_call(
            "execute",
            &json!({ "command": command }),
            Path::new("/proj"),
            widen,
        )
    }

    fn read(rule_text: &str, path: &str, widen: bool) -> bool {
        rule(rule_text).matches_call(
            "read_file",
            &json!({ "path": path }),
            Path::new("/proj"),
            widen,
        )
    }

    #[test]
    fn parsing() {
        let r = rule("Bash(git diff:*)");
        assert_eq!(
            (r.tool.as_str(), r.spec.as_deref()),
            ("Bash", Some("git diff:*"))
        );
        assert!(rule("Bash(*)").is_whole_tool());
        assert!(rule("Bash()").is_whole_tool());
        assert!(rule("mcp__github__list").is_whole_tool());
        assert!(Rule::parse("Bash(x) trailing", Path::new("/")).is_err());
        assert!(Rule::parse("(x)", Path::new("/")).is_err());
        assert_eq!(rule("Bash(echo (hi))").spec.as_deref(), Some("echo (hi)"));
    }

    #[test]
    fn tool_names_in_either_spelling() {
        assert!(rule("Bash").names("execute"));
        assert!(rule("execute").names("execute"));
        assert!(rule("Edit").names("write_file"));
        assert!(rule("Edit").names("edit_file"));
        assert!(rule("Write").names("write_file"));
        assert!(!rule("Write").names("edit_file"));
        assert!(rule("Read").names("search_files"));
        assert!(!rule("Read").names("execute"));
    }

    #[test]
    fn mcp_rules() {
        assert!(rule("mcp__github__list").names("github__list"));
        assert!(!rule("mcp__github__list").names("github__create"));
        assert!(rule("mcp__github").names("github__create"));
        assert!(rule("mcp__github__*").names("github__create"));
        assert!(!rule("mcp__github").names("gitlab__create"));
        assert!(!rule("mcp__git").names("github__create"), "whole segment");
        assert!(rule("mcp__*").names("anything__x"));
    }

    #[test]
    fn bash_prefix_and_wildcards() {
        assert!(bash("Bash(git diff:*)", "git diff", false));
        assert!(bash("Bash(git diff:*)", "git diff --stat", false));
        assert!(!bash("Bash(git diff:*)", "git difftool", false));
        assert!(bash("Bash(git *)", "git status", false));
        assert!(
            bash("Bash(git *)", "git", false),
            "trailing ` *` allows bare"
        );
        assert!(!bash("Bash(git*)", "svn", false));
        assert!(bash("Bash(git*)", "gitk", false));
        assert!(bash("Bash(npm run * --fix)", "npm run lint --fix", false));
        assert!(bash("Bash(ls)", "ls", false));
        assert!(!bash("Bash(ls)", "ls -la", false));
    }

    #[test]
    fn deny_looks_past_assignments_and_into_compounds() {
        assert!(bash("Bash(rm:*)", "echo a && rm x", true));
        assert!(bash("Bash(rm:*)", "FOO=1 rm x", true));
        assert!(bash("Bash(rm:*)", "echo $(rm x)", true));
        assert!(
            !bash("Bash(rm:*)", "echo a && rm x", false),
            "allow does not"
        );
    }

    #[test]
    fn path_anchors() {
        assert!(read("Read(./secrets/**)", "secrets/a.txt", false));
        assert!(read(
            "Read(./secrets/**)",
            "/proj/secrets/deep/a.txt",
            false
        ));
        assert!(!read("Read(./secrets/**)", "other/a.txt", false));
        assert!(
            read("Read(/secrets/**)", "secrets/a.txt", false),
            "root-relative"
        );
        assert!(read("Read(//etc/passwd)", "/etc/passwd", false));
        assert!(
            read("Read(.env)", "app/.env", false),
            "bare name, any depth"
        );
        assert!(
            !read("Read(src/**)", "app/src/x.rs", false),
            "allow stays put"
        );
        assert!(read("Read(src/**)", "app/src/x.rs", true), "deny widens");
        assert!(
            read("Read(./secrets)", "secrets/a.txt", false),
            "directory covers contents"
        );
    }

    #[test]
    fn dot_dot_cannot_escape_a_deny() {
        assert!(read("Read(./secrets/**)", "src/../secrets/a.txt", false));
    }

    #[test]
    fn webfetch_domains() {
        let fetch = |rule_text: &str, url: &str| {
            rule(rule_text).matches_call(
                "web_fetch",
                &json!({ "url": url }),
                Path::new("/proj"),
                false,
            )
        };
        assert!(fetch(
            "WebFetch(domain:example.com)",
            "https://example.com/x"
        ));
        assert!(fetch(
            "WebFetch(domain:Example.COM)",
            "https://example.com./x"
        ));
        assert!(!fetch(
            "WebFetch(domain:example.com)",
            "https://a.example.com/x"
        ));
        assert!(fetch(
            "WebFetch(domain:*.example.com)",
            "https://a.b.example.com/x"
        ));
        assert!(!fetch(
            "WebFetch(domain:*.example.com)",
            "https://example.com/x"
        ));
        assert!(fetch("WebFetch(domain:*)", "https://anything.org"));
    }
}
