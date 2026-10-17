//! Ctrl+G: hand the draft to `$VISUAL` or `$EDITOR` and take the result back (Codex
//! `external_editor.rs`). The editor gets the terminal to itself while it runs; `app::run` does
//! the pausing, this file resolves the command, runs it and reads the file back.

use std::fmt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Shown while the editor is open, in the footer's left slot.
pub const HINT: &str = "Save and close external editor to continue.";

#[derive(Debug, PartialEq, Eq)]
pub enum EditorError {
    Missing,
    ParseFailed,
    EmptyCommand,
    Io(String),
    Status(String),
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("neither VISUAL nor EDITOR is set"),
            Self::ParseFailed => f.write_str("failed to parse editor command"),
            Self::EmptyCommand => f.write_str("editor command is empty"),
            Self::Io(e) => f.write_str(e),
            Self::Status(s) => write!(f, "editor exited with status {s}"),
        }
    }
}

/// Split like a POSIX shell word list: whitespace separates, single quotes are literal, double
/// quotes allow `\"` and `\\`, a backslash outside quotes escapes the next character. `None` for
/// an unterminated quote or a trailing backslash (Codex uses `shlex::split`).
pub fn split_command(raw: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => cur.push(c),
                            '\n' => {}
                            c => {
                                cur.push('\\');
                                cur.push(c);
                            }
                        },
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(chars.next()?);
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Some(out)
}

/// `$VISUAL` first, then `$EDITOR`, split into program and arguments.
pub fn resolve_editor_command(
    visual: Option<String>,
    editor: Option<String>,
) -> Result<Vec<String>, EditorError> {
    let raw = visual.or(editor).ok_or(EditorError::Missing)?;
    let parts = split_command(&raw).ok_or(EditorError::ParseFailed)?;
    if parts.is_empty() {
        return Err(EditorError::EmptyCommand);
    }
    Ok(parts)
}

pub fn resolve_from_env() -> Result<Vec<String>, EditorError> {
    resolve_editor_command(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok())
}

/// Write `seed` to a `.md` temp file, run the editor on it with the terminal inherited, and
/// return what the file holds afterwards.
pub fn run_editor(seed: &str, cmd: &[String]) -> Result<String, EditorError> {
    let (program, args) = cmd.split_first().ok_or(EditorError::EmptyCommand)?;
    let path = temp_path();
    let io = |e: std::io::Error| EditorError::Io(e.to_string());
    std::fs::write(&path, seed).map_err(io)?;
    let status = Command::new(program)
        .args(args)
        .arg(&path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    let result = match status {
        Err(e) => Err(io(e)),
        Ok(st) if !st.success() => Err(EditorError::Status(st.to_string())),
        Ok(_) => std::fs::read_to_string(&path).map_err(io),
    };
    let _ = std::fs::remove_file(&path);
    result
}

fn temp_path() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    std::env::temp_dir().join(format!(".codexw-{}-{n}-{nanos}.md", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Option<Vec<String>> {
        split_command(s)
    }

    #[test]
    fn split_follows_shell_quoting() {
        assert_eq!(words("vim"), Some(vec!["vim".into()]));
        assert_eq!(
            words("code --wait -n"),
            Some(vec!["code".into(), "--wait".into(), "-n".into()])
        );
        assert_eq!(
            words(r#"emacs -nw --eval '(setq a "b c")'"#),
            Some(vec![
                "emacs".into(),
                "-nw".into(),
                "--eval".into(),
                r#"(setq a "b c")"#.into()
            ])
        );
        assert_eq!(
            words(r#""my editor" a\ b"#),
            Some(vec!["my editor".into(), "a b".into()])
        );
        assert_eq!(words("'unterminated"), None);
        assert_eq!(words("trailing\\"), None);
        assert_eq!(words("  "), Some(vec![]));
        assert_eq!(words("''"), Some(vec![String::new()]));
    }

    #[test]
    fn visual_wins_over_editor_and_neither_is_an_error() {
        assert_eq!(
            resolve_editor_command(Some("vis".into()), Some("ed".into())),
            Ok(vec!["vis".to_string()])
        );
        assert_eq!(
            resolve_editor_command(None, Some("ed -x".into())),
            Ok(vec!["ed".to_string(), "-x".to_string()])
        );
        assert_eq!(
            resolve_editor_command(None, None),
            Err(EditorError::Missing)
        );
        assert_eq!(
            resolve_editor_command(Some("".into()), None),
            Err(EditorError::EmptyCommand)
        );
        assert_eq!(
            resolve_editor_command(Some("'x".into()), None),
            Err(EditorError::ParseFailed)
        );
    }

    /// A script run through `sh`: executing a file another thread has just written fails with
    /// "text file busy" when a test thread forks while its descriptor is open.
    #[cfg(unix)]
    fn script(name: &str, body: &str) -> Vec<String> {
        let p = std::env::temp_dir().join(format!("cxw-ed-{}-{name}.sh", std::process::id()));
        std::fs::write(&p, format!("{body}\n")).unwrap();
        vec!["sh".into(), p.to_string_lossy().into_owned()]
    }

    #[cfg(unix)]
    #[test]
    fn the_editor_sees_the_seed_and_what_it_writes_comes_back() {
        let sh = script("append", r#"printf '%s and more' "$(cat "$1")" > "$1""#);
        let out = run_editor("seed", &sh).unwrap();
        let _ = std::fs::remove_file(&sh[1]);
        assert_eq!(out, "seed and more");
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_editor_is_an_error_and_a_missing_one_too() {
        let err = run_editor("x", &["false".into()]).unwrap_err();
        assert!(matches!(err, EditorError::Status(_)), "{err:?}");
        assert!(err.to_string().starts_with("editor exited with status"));
        let err = run_editor("x", &["/nonexistent/cxw-editor".into()]).unwrap_err();
        assert!(matches!(err, EditorError::Io(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_temp_file_is_markdown_and_is_removed() {
        let sh = script(
            "path",
            r#"case "$1" in *.md) printf ok > "$1";; *) printf bad > "$1";; esac"#,
        );
        let out = run_editor("", &sh).unwrap();
        let _ = std::fs::remove_file(&sh[1]);
        assert_eq!(out, "ok");
    }
}
