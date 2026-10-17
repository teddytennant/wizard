// OWNER: bottom-pane
//! Composer history: shell-style Up/Down recall and Ctrl+R search (spec C.2.7), modelled on
//! Codex's `chat_composer_history.rs` with one synchronous store instead of Codex's async
//! persistent lookups.
//!
//! Entries from earlier sessions are loaded from `history.jsonl` under the codexw state
//! directory (never `~/.codex`) and restore as plain text; entries from this session keep their
//! placeholder elements and pasted payloads. Persistence is off until `enable_persistence` is
//! called, so tests and the headless harness never touch the disk.

use std::collections::HashSet;
use std::io::Write;
use std::ops::Range;
use std::path::PathBuf;

/// What a recall puts back in the draft.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HistoryEntry {
    pub text: String,
    /// Atomic placeholder ranges inside `text`.
    pub elements: Vec<Range<usize>>,
    pub pending_pastes: Vec<(String, String)>,
    /// `(placeholder, path)` of the images the draft carried, so a recalled draft still sends them.
    pub images: Vec<(String, std::path::PathBuf)>,
}

impl HistoryEntry {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchDirection {
    Older,
    Newer,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SearchResult {
    Found(HistoryEntry),
    /// The selection stays; there is nothing further in that direction.
    AtBoundary,
    NotFound,
}

#[derive(Debug, Default)]
pub struct ComposerHistory {
    /// Oldest first.
    entries: Vec<HistoryEntry>,
    cursor: Option<usize>,
    last_history_text: Option<String>,
    /// Newest-first indices of unique matches for the active query, and the selected one.
    matches: Vec<usize>,
    selected_match: Option<usize>,
    path: Option<PathBuf>,
}

/// `${CODEXW_STATE_DIR:-$XDG_STATE_HOME/codexw or ~/.local/state/codexw}`.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CODEXW_STATE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("codexw"));
    }
    let home = std::env::var_os("HOME").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(home).join(".local/state/codexw"))
}

const MAX_PERSISTED_ENTRIES: usize = 1000;

impl ComposerHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load earlier sessions from `<state dir>/history.jsonl` and keep appending to it.
    pub fn enable_persistence(&mut self) {
        let Some(dir) = state_dir() else {
            return;
        };
        let path = dir.join("history.jsonl");
        let mut loaded: Vec<HistoryEntry> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                        if !t.is_empty() {
                            loaded.push(HistoryEntry::plain(t));
                        }
                    }
                }
            }
        }
        if loaded.len() > MAX_PERSISTED_ENTRIES {
            loaded.drain(..loaded.len() - MAX_PERSISTED_ENTRIES);
        }
        loaded.append(&mut self.entries);
        self.entries = loaded;
        self.path = Some(path);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remember a submission. `persist_text` is what goes to disk (placeholders expanded).
    pub fn record(&mut self, entry: HistoryEntry, persist_text: &str) {
        if entry.text.is_empty() {
            return;
        }
        self.reset_navigation();
        if self.entries.last().is_some_and(|p| p == &entry) {
            return;
        }
        self.entries.push(entry);
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let line = serde_json::json!({ "text": persist_text }).to_string();
                let _ = writeln!(f, "{line}");
            }
        }
    }

    pub fn reset_navigation(&mut self) {
        self.cursor = None;
        self.last_history_text = None;
        self.reset_search();
    }

    pub fn reset_search(&mut self) {
        self.matches.clear();
        self.selected_match = None;
    }

    /// Up and Down walk history only from an empty draft, or from the text a recall just put
    /// there with the cursor at either end.
    pub fn should_handle_navigation(&self, text: &str, cursor: usize) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        if text.is_empty() {
            return true;
        }
        if cursor != 0 && cursor != text.len() {
            return false;
        }
        self.last_history_text.as_deref() == Some(text)
    }

    pub fn navigate_up(&mut self) -> Option<HistoryEntry> {
        self.reset_search();
        let total = self.entries.len();
        if total == 0 {
            return None;
        }
        let next = match self.cursor {
            None => total - 1,
            Some(0) => return None,
            Some(i) => i - 1,
        };
        self.recall(next)
    }

    /// Past the newest entry the composer clears: an empty entry comes back.
    pub fn navigate_down(&mut self) -> Option<HistoryEntry> {
        self.reset_search();
        let total = self.entries.len();
        let cur = self.cursor?;
        if cur + 1 >= total {
            self.cursor = None;
            self.last_history_text = None;
            return Some(HistoryEntry::default());
        }
        self.recall(cur + 1)
    }

    fn recall(&mut self, idx: usize) -> Option<HistoryEntry> {
        self.cursor = Some(idx);
        let e = self.entries.get(idx)?.clone();
        self.last_history_text = Some(e.text.clone());
        Some(e)
    }

    /// One Ctrl+R step. A new query (`restart`) begins at the newest match; stepping then moves
    /// through unique texts, older with Ctrl+R or Up, newer with Ctrl+S or Down.
    pub fn search(&mut self, query: &str, dir: SearchDirection, restart: bool) -> SearchResult {
        if restart || self.matches.is_empty() {
            let q = query.to_lowercase();
            let mut seen: HashSet<&str> = HashSet::new();
            self.matches = self
                .entries
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, e)| e.text.to_lowercase().contains(&q))
                .filter(|(_, e)| seen.insert(e.text.as_str()))
                .map(|(i, _)| i)
                .collect();
            self.selected_match = None;
        }
        if self.matches.is_empty() {
            return SearchResult::NotFound;
        }
        let next = match (self.selected_match, dir) {
            (None, _) => 0,
            (Some(i), SearchDirection::Older) => {
                if i + 1 >= self.matches.len() {
                    return SearchResult::AtBoundary;
                }
                i + 1
            }
            (Some(0), SearchDirection::Newer) => return SearchResult::AtBoundary,
            (Some(i), SearchDirection::Newer) => i - 1,
        };
        self.selected_match = Some(next);
        let idx = self.matches[next];
        self.cursor = Some(idx);
        let e = self.entries[idx].clone();
        self.last_history_text = Some(e.text.clone());
        SearchResult::Found(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(texts: &[&str]) -> ComposerHistory {
        let mut h = ComposerHistory::new();
        for t in texts {
            h.record(HistoryEntry::plain(*t), t);
        }
        h
    }

    #[test]
    fn up_walks_back_and_down_clears_past_the_newest() {
        let mut h = h(&["first", "second"]);
        assert!(h.should_handle_navigation("", 0));
        assert_eq!(h.navigate_up().unwrap().text, "second");
        assert!(h.should_handle_navigation("second", 6));
        assert!(h.should_handle_navigation("second", 0));
        assert!(!h.should_handle_navigation("second", 3));
        assert!(!h.should_handle_navigation("other", 0));
        assert_eq!(h.navigate_up().unwrap().text, "first");
        assert!(h.navigate_up().is_none());
        assert_eq!(h.navigate_down().unwrap().text, "second");
        assert_eq!(h.navigate_down().unwrap().text, "");
        assert!(h.navigate_down().is_none());
    }

    #[test]
    fn empty_history_never_navigates() {
        let h = ComposerHistory::new();
        assert!(!h.should_handle_navigation("", 0));
    }

    #[test]
    fn duplicate_of_the_last_entry_is_not_recorded() {
        let h = h(&["a", "a"]);
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn search_steps_through_unique_matches() {
        let mut h = h(&[
            "first prompt",
            "second prompt",
            "first again",
            "first prompt",
        ]);
        let SearchResult::Found(e) = h.search("first", SearchDirection::Older, true) else {
            panic!()
        };
        assert_eq!(e.text, "first prompt");
        let SearchResult::Found(e) = h.search("first", SearchDirection::Older, false) else {
            panic!()
        };
        assert_eq!(e.text, "first again");
        assert_eq!(
            h.search("first", SearchDirection::Older, false),
            SearchResult::AtBoundary
        );
        let SearchResult::Found(e) = h.search("first", SearchDirection::Newer, false) else {
            panic!()
        };
        assert_eq!(e.text, "first prompt");
        assert_eq!(
            h.search("zzz", SearchDirection::Older, true),
            SearchResult::NotFound
        );
    }

    #[test]
    fn persistence_round_trips_under_the_state_dir() {
        let dir = std::env::temp_dir().join(format!("cxw-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: tests in this module are the only readers of CODEXW_STATE_DIR in this process.
        unsafe { std::env::set_var("CODEXW_STATE_DIR", &dir) };
        let mut a = ComposerHistory::new();
        a.enable_persistence();
        a.record(HistoryEntry::plain("one\ntwo"), "one\ntwo");
        let mut b = ComposerHistory::new();
        b.enable_persistence();
        assert_eq!(b.navigate_up().unwrap().text, "one\ntwo");
        assert!(dir.join("history.jsonl").exists());
        unsafe { std::env::remove_var("CODEXW_STATE_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
