//! Writing a "don't ask again" answer down.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Where an allow rule chosen at a prompt goes: the project's
/// `.claude/settings.local.json`, which Claude Code writes the same answer to
/// and which Wizard reads as well. A project that already has a `.wizard/`
/// directory and no `.claude/` one keeps it there instead.
pub fn settings_local_path(project: &Path) -> PathBuf {
    if project.join(".wizard").is_dir() && !project.join(".claude").is_dir() {
        project.join(".wizard").join("settings.local.json")
    } else {
        project.join(".claude").join("settings.local.json")
    }
}

/// Add `rules` to `permissions.allow` in the project's local settings, keeping
/// every other key and skipping a rule already there. Returns the file.
pub fn persist_allow(project: &Path, rules: &[String]) -> std::io::Result<PathBuf> {
    let path = settings_local_path(project);
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not valid JSON: {err}", path.display()),
            )
        })?,
        _ => json!({}),
    };
    let Some(object) = root.as_object_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a JSON object", path.display()),
        ));
    };
    let permissions = object
        .entry("permissions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "`permissions` is not an object",
            )
        })?;
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "`permissions.allow` is not a list",
            )
        })?;
    for rule in rules {
        if !allow.iter().any(|have| have.as_str() == Some(rule)) {
            allow.push(Value::String(rule.clone()));
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(&root)?;
    text.push('\n');
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wizard-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_rule_is_added_and_other_keys_survive() {
        let dir = temp("keep");
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(
            dir.join(".claude/settings.local.json"),
            r#"{"env":{"A":"1"},"permissions":{"allow":["Read"],"deny":["Bash(rm:*)"]}}"#,
        )
        .unwrap();
        let path =
            persist_allow(&dir, &["Bash(git diff:*)".to_string(), "Read".to_string()]).unwrap();
        let got: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(got["env"]["A"], "1");
        assert_eq!(got["permissions"]["deny"][0], "Bash(rm:*)");
        assert_eq!(
            got["permissions"]["allow"],
            json!(["Read", "Bash(git diff:*)"])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_file_is_created_under_claude() {
        let dir = temp("new");
        let path = persist_allow(&dir, &["WebFetch(domain:example.com)".to_string()]).unwrap();
        assert_eq!(path, dir.join(".claude/settings.local.json"));
        let got: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            got["permissions"]["allow"],
            json!(["WebFetch(domain:example.com)"])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_wizard_only_project_keeps_it_in_wizard() {
        let dir = temp("wizard");
        std::fs::create_dir_all(dir.join(".wizard")).unwrap();
        assert_eq!(
            settings_local_path(&dir),
            dir.join(".wizard/settings.local.json")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broken_json_is_left_alone() {
        let dir = temp("broken");
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/settings.local.json"), "{nope").unwrap();
        assert!(persist_allow(&dir, &["Read".to_string()]).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(".claude/settings.local.json")).unwrap(),
            "{nope"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
