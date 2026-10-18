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
    /// What the loop was doing when it last stamped itself — "cycle 12:
    /// running turn", "cycle 12: waiting out circuit breaker", "held by
    /// operator pause".
    ///
    /// An operator watching a perpetual run from outside cannot tell "thinking
    /// hard about a big refactor" from "wedged on a request that will never
    /// answer": both look like a process using no CPU and writing no output.
    /// This field plus [`Mission::heartbeat`] make the difference legible —
    /// the phase says what it *believed* it was doing and the heartbeat says
    /// how long ago it believed it. Deliberately stamped only at phase
    /// boundaries by the loop itself, never by a background timer: a timer
    /// keeps ticking merrily while the agent hangs on a socket, which is
    /// precisely the state worth detecting.
    #[serde(default)]
    pub phase: Option<String>,
    /// When [`Mission::phase`] was last stamped. `None` on a mission written
    /// by a build that predates the field.
    #[serde(default)]
    pub heartbeat: Option<DateTime<Utc>>,
    /// Cycles that ended in a hard error or a tripped breaker since the last
    /// one that landed. Mirrors the loop's live counter so an operator reading
    /// `mission.toml` can see a run that is thrashing rather than progressing,
    /// and how close it is to `max_consecutive_failures`.
    #[serde(default)]
    pub consecutive_failures: u32,
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
            phase: None,
            heartbeat: None,
            consecutive_failures: 0,
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
        // Rename over the destination. `fs::write` truncates first, and a
        // kill in that window leaves a mission `load` will not parse, which
        // is how a continuous run stops being resumable.
        crate::platform::secrets::replace_file(&path, serialized.as_bytes())
            .with_context(|| format!("writing mission file at {}", path.display()))?;
        Ok(())
    }

    /// Record completion of one cycle, optionally logging a progress note.
    ///
    /// The note is appended to [`Mission::notes`]; once the log exceeds
    /// [`MAX_NOTES`], the oldest entries are dropped from the front.
    /// A cycle that reaches here landed, so it also clears the consecutive
    /// failure streak: the bound that ends a thrashing perpetual run counts
    /// *consecutive* bad cycles, and one good cycle proves the run is not
    /// stuck.
    pub fn record_cycle(&mut self, note: Option<String>) {
        self.cycles += 1;
        self.updated = Utc::now();
        self.consecutive_failures = 0;
        if let Some(n) = note {
            self.note(n);
        }
    }

    /// Record a cycle that ended badly — a hard error or a tripped circuit
    /// breaker — and return the new consecutive streak.
    ///
    /// Deliberately not a cycle: `cycles` is the count of work the mission
    /// actually advanced by, and inflating it with failures would make the
    /// number the continuation prompt quotes back to the model a lie.
    pub fn record_failure(&mut self, note: impl Into<String>) -> u32 {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.note(note);
        self.consecutive_failures
    }

    /// Forget the failure streak without recording a cycle — for a turn that
    /// ran out of steps rather than finishing. It did real work, so the run is
    /// demonstrably not wedged, but it has not completed anything to count.
    pub fn clear_failures(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Stamp what the loop is doing now and when, so an outside observer can
    /// tell a long turn from a hung one. Called at phase boundaries — cycle
    /// start, turn end, before every wait — and nowhere else; see
    /// [`Mission::phase`] for why there is no timer behind it.
    pub fn stamp(&mut self, phase: impl Into<String>) {
        let now = Utc::now();
        self.phase = Some(phase.into());
        self.heartbeat = Some(now);
        self.updated = now;
    }

    /// Append a progress note (bumping `updated`) without counting a cycle —
    /// e.g. a checkpoint rollback after a failed cycle. The log stays capped
    /// at [`MAX_NOTES`].
    pub fn note(&mut self, note: impl Into<String>) {
        self.updated = Utc::now();
        self.notes.push(note.into());
        if self.notes.len() > MAX_NOTES {
            let excess = self.notes.len() - MAX_NOTES;
            self.notes.drain(0..excess);
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

    #[test]
    fn failure_streak_counts_consecutively_and_any_landed_cycle_clears_it() {
        let mut mission = Mission::new("endure");
        assert_eq!(mission.record_failure("hard error: disk full"), 1);
        assert_eq!(mission.record_failure("circuit breaker"), 2);
        assert_eq!(mission.consecutive_failures, 2);
        // A failure is not progress: the cycle count must not move.
        assert_eq!(mission.cycles, 0);

        mission.record_cycle(Some("landed".to_string()));
        assert_eq!(
            mission.consecutive_failures, 0,
            "one good cycle proves the run is not stuck"
        );
        assert_eq!(mission.cycles, 1);

        assert_eq!(mission.record_failure("another"), 1);
        mission.clear_failures();
        assert_eq!(mission.consecutive_failures, 0);
        assert_eq!(mission.cycles, 1, "clear_failures is not a cycle");
    }

    #[test]
    fn stamp_records_phase_and_heartbeat_and_survives_a_round_trip() {
        let tmp = TempDir::new();
        let mut mission = Mission::new("endure");
        assert!(mission.phase.is_none());
        assert!(mission.heartbeat.is_none());

        mission.stamp("cycle 3: waiting out circuit breaker");
        mission.consecutive_failures = 2;
        let stamped = mission.heartbeat.expect("heartbeat set");
        assert_eq!(mission.updated, stamped, "a stamp is an update");
        mission.save(&tmp.0).expect("save mission");

        let loaded = Mission::load(&tmp.0)
            .expect("load mission")
            .expect("mission present");
        assert_eq!(
            loaded.phase.as_deref(),
            Some("cycle 3: waiting out circuit breaker")
        );
        assert_eq!(loaded.heartbeat, Some(stamped));
        assert_eq!(loaded.consecutive_failures, 2);
    }

    /// A mission written by a build that predates the liveness fields must
    /// still load. Losing it would restart a long-running mission from zero,
    /// which is the failure the corruption test below also guards.
    #[test]
    fn mission_without_liveness_fields_still_loads() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(control_dir(&tmp.0)).unwrap();
        let legacy = "goal = \"endure\"\n\
                      created = \"2024-01-01T00:00:00Z\"\n\
                      updated = \"2024-01-02T00:00:00Z\"\n\
                      cycles = 7\n\
                      notes = [\"a note\"]\n";
        std::fs::write(mission_path(&tmp.0), legacy).unwrap();

        let loaded = Mission::load(&tmp.0)
            .expect("legacy mission loads")
            .expect("mission present");
        assert_eq!(loaded.cycles, 7);
        assert!(loaded.phase.is_none());
        assert!(loaded.heartbeat.is_none());
        assert_eq!(loaded.consecutive_failures, 0);
    }

    #[test]
    fn corrupt_mission_file_is_an_error_not_a_fresh_mission() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(control_dir(&tmp.0)).unwrap();
        std::fs::write(mission_path(&tmp.0), "goal = ").unwrap();

        let err = Mission::load(&tmp.0).expect_err("corruption surfaces");
        assert!(
            format!("{err:#}").contains("parsing mission file"),
            "a broken mission must not silently restart the loop from zero: {err:#}"
        );
    }

    /// A save must not truncate the inode a reader already has open.
    ///
    /// `fs::write` opens that inode and truncates it, so a kill between the
    /// truncate and the write leaves `mission.toml` unparseable — and
    /// [`Mission::load`] then fails the whole run, which is how a continuous
    /// mission stops being resumable. Replacing the directory entry leaves
    /// the previous inode intact until the new file is complete.
    #[test]
    fn a_reader_holding_the_file_keeps_the_previous_mission() {
        use std::io::Read;

        let tmp = TempDir::new();
        let mission = Mission::new("the previous goal");
        mission.save(&tmp.0).expect("save the previous mission");

        let mut held = std::fs::File::open(mission_path(&tmp.0)).expect("hold the live file");

        let mut next = Mission::new("the next goal");
        next.notes.push("n".repeat(64 * 1024));
        next.save(&tmp.0).expect("save the next mission");

        let mut raw = String::new();
        held.read_to_string(&mut raw).expect("read the held inode");
        let parsed: Mission = toml::from_str(&raw).expect("held bytes are a whole mission");
        assert_eq!(
            parsed.goal, "the previous goal",
            "the inode a reader already had must stay the previous mission"
        );

        let loaded = Mission::load(&tmp.0)
            .expect("load")
            .expect("mission present");
        assert_eq!(loaded.goal, "the next goal");
    }

    /// A symlink planted at `mission.toml` names some other file. Following
    /// it truncates that file; replacing the link leaves the target alone.
    #[cfg(unix)]
    #[test]
    fn save_replaces_a_mission_symlink_instead_of_truncating_its_target() {
        let tmp = TempDir::new();
        let victim = tmp.0.join("victim.toml");
        std::fs::write(&victim, "goal = \"do not clobber\"\n").unwrap();
        std::fs::create_dir_all(control_dir(&tmp.0)).unwrap();
        std::os::unix::fs::symlink(&victim, mission_path(&tmp.0)).unwrap();

        Mission::new("the real mission").save(&tmp.0).expect("save");

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "goal = \"do not clobber\"\n",
            "mission.toml must not be followed into whatever it names"
        );
        let loaded = Mission::load(&tmp.0)
            .expect("load")
            .expect("mission present");
        assert_eq!(loaded.goal, "the real mission");
        assert!(
            mission_path(&tmp.0).is_file() && !mission_path(&tmp.0).is_symlink(),
            "the published path is a regular file"
        );
    }
}
