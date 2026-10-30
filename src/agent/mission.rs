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
    /// When the current goal was set. Separate from [`Mission::created`],
    /// which a changed goal keeps. `None` on a mission written by a build that
    /// predates the field; [`Mission::since`] falls back to `created` then.
    #[serde(default)]
    pub goal_since: Option<DateTime<Utc>>,
    /// Working turns the interactive `/goal` loop has spent on this goal.
    #[serde(default)]
    pub goal_rounds: u32,
    /// An interactive `/goal` loop is live on this goal. Stays set if the
    /// process dies, which is how the next session knows one was cut off.
    #[serde(default)]
    pub goal_running: bool,
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
            goal_since: Some(now),
            goal_rounds: 0,
            goal_running: false,
        }
    }

    /// When the current goal was set.
    pub fn since(&self) -> DateTime<Utc> {
        self.goal_since.unwrap_or(self.created)
    }

    /// Replace the goal, keeping the mission's history. The clock and the
    /// round count belong to the goal, so they restart with a different one and
    /// stand when the same text is set again.
    pub fn set_goal(&mut self, goal: impl Into<String>) {
        let goal = goal.into();
        if goal == self.goal {
            return;
        }
        self.note(format!("goal changed to: {goal}"));
        self.goal = goal;
        self.goal_since = Some(Utc::now());
        self.goal_rounds = 0;
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

    /// Delete the mission for `project_root`. `false` when there was none.
    pub fn clear(project_root: &Path) -> Result<bool> {
        let path = mission_path(project_root);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => {
                Err(err).with_context(|| format!("removing mission file at {}", path.display()))
            }
        }
    }

    /// Persist the mission to `<project_root>/.wizard/mission.toml`.
    ///
    /// Written beside the target and renamed over it, so a crash mid-write
    /// leaves the old mission rather than a truncated file that fails to parse.
    pub fn save(&self, project_root: &Path) -> Result<()> {
        let dir = control_dir(project_root);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating control dir at {}", dir.display()))?;
        let path = mission_path(project_root);
        let serialized = toml::to_string_pretty(self).context("serializing mission to TOML")?;
        let staged = dir.join(format!(".mission.toml.{}", std::process::id()));
        std::fs::write(&staged, serialized)
            .with_context(|| format!("writing mission file at {}", staged.display()))?;
        std::fs::rename(&staged, &path)
            .with_context(|| format!("replacing mission file at {}", path.display()))?;
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

/// How long `since` was, to the largest whole unit: `42s`, `5m`, `2h`, `3d`.
/// A clock that moved backwards reads `0s`.
pub fn age_label(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - since).num_seconds().max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
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
    fn age_reads_in_its_largest_whole_unit() {
        let now = Utc::now();
        let age = |secs| age_label(now - chrono::Duration::seconds(secs), now);
        assert_eq!(age(0), "0s");
        assert_eq!(age(59), "59s");
        assert_eq!(age(60), "1m");
        assert_eq!(age(3_599), "59m");
        assert_eq!(age(3_600), "1h");
        assert_eq!(age(86_399), "23h");
        assert_eq!(age(86_400), "1d");
        assert_eq!(age(3 * 86_400 + 5), "3d");
        assert_eq!(age(-30), "0s", "a clock that moved back is not negative");
    }

    #[test]
    fn a_changed_goal_restarts_its_clock_and_the_same_goal_does_not() {
        let mut mission = Mission::new("ship it");
        let long_ago = Utc::now() - chrono::Duration::days(3);
        mission.goal_since = Some(long_ago);
        mission.goal_rounds = 7;

        mission.set_goal("ship it");
        assert_eq!(mission.since(), long_ago);
        assert_eq!(mission.goal_rounds, 7);
        assert!(mission.notes.is_empty());

        mission.set_goal("cut a patch release");
        assert!(mission.since() > long_ago);
        assert_eq!(mission.goal_rounds, 0);
        assert_eq!(mission.notes, vec!["goal changed to: cut a patch release"]);
    }

    #[test]
    fn a_mission_from_before_the_goal_fields_loads_with_its_creation_time() {
        let tmp = TempDir::new();
        let path = mission_path(&tmp.0);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "goal = \"old\"\ncreated = \"2026-01-02T03:04:05Z\"\n\
             updated = \"2026-01-02T03:04:05Z\"\ncycles = 2\n",
        )
        .unwrap();
        let loaded = Mission::load(&tmp.0).unwrap().unwrap();
        assert_eq!(loaded.since(), loaded.created);
        assert!(!loaded.goal_running);
    }

    #[test]
    fn clear_removes_the_mission_and_says_whether_there_was_one() {
        let tmp = TempDir::new();
        assert!(!Mission::clear(&tmp.0).unwrap());
        Mission::new("ship it").save(&tmp.0).unwrap();
        assert!(Mission::clear(&tmp.0).unwrap());
        assert!(Mission::load(&tmp.0).unwrap().is_none());
    }

    #[test]
    fn save_leaves_nothing_but_the_mission_behind() {
        let tmp = TempDir::new();
        Mission::new("ship it").save(&tmp.0).unwrap();
        let names: Vec<_> = std::fs::read_dir(control_dir(&tmp.0))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["mission.toml"]);
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
}
