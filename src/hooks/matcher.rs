//! Hook matchers.
//!
//! `hooks.toml` matchers are globs over the tool name and stay that way. Hooks
//! that come from a Claude-format settings file use Claude Code's rules
//! instead: `*`, an empty string or no matcher matches everything; a matcher
//! made only of letters, digits, `_`, `-`, spaces, `,` and `|` is a list of
//! exact names; anything else is an unanchored regular expression.

use globset::{Glob, GlobMatcher};
use regex::Regex;

/// A compiled matcher.
pub enum Matcher {
    /// Matches every subject.
    Any,
    /// `hooks.toml` glob over the tool name.
    Glob(GlobMatcher),
    /// Claude-style list of exact names.
    Exact(Vec<String>),
    /// Claude-style regular expression.
    Regex(Regex),
}

impl Matcher {
    /// Compile a `hooks.toml` glob.
    pub fn glob(pattern: &str) -> Result<Self, String> {
        Glob::new(pattern)
            .map(|glob| Matcher::Glob(glob.compile_matcher()))
            .map_err(|err| err.to_string())
    }

    /// Compile a Claude-style matcher. `None` and `""` and `"*"` match all.
    pub fn claude(pattern: Option<&str>) -> Result<Self, String> {
        let Some(pattern) = pattern.map(str::trim) else {
            return Ok(Matcher::Any);
        };
        if pattern.is_empty() || pattern == "*" {
            return Ok(Matcher::Any);
        }
        let plain = pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ' | ',' | '|'));
        if plain {
            let names = pattern
                .split(['|', ','])
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect();
            return Ok(Matcher::Exact(names));
        }
        Regex::new(pattern)
            .map(Matcher::Regex)
            .map_err(|err| err.to_string())
    }

    /// True when `subject` satisfies the matcher. `aliases` are other
    /// spellings of the same subject (a tool is `execute` to Wizard and `Bash`
    /// to a Claude settings file); any one matching is enough.
    pub fn is_match(&self, subject: &str, aliases: &[&str]) -> bool {
        let one = |name: &str| match self {
            Matcher::Any => true,
            Matcher::Glob(glob) => glob.is_match(name),
            Matcher::Exact(names) => names.iter().any(|n| n == name),
            Matcher::Regex(re) => re.is_match(name),
        };
        one(subject) || aliases.iter().any(|alias| one(alias))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude(pattern: &str) -> Matcher {
        Matcher::claude(Some(pattern)).expect("valid")
    }

    #[test]
    fn empty_star_and_missing_match_everything() {
        for m in [
            Matcher::claude(None).unwrap(),
            claude(""),
            claude("*"),
            claude("  "),
        ] {
            assert!(m.is_match("anything", &[]));
        }
    }

    #[test]
    fn plain_characters_are_an_exact_list() {
        let m = claude("Edit|Write");
        assert!(m.is_match("Edit", &[]));
        assert!(m.is_match("Write", &[]));
        assert!(!m.is_match("MultiEdit", &[]), "not a substring match");
        let m = claude("Edit, Write");
        assert!(m.is_match("Write", &[]));
    }

    #[test]
    fn anything_else_is_an_unanchored_regex() {
        let m = claude("mcp__.*");
        assert!(m.is_match("mcp__github__list", &[]));
        assert!(!m.is_match("Bash", &[]));
        let m = claude("^Notebook");
        assert!(m.is_match("NotebookEdit", &[]));
        assert!(Matcher::claude(Some("a(")).is_err());
    }

    #[test]
    fn a_bare_mcp_server_name_matches_no_tool() {
        assert!(!claude("mcp__github").is_match("mcp__github__list", &[]));
    }

    #[test]
    fn aliases_let_one_tool_answer_to_both_names() {
        let m = claude("Bash");
        assert!(m.is_match("execute", &["Bash"]));
        assert!(!m.is_match("execute", &[]));
    }

    #[test]
    fn glob_matchers_keep_their_old_meaning() {
        let m = Matcher::glob("git_*").unwrap();
        assert!(m.is_match("git_status", &[]));
        assert!(!m.is_match("execute", &[]));
        assert!(Matcher::glob("a{").is_err());
    }
}
