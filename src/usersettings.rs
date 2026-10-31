//! Read and write `~/.wizard/settings.json`, the Claude-format file Wizard
//! owns, for the commands that save a setting there (`/advisor`,
//! `/setup-bedrock`, `/setup-vertex`, `/voice`).
//!
//! Edits keep every key they do not name, and a file that is not valid JSON is
//! left alone with an error rather than overwritten.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::Config;

pub fn path() -> Result<PathBuf, String> {
    Config::wizard_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|err| format!("{err:#}"))
}

/// The file's path and its JSON (an empty object when it does not exist).
pub fn load() -> Result<(PathBuf, Value), String> {
    load_at(path()?)
}

pub fn load_at(path: PathBuf) -> Result<(PathBuf, Value), String> {
    let value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|err| format!("{} is not valid JSON ({err}); left alone", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    if !value.is_object() {
        return Err(format!("{} is not a JSON object; left alone", path.display()));
    }
    Ok((path, value))
}

pub fn save(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(value).map_err(|err| err.to_string())?;
    std::fs::write(path, format!("{text}\n")).map_err(|err| format!("{}: {err}", path.display()))
}

/// A top-level string setting, if present and non-empty.
pub fn get_str(key: &str) -> Option<String> {
    let (_, value) = load().ok()?;
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Set (`Some`) or remove (`None`) a top-level key. Returns the file written.
pub fn set(key: &str, value: Option<Value>) -> Result<PathBuf, String> {
    set_at(path()?, key, value)
}

pub fn set_at(path: PathBuf, key: &str, value: Option<Value>) -> Result<PathBuf, String> {
    let (path, mut settings) = load_at(path)?;
    let object = settings.as_object_mut().expect("load checked the shape");
    match value {
        Some(value) => {
            object.insert(key.to_string(), value);
        }
        None => {
            object.remove(key);
        }
    }
    save(&path, &settings)?;
    Ok(path)
}

/// Merge `pairs` into the `env` block, and drop every name in `remove`.
pub fn merge_env(pairs: &[(&str, String)], remove: &[&str]) -> Result<PathBuf, String> {
    merge_env_at(path()?, pairs, remove)
}

pub fn merge_env_at(
    path: PathBuf,
    pairs: &[(&str, String)],
    remove: &[&str],
) -> Result<PathBuf, String> {
    let (path, mut settings) = load_at(path)?;
    let object = settings.as_object_mut().expect("load checked the shape");
    let env = object
        .entry("env")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(env) = env.as_object_mut() else {
        return Err(format!(
            "{}: \"env\" is not an object; left alone",
            path.display()
        ));
    };
    for name in remove {
        env.remove(*name);
    }
    for (name, value) in pairs {
        env.insert((*name).to_string(), Value::String(value.clone()));
    }
    save(&path, &settings)?;
    Ok(path)
}

/// The names in the `env` block with their values.
pub fn env_block() -> Vec<(String, String)> {
    let Ok((_, value)) = load() else {
        return Vec::new();
    };
    value
        .get("env")
        .and_then(Value::as_object)
        .map(|env| {
            env.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_keeps_other_keys_and_remove_deletes_only_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        set_at(file.clone(), "advisorModel", Some(json!("opus"))).unwrap();
        set_at(file.clone(), "keep", Some(json!(1))).unwrap();
        set_at(file.clone(), "advisorModel", None).unwrap();
        let (_, v) = load_at(file).unwrap();
        assert_eq!(v.get("advisorModel"), None);
        assert_eq!(v["keep"], 1);
    }

    #[test]
    fn merge_env_adds_and_removes_names() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        merge_env_at(file.clone(), &[("A", "1".into()), ("B", "2".into())], &[]).unwrap();
        merge_env_at(file.clone(), &[("C", "3".into())], &["A"]).unwrap();
        let (_, v) = load_at(file).unwrap();
        assert_eq!(v["env"], json!({"B": "2", "C": "3"}));
    }

    #[test]
    fn a_file_that_is_not_json_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, "{ nope").unwrap();
        assert!(set_at(file.clone(), "a", Some(json!(1))).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "{ nope");
    }
}
