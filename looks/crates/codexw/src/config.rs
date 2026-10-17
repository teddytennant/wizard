//! `~/.config/codexw/config.toml`: the few settings codexw keeps itself (`theme`,
//! `session_picker_view`). Flat `key = "value"` lines; a write changes one key and leaves every
//! other line, comment and blank as it was.

use std::path::{Path, PathBuf};

pub fn path(home: &str) -> PathBuf {
    PathBuf::from(home).join(".config/codexw/config.toml")
}

fn parse_line(line: &str) -> Option<(&str, String)> {
    let line = line.trim();
    if line.starts_with('#') || line.starts_with('[') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    let v = v.trim();
    let v = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v);
    Some((k.trim(), v.replace("\\\"", "\"").replace("\\\\", "\\")))
}

/// The value of `key`, if the file has one.
pub fn get(path: &Path, key: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .filter_map(parse_line)
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// Set `key` to `value`, or remove it with `None`, keeping the rest of the file.
pub fn set(path: &Path, key: &str, value: Option<&str>) -> std::io::Result<()> {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    let mut done = false;
    for line in old.lines() {
        match parse_line(line) {
            Some((k, _)) if k == key => {
                if let Some(v) = value.filter(|_| !done) {
                    out.push(format!(
                        "{key} = \"{}\"",
                        v.replace('\\', "\\\\").replace('"', "\\\"")
                    ));
                }
                done = true;
            }
            _ => out.push(line.to_string()),
        }
    }
    if let (false, Some(v)) = (done, value) {
        out.push(format!(
            "{key} = \"{}\"",
            v.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = out.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cxw-cfg-{}-{name}.toml", std::process::id()))
    }

    #[test]
    fn set_adds_replaces_and_removes_one_key_only() {
        let p = tmp("a");
        let _ = std::fs::remove_file(&p);
        set(&p, "theme", Some("nord")).unwrap();
        set(&p, "session_picker_view", Some("comfortable")).unwrap();
        assert_eq!(get(&p, "theme").as_deref(), Some("nord"));
        set(&p, "theme", Some("dracula")).unwrap();
        assert_eq!(get(&p, "theme").as_deref(), Some("dracula"));
        assert_eq!(
            get(&p, "session_picker_view").as_deref(),
            Some("comfortable")
        );
        set(&p, "theme", None).unwrap();
        assert_eq!(get(&p, "theme"), None);
        assert_eq!(
            get(&p, "session_picker_view").as_deref(),
            Some("comfortable")
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn comments_tables_and_unknown_keys_survive() {
        let p = tmp("b");
        std::fs::write(&p, "# mine\nother = 3\n\n[tui]\nx = \"y\"\n").unwrap();
        set(&p, "theme", Some("zenburn")).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(
            text,
            "# mine\nother = 3\n\n[tui]\nx = \"y\"\ntheme = \"zenburn\"\n"
        );
    }

    #[test]
    fn a_missing_file_reads_as_nothing() {
        assert_eq!(get(Path::new("/no/such/config.toml"), "theme"), None);
    }
}
