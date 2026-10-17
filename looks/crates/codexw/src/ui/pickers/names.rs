//! Session names. Wizard keeps no thread names, so `/rename` is client-only: a JSON map from
//! session id to name in `~/.config/codexw/names.json`, read by the resume picker and the exit
//! hint.

use std::collections::BTreeMap;
use std::path::PathBuf;

pub fn path(home: &str) -> PathBuf {
    PathBuf::from(home).join(".config/codexw/names.json")
}

pub fn load(home: &str) -> BTreeMap<String, String> {
    std::fs::read_to_string(path(home))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn get(home: &str, id: &str) -> Option<String> {
    load(home).remove(id)
}

/// The id a name was given to, for `/resume <name>`.
pub fn id_for(home: &str, name: &str) -> Option<String> {
    load(home)
        .into_iter()
        .find(|(_, n)| n == name)
        .map(|(id, _)| id)
}

pub fn set(home: &str, id: &str, name: &str) -> std::io::Result<()> {
    let mut map = load(home);
    map.insert(id.to_string(), name.to_string());
    let p = path(home);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&map).map_err(std::io::Error::other)?;
    std::fs::write(p, text + "\n")
}

/// `codexw resume, then select my-thread (<uuid>)`, or `codexw resume <uuid>` when unnamed.
pub fn resume_hint(name: Option<&str>, id: &str) -> String {
    match name {
        Some(n) => format!("wizard-ui-codex resume, then select {n} ({id})"),
        None => format!("wizard-ui-codex resume {id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_resolve() {
        let home = std::env::temp_dir().join(format!("cxw-names-{}", std::process::id()));
        let h = home.to_str().unwrap();
        assert_eq!(get(h, "a"), None);
        set(h, "a", "my-thread").unwrap();
        set(h, "b", "other").unwrap();
        assert_eq!(get(h, "a").as_deref(), Some("my-thread"));
        assert_eq!(id_for(h, "other").as_deref(), Some("b"));
        assert_eq!(
            resume_hint(Some("my-thread"), "a"),
            "wizard-ui-codex resume, then select my-thread (a)"
        );
        assert_eq!(resume_hint(None, "a"), "wizard-ui-codex resume a");
        let _ = std::fs::remove_dir_all(home);
    }
}
