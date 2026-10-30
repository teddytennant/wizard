//! The `.claude/` directory, read in place.
//!
//! Claude Code keeps commands, skills, subagents, output styles, rules and
//! plugins in `~/.claude/` and `<project>/.claude/`. Wizard reads the same
//! files where they are, so a project set up for Claude Code works here
//! without the `/settings` import step. Wizard's own directories
//! (`~/.wizard/...`, `<project>/.wizard/...`) shadow these on a name clash.
//!
//! This module only resolves paths and parses the shared file formats. What a
//! loader does with them stays in the loader (`crate::commands::load`,
//! `crate::skills::default_roots`, `crate::instructions`).

pub mod agents;
pub mod mcp;
pub mod plugins;
pub mod styles;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn env_on(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty() && value != "0")
}

/// `--bare` / `--safe-mode` (or Claude Code's `CLAUDE_CODE_SIMPLE`): no
/// automatic discovery of `.claude` files, instruction files, plugins or
/// `.mcp.json`. Only what the command line names is loaded.
pub fn bare() -> bool {
    env_on(crate::cli_claude::env::BARE) || env_on("CLAUDE_CODE_SIMPLE")
}

/// `CLAUDE_CODE_DISABLE_CLAUDE_MDS`: ignore every `CLAUDE.md` (and
/// `CLAUDE.local.md`, and Claude's rules folders). `WIZARD.md` and
/// `AGENTS.md` still load.
pub fn claude_mds_disabled() -> bool {
    env_on("CLAUDE_CODE_DISABLE_CLAUDE_MDS")
}

/// `--disable-slash-commands`: no custom commands, no skills.
pub fn slash_commands_disabled() -> bool {
    env_on(crate::cli_claude::env::NO_SLASH_COMMANDS)
}

/// `~/.claude`, or `$CLAUDE_CONFIG_DIR` when set (Claude Code honors it too).
pub fn user_dir() -> Option<PathBuf> {
    user_dir_from(
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn user_dir_from(config_dir: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    config_dir
        .filter(|dir| !dir.as_os_str().is_empty())
        .or_else(|| home.map(|home| home.join(".claude")))
}

/// `<project>/.claude`.
pub fn project_dir(project_root: &Path) -> PathBuf {
    project_root.join(".claude")
}

/// The `.claude/<leaf>` directories to read, user first and project last, so a
/// later entry shadows an earlier one.
pub fn dirs_for(project_root: &Path, leaf: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if bare() {
        return out;
    }
    if let Some(user) = user_dir() {
        out.push(user.join(leaf));
    }
    out.push(project_dir(project_root).join(leaf));
    out
}

/// Frontmatter of a markdown file as flat `key: value` pairs, plus the body.
///
/// Claude Code files use a small subset of YAML: scalars, inline lists
/// (`tools: Read, Grep` or `[a, b]`) and `-` block lists. Block lists are
/// joined with commas so every value is one string. Anything else is ignored.
pub fn split_frontmatter(raw: &str) -> (BTreeMap<String, String>, String) {
    let lines: Vec<&str> = raw.lines().collect();
    let mut meta = BTreeMap::new();
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return (meta, raw.trim().to_string());
    }
    let Some(end) = lines[1..].iter().position(|l| l.trim_end() == "---") else {
        return (meta, raw.trim().to_string());
    };
    let mut key: Option<String> = None;
    for line in &lines[1..1 + end] {
        if let Some(item) = line.trim_start().strip_prefix("- ")
            && let Some(key) = &key
        {
            let entry = meta.entry(key.clone()).or_insert_with(String::new);
            if !entry.is_empty() {
                entry.push(',');
            }
            entry.push_str(unquote(item.trim()));
            continue;
        }
        if line.starts_with(char::is_whitespace) || line.trim().starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim().to_string();
        let v = v.trim();
        let v = v
            .strip_prefix('[')
            .and_then(|v| v.strip_suffix(']'))
            .unwrap_or(v);
        if !v.is_empty() {
            meta.insert(k.clone(), unquote(v).to_string());
        }
        key = Some(k);
    }
    (meta, lines[2 + end..].join("\n").trim().to_string())
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Every `*.md` under `dir`, recursively, with the colon-joined namespace of
/// the subdirectories it sits in (`team/deploy.md` is `("team:deploy", path)`).
/// Sorted by name; a missing directory is empty.
pub fn namespaced_markdown(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    walk(dir, "", &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if !name.starts_with('.') {
                walk(&path, &format!("{prefix}{name}:"), out);
            }
        } else if let Some(stem) = name.strip_suffix(".md")
            && !stem.is_empty()
        {
            out.push((format!("{prefix}{stem}"), path));
        }
    }
}

/// `.claude/rules/**/*.md` bodies that apply everywhere, in path order.
///
/// A rule whose frontmatter has `paths:` only applies to matching files, and
/// Wizard has no per-file instruction injection, so those are left out rather
/// than applied to the whole project.
pub fn unconditional_rules(rules_dir: &Path) -> Vec<(PathBuf, String)> {
    namespaced_markdown(rules_dir)
        .into_iter()
        .filter_map(|(_, path)| {
            let raw = std::fs::read_to_string(&path).ok()?;
            let (meta, body) = split_frontmatter(&raw);
            (!meta.contains_key("paths") && !body.trim().is_empty()).then_some((path, body))
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};

    pub struct Tmp(pub PathBuf);

    impl Tmp {
        pub fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wizard-claude-dir-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("tmp dir");
            Self(dir)
        }

        pub fn write(&self, rel: &str, content: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, content).expect("write");
            path
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Tmp;
    use super::*;

    #[test]
    fn config_dir_env_beats_home() {
        assert_eq!(
            user_dir_from(Some("/cfg".into()), Some("/home/u".into())),
            Some(PathBuf::from("/cfg"))
        );
        assert_eq!(
            user_dir_from(None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.claude"))
        );
        assert_eq!(
            user_dir_from(Some("".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.claude")),
            "an empty variable is unset"
        );
    }

    #[test]
    fn frontmatter_reads_scalars_inline_and_block_lists() {
        let raw = "---\nname: rev\ndescription: \"checks things\"\ntools: Read, Grep\nmodel: sonnet\nlist:\n  - a\n  - b\n---\nbody here\n";
        let (meta, body) = split_frontmatter(raw);
        assert_eq!(meta["name"], "rev");
        assert_eq!(meta["description"], "checks things");
        assert_eq!(meta["tools"], "Read, Grep");
        assert_eq!(meta["list"], "a,b");
        assert_eq!(body, "body here");
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let (meta, body) = split_frontmatter("just text\n");
        assert!(meta.is_empty());
        assert_eq!(body, "just text");
    }

    #[test]
    fn subdirectories_become_colon_namespaces() {
        let tmp = Tmp::new("ns");
        tmp.write("c/hi.md", "x");
        tmp.write("c/team/deploy.md", "x");
        tmp.write("c/team/deep/run.md", "x");
        tmp.write("c/notes.txt", "x");
        let names: Vec<String> = namespaced_markdown(&tmp.path().join("c"))
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["hi", "team:deep:run", "team:deploy"]);
    }

    #[test]
    fn path_scoped_rules_are_left_out() {
        let tmp = Tmp::new("rules");
        tmp.write("r/always.md", "keep me");
        tmp.write("r/scoped.md", "---\npaths: src/**/*.rs\n---\nrust only");
        let rules = unconditional_rules(&tmp.path().join("r"));
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].1, "keep me");
    }
}
