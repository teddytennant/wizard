// OWNER: dialogs
//! What the dialogs remember between runs: the theme, favorite and recent models, pinned
//! sessions and session titles. One small JSON file, `~/.config/openw/state.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "state.json";
const RECENT_MAX: usize = 10;
/// Pinned sessions get quick-switch slots 1 to 9.
pub const PIN_SLOTS: usize = 9;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub theme: Option<String>,
    pub favorites: Vec<String>,
    pub recent: Vec<String>,
    pub pinned: Vec<String>,
    pub titles: BTreeMap<String, String>,
    pub disable_title: bool,
    pub disable_animations: bool,
}

impl Prefs {
    pub fn load(dir: &Path) -> Prefs {
        std::fs::read_to_string(dir.join(FILE))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write beside the file and rename, so a crash never leaves half a file.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let body = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::private::write_atomic(&dir.join(FILE), body.as_bytes())
    }

    pub fn is_favorite(&self, id: &str) -> bool {
        self.favorites.iter().any(|f| f == id)
    }

    pub fn toggle_favorite(&mut self, id: &str) {
        match self.favorites.iter().position(|f| f == id) {
            Some(i) => {
                self.favorites.remove(i);
            }
            None => self.favorites.insert(0, id.to_string()),
        }
    }

    /// Most recent first, no repeats, at most ten.
    pub fn push_recent(&mut self, id: &str) {
        self.recent.retain(|r| r != id);
        self.recent.insert(0, id.to_string());
        self.recent.truncate(RECENT_MAX);
    }

    pub fn is_pinned(&self, id: &str) -> bool {
        self.pinned.iter().any(|p| p == id)
    }

    pub fn toggle_pin(&mut self, id: &str) {
        match self.pinned.iter().position(|p| p == id) {
            Some(i) => {
                self.pinned.remove(i);
            }
            None => self.pinned.push(id.to_string()),
        }
    }

    /// Quick-switch slot (1 to 9) of a pinned session.
    pub fn pin_slot(&self, id: &str) -> Option<usize> {
        self.pinned
            .iter()
            .position(|p| p == id)
            .filter(|i| *i < PIN_SLOTS)
            .map(|i| i + 1)
    }
}

/// `$XDG_CONFIG_HOME/openw`, else `~/.config/openw`.
pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(s).join("openw"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/openw")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_dedupes_and_caps() {
        let mut p = Prefs::default();
        for i in 0..12 {
            p.push_recent(&format!("m{i}"));
        }
        p.push_recent("m5");
        assert_eq!(p.recent.len(), RECENT_MAX);
        assert_eq!(p.recent[0], "m5");
        assert_eq!(p.recent.iter().filter(|r| *r == "m5").count(), 1);
    }

    #[test]
    fn favorites_and_pins_toggle() {
        let mut p = Prefs::default();
        p.toggle_favorite("a/b");
        assert!(p.is_favorite("a/b"));
        p.toggle_favorite("a/b");
        assert!(!p.is_favorite("a/b"));
        p.toggle_pin("s1");
        p.toggle_pin("s2");
        assert_eq!(p.pin_slot("s2"), Some(2));
        p.toggle_pin("s1");
        assert_eq!(p.pin_slot("s2"), Some(1));
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("openw-prefs-{}", std::process::id()));
        let mut p = Prefs {
            theme: Some("tokyonight".into()),
            ..Default::default()
        };
        p.toggle_favorite("x/y");
        p.titles.insert("s1".into(), "Renamed".into());
        p.save(&dir).unwrap();
        assert_eq!(Prefs::load(&dir), p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_broken_file_is_defaults() {
        let dir = std::env::temp_dir().join(format!("openw-prefs-bad-{}", std::process::id()));
        assert_eq!(Prefs::load(&dir), Prefs::default());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), "{nope").unwrap();
        assert_eq!(Prefs::load(&dir), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
