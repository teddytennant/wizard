//! Shell command text, split far enough for permission rules.
//!
//! Rules match command text, not a parsed program, and a rule is only as good
//! as the text it can see. So the splitter is conservative in the direction
//! that matters: a deny or ask rule gets to look at every piece it can find
//! (each subcommand, and whatever sits inside `$(...)` and backticks), and an
//! allow rule must cover every piece or it does not apply at all.

/// Operators that end one command and start another.
const SEPARATORS: [&str; 7] = ["&&", "||", "|&", ";", "|", "&", "\n"];

/// A command line broken into the commands it runs.
#[derive(Debug, PartialEq, Eq)]
pub struct Parts {
    /// Each subcommand, trimmed, in order. Includes commands found inside
    /// substitutions.
    pub commands: Vec<String>,
    /// True when the text could not be split with confidence (an unclosed
    /// quote or substitution, a dangling `&&` or `||`). An allow rule must
    /// not approve such a command.
    pub unparseable: bool,
}

/// Split `line` on shell separators outside quotes, and descend into command
/// substitutions.
pub fn split(line: &str) -> Parts {
    let mut parts = Parts {
        commands: Vec::new(),
        unparseable: false,
    };
    split_into(line, &mut parts, 0);
    parts
}

fn split_into(line: &str, parts: &mut Parts, depth: usize) {
    if depth > 8 {
        parts.unparseable = true;
        return;
    }
    let chars: Vec<char> = line.chars().collect();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut dangling = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == '\\' && q == '"' && i + 1 < chars.len() {
                    current.push(c);
                    current.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if q == '"' && c == '$' && chars.get(i + 1) == Some(&'(') {
                    i = take_substitution(&chars, i, &mut current, parts, depth);
                    continue;
                }
                if q == '"' && c == '`' {
                    i = take_backticks(&chars, i, &mut current, parts, depth);
                    continue;
                }
                if c == q {
                    quote = None;
                }
                current.push(c);
            }
            None => {
                if c == '\\' && i + 1 < chars.len() {
                    current.push(c);
                    current.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    current.push(c);
                } else if c == '$' && chars.get(i + 1) == Some(&'(') {
                    i = take_substitution(&chars, i, &mut current, parts, depth);
                    continue;
                } else if c == '`' {
                    i = take_backticks(&chars, i, &mut current, parts, depth);
                    continue;
                } else if let Some(op) = operator_at(&chars, i) {
                    flush(&mut current, parts);
                    dangling = op == "&&" || op == "||";
                    i += op.chars().count();
                    continue;
                } else {
                    current.push(c);
                }
            }
        }
        if !c.is_whitespace() {
            dangling = false;
        }
        i += 1;
    }
    if quote.is_some() || dangling {
        parts.unparseable = true;
    }
    flush(&mut current, parts);
}

fn operator_at(chars: &[char], i: usize) -> Option<&'static str> {
    SEPARATORS.iter().copied().find(|op| {
        op.chars()
            .enumerate()
            .all(|(k, oc)| chars.get(i + k) == Some(&oc))
    })
}

fn flush(current: &mut String, parts: &mut Parts) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        parts.commands.push(trimmed.to_string());
    }
    current.clear();
}

/// Consume `$( ... )` starting at `chars[start]`; the inner text is split as
/// its own command line. Returns the index after the closing paren.
fn take_substitution(
    chars: &[char],
    start: usize,
    current: &mut String,
    parts: &mut Parts,
    depth: usize,
) -> usize {
    let mut level = 0usize;
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '(' => level += 1,
            ')' => {
                level -= 1;
                if level == 0 {
                    let inner: String = chars[start + 2..i].iter().collect();
                    split_into(&inner, parts, depth + 1);
                    current.extend(&chars[start..=i]);
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    parts.unparseable = true;
    current.extend(&chars[start..]);
    chars.len()
}

fn take_backticks(
    chars: &[char],
    start: usize,
    current: &mut String,
    parts: &mut Parts,
    depth: usize,
) -> usize {
    match chars[start + 1..].iter().position(|c| *c == '`') {
        Some(offset) => {
            let end = start + 1 + offset;
            let inner: String = chars[start + 1..end].iter().collect();
            split_into(&inner, parts, depth + 1);
            current.extend(&chars[start..=end]);
            end + 1
        }
        None => {
            parts.unparseable = true;
            current.extend(&chars[start..]);
            chars.len()
        }
    }
}

/// Words that run another command; rules see the command underneath.
const WRAPPERS: [&str; 5] = ["time", "nohup", "nice", "timeout", "command"];

/// `cmd` with leading `VAR=value` assignments and the process wrappers in
/// [`WRAPPERS`] removed, so `FOO=1 timeout 5 rm x` is matched as `rm x`.
pub fn strip_prefixes(cmd: &str) -> String {
    let mut words: Vec<&str> = cmd.split_whitespace().collect();
    while let Some(first) = words.first().copied() {
        if is_assignment(first) {
            words.remove(0);
        } else if WRAPPERS.contains(&first) {
            words.remove(0);
            // `timeout 5`, `nice -n 5`: drop the wrapper's own arguments.
            while words
                .first()
                .is_some_and(|w| w.starts_with('-') || w.parse::<f64>().is_ok())
            {
                words.remove(0);
            }
        } else {
            break;
        }
    }
    words.join(" ")
}

/// Commands that only read. Claude Code runs these without asking in every
/// mode that would otherwise ask, and so does Wizard's `dontAsk`: a compound
/// like `touch x && echo done` needs a rule for `touch` and none for `echo`.
const READ_ONLY: [&str; 36] = [
    "echo", "pwd", "ls", "cat", "head", "tail", "wc", "grep", "rg", "which", "whoami", "date",
    "uname", "true", "false", "file", "stat", "du", "df", "printenv", "sort", "uniq", "cut", "tr",
    "diff", "basename", "dirname", "realpath", "id", "hostname", "tree", "nl", "seq", "cd", "type",
    "test",
];

/// Git subcommands that do not change the repository.
const READ_ONLY_GIT: [&str; 9] = [
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "rev-parse",
    "ls-files",
    "describe",
    "shortlog",
];

/// True when one subcommand cannot change anything: a read-only program,
/// with no redirection and no substitution that could smuggle something in.
pub fn is_read_only(cmd: &str) -> bool {
    if cmd.contains('>') || cmd.contains('`') || cmd.contains("$(") {
        return false;
    }
    let mut words = cmd.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    match first {
        "git" => {
            // `git -C dir diff`: `-C` and `-c` take a value; skip it.
            let mut sub = None;
            while let Some(word) = words.next() {
                if matches!(word, "-C" | "-c") {
                    words.next();
                } else if !word.starts_with('-') {
                    sub = Some(word);
                    break;
                }
            }
            sub.is_some_and(|sub| READ_ONLY_GIT.contains(&sub))
        }
        "find" => !cmd.split_whitespace().any(|w| {
            matches!(
                w,
                "-exec" | "-execdir" | "-delete" | "-ok" | "-fprint" | "-fprintf"
            )
        }),
        other => READ_ONLY.contains(&other),
    }
}

fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmds(line: &str) -> Vec<String> {
        split(line).commands
    }

    #[test]
    fn splits_on_every_operator() {
        assert_eq!(
            cmds("a && b || c ; d | e |& f & g\nh"),
            ["a", "b", "c", "d", "e", "f", "g", "h"]
        );
    }

    #[test]
    fn quotes_hide_operators() {
        assert_eq!(
            cmds(r#"echo "a && b" 'c ; d'"#),
            [r#"echo "a && b" 'c ; d'"#]
        );
    }

    #[test]
    fn substitutions_are_commands_too() {
        let parts = split("echo $(rm x) `ls`");
        assert!(parts.commands.contains(&"rm x".to_string()));
        assert!(parts.commands.contains(&"ls".to_string()));
        assert!(!parts.unparseable);
        let parts = split(r#"echo "$(rm x)""#);
        assert!(parts.commands.contains(&"rm x".to_string()));
    }

    #[test]
    fn a_dangling_operator_or_open_quote_is_unparseable() {
        assert!(split("a &&").unparseable);
        assert!(split("echo 'oops").unparseable);
        assert!(split("echo $(oops").unparseable);
        assert!(!split("a && b").unparseable);
    }

    #[test]
    fn read_only_commands_are_recognised() {
        for yes in [
            "echo hi",
            "ls -la",
            "git status",
            "git -C x diff --stat",
            "find . -name x",
            "cat a b",
        ] {
            assert!(is_read_only(yes), "{yes}");
        }
        for no in [
            "touch x",
            "echo hi > out",
            "echo $(rm x)",
            "git push",
            "git commit -m x",
            "find . -delete",
            "find . -exec rm {} ;",
            "rm -rf x",
            "",
        ] {
            assert!(!is_read_only(no), "{no}");
        }
    }

    #[test]
    fn prefixes_and_wrappers_are_stripped() {
        assert_eq!(strip_prefixes("FOO=1 BAR=2 rm x"), "rm x");
        assert_eq!(strip_prefixes("timeout 5 rm x"), "rm x");
        assert_eq!(strip_prefixes("nice -n 5 time rm x"), "rm x");
        assert_eq!(strip_prefixes("rm FOO=1"), "rm FOO=1");
    }
}
