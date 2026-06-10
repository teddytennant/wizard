//! Durable mission state for continuous sovereign mode.
//!
//! A `Mission` is the long-lived goal a perpetual agent works toward. It is
//! persisted to `<project_root>/.wizard/mission.toml` so the loop survives
//! restarts and binary self-replacement (deep `/evolve`). Marker files in the
//! same directory coordinate self-evolution hand-offs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Cap on the rolling progress log so the mission file stays bounded.
const MAX_NOTES: usize = 50;

/// Directory holding all wizard control state for a project.
pub fn control_dir(project_root: &Path) -> PathBuf {
    project_root.join(".wizard")
}

/// Path to the persisted mission file.
pub fn mission_path(project_root: &Path) -> PathBuf {
    control_dir(project_root).join("mission.toml")
}

/// Marker requesting the loop re-exec a freshly built binary (deep evolve).
pub fn reexec_marker(project_root: &Path) -> PathBuf {
    control_dir(project_root).join("evolve-reexec")
}

/// Marker requesting the loop reload state in place (shallow evolve).
pub fn reload_marker(project_root: &Path) -> PathBuf {
    control_dir(project_root).join("evolve-reload")
}

/// A durable, long-lived goal for a continuous sovereign agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    /// The standing goal the agent works toward.
    pub goal: String,
    /// When the mission was first created.
    pub created: DateTime<Utc>,
    /// When the mission was last updated.
    pub updated: DateTime<Utc>,
    /// Number of completed continuous cycles.
    pub cycles: u64,
    /// Rolling progress log (most recent last), capped at [`MAX_NOTES`].
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Mission {
    /// Create a fresh mission for `goal`, with no recorded cycles.
    pub fn new(goal: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            goal: goal.into(),
            created: now,
            updated: now,
            cycles: 0,
            notes: Vec::new(),
        }
    }

    /// Load the mission for `project_root`, returning `Ok(None)` if none exists.
    pub fn load(project_root: &Path) -> Result<Option<Self>> {
        let path = mission_path(project_root);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading mission file at {}", path.display()))?;
        let mission = toml::from_str(&raw)
            .with_context(|| format!("parsing mission file at {}", path.display()))?;
        Ok(Some(mission))
    }

    /// Persist the mission to `<project_root>/.wizard/mission.toml`.
    pub fn save(&self, project_root: &Path) -> Result<()> {
        let dir = control_dir(project_root);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating control dir at {}", dir.display()))?;
        let path = mission_path(project_root);
        let serialized = toml::to_string_pretty(self).context("serializing mission to TOML")?;
        std::fs::write(&path, serialized)
            .with_context(|| format!("writing mission file at {}", path.display()))?;
        Ok(())
    }

    /// Record completion of one cycle, optionally logging a progress note.
    ///
    /// The note is appended to [`Mission::notes`]; once the log exceeds
    /// [`MAX_NOTES`], the oldest entries are dropped from the front.
    pub fn record_cycle(&mut self, note: Option<String>) {
        self.cycles += 1;
        self.updated = Utc::now();
        if let Some(n) = note {
            self.notes.push(n);
            if self.notes.len() > MAX_NOTES {
                let excess = self.notes.len() - MAX_NOTES;
                self.notes.drain(0..excess);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "wizard-mission-test-{}-{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn round_trips() {
        let tmp = TempDir::new();
        let mut mission = Mission::new("ship the sovereign loop");
        mission.record_cycle(Some("first pass".to_string()));
        mission.save(&tmp.0).expect("save mission");

        let loaded = Mission::load(&tmp.0)
            .expect("load mission")
            .expect("mission present");
        assert_eq!(loaded.goal, "ship the sovereign loop");
        assert_eq!(loaded.cycles, 1);
        assert_eq!(loaded.notes, vec!["first pass".to_string()]);
    }

    #[test]
    fn load_missing_is_none() {
        let tmp = TempDir::new();
        let loaded = Mission::load(&tmp.0).expect("load from empty dir");
        assert!(loaded.is_none());
    }

    #[test]
    fn record_cycle_caps_notes() {
        let mut mission = Mission::new("endure");
        let total = MAX_NOTES + 10;
        for i in 0..total {
            mission.record_cycle(Some(format!("note-{i}")));
        }
        assert_eq!(mission.notes.len(), MAX_NOTES);
        assert_eq!(mission.cycles, total as u64);
        // The newest note is retained at the back.
        assert_eq!(
            mission.notes.last().expect("non-empty notes"),
            &format!("note-{}", total - 1)
        );
        // The oldest survivor is the expected front entry.
        assert_eq!(mission.notes.first().expect("non-empty notes"), "note-10");
    }
}
