//! Per-file copy-on-write checkpoints under `<project>/.wizard/checkpoints/`.
//!
//! Before an `Edit`-class tool runs, the dispatcher (and the subagent loop)
//! snapshot the target file's current content. `/rewind` in the TUI and the
//! perpetual `rollback_failed_cycles` option restore those before-states.
//!
//! This is deliberately *not* shadow-git: the repo Wizard runs in may be
//! edited concurrently by other processes, so checkpoints only ever touch
//! files Wizard itself modified. Layout:
//!
//! ```text
//! .wizard/checkpoints/index.jsonl   one SnapshotRecord per line
//! .wizard/checkpoints/<turn>/<n>.snap   copied file contents
//! ```
//!
//! Every operation is best-effort from the agent's point of view: a failed
//! snapshot must never fail the tool call (see [`snapshot_edit_target`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::{ToolAccess, ToolContext, registry::ToolRegistry};

/// One line of `index.jsonl`: a before-state captured for one file in one
/// turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRecord {
    /// Turn the snapshot belongs to.
    pub turn: u64,
    /// Tool whose call triggered the snapshot (`write_file`, `edit_file`).
    pub tool: String,
    /// The real target file path (absolute).
    pub path: PathBuf,
    /// Snap file path relative to the checkpoints root
    /// (`<turn>/<n>.snap`); empty when `existed_before` is false.
    pub snap: String,
    /// False when the tool was about to create a new file — rewinding then
    /// deletes it instead of restoring content.
    pub existed_before: bool,
}

/// A turn and the files it snapshotted (for the `/rewind` picker).
#[derive(Debug, Clone)]
pub struct TurnFiles {
    pub turn: u64,
    pub files: Vec<PathBuf>,
}

/// Copy-on-write snapshot store for one project. Cheap to share behind an
/// `Arc`; all methods take `&self`.
#[derive(Debug)]
pub struct CheckpointStore {
    /// `<project>/.wizard/checkpoints`.
    root: PathBuf,
    /// Number of most recent turns [`gc`](Self::gc) keeps.
    keep_turns: usize,
    /// The turn currently executing; bumped by [`begin_turn`](Self::begin_turn).
    current_turn: AtomicU64,
    /// `(turn, path)` pairs already snapshotted — the first snapshot of a
    /// path within a turn wins (it is the turn's before-state). Also
    /// serializes index file writes.
    seen: Mutex<HashSet<(u64, PathBuf)>>,
}

impl CheckpointStore {
    /// Open (or lazily create) the store for `project_root`. Never fails:
    /// a corrupt or missing index just starts the turn counter at the
    /// highest readable turn (or zero).
    pub fn open(project_root: &Path, keep_turns: usize) -> Self {
        let root = project_root.join(".wizard").join("checkpoints");
        let max_turn = read_index(&root.join("index.jsonl"))
            .iter()
            .map(|record| record.turn)
            .max()
            .unwrap_or(0);
        Self {
            root,
            keep_turns,
            current_turn: AtomicU64::new(max_turn),
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// Number the next turn after `turn` at least.
    pub fn continue_after(&self, turn: u64) {
        self.current_turn.fetch_max(turn, Ordering::SeqCst);
    }

    /// The turn currently executing (0 before the first
    /// [`begin_turn`](Self::begin_turn)).
    pub fn current_turn(&self) -> u64 {
        self.current_turn.load(Ordering::SeqCst)
    }

    /// Start a new turn and return its id. Turn ids increase monotonically
    /// across sessions of a project (seeded from the persisted index).
    pub fn begin_turn(&self) -> u64 {
        let turn = self.current_turn.fetch_add(1, Ordering::SeqCst) + 1;
        // Dedup keys carry the turn id, so old entries are dead weight only.
        self.seen.lock().unwrap().clear();
        turn
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.jsonl")
    }

    /// Snapshot `target`'s current content as `turn`'s before-state. The
    /// first snapshot of a path within a turn wins; later calls are skipped
    /// (returns `Ok(false)`). A missing target records
    /// `existed_before = false` so rewind deletes the file.
    pub fn snapshot(&self, turn: u64, tool: &str, target: &Path) -> Result<bool> {
        let mut seen = self.seen.lock().unwrap();
        if !seen.insert((turn, target.to_path_buf())) {
            return Ok(false);
        }
        let record = if target.exists() {
            let turn_dir = self.root.join(turn.to_string());
            std::fs::create_dir_all(&turn_dir)
                .with_context(|| format!("creating {}", turn_dir.display()))?;
            let n = std::fs::read_dir(&turn_dir)
                .map(|dir| dir.count())
                .unwrap_or(0);
            let snap = format!("{turn}/{n}.snap");
            std::fs::copy(target, self.root.join(&snap))
                .with_context(|| format!("snapshotting {} to {snap}", target.display()))?;
            SnapshotRecord {
                turn,
                tool: tool.to_string(),
                path: target.to_path_buf(),
                snap,
                existed_before: true,
            }
        } else {
            SnapshotRecord {
                turn,
                tool: tool.to_string(),
                path: target.to_path_buf(),
                snap: String::new(),
                existed_before: false,
            }
        };
        self.append_record(&record)?;
        Ok(true)
    }

    /// Restore the before-states of `turn` and every later turn (the
    /// `/rewind` and cycle-rollback operation): the earliest snapshot of
    /// each path wins, so files return to their state just before `turn`
    /// started. Restored turns are pruned from the index. Returns the
    /// restored file paths.
    pub fn restore_turns_from(&self, turn: u64) -> Result<Vec<PathBuf>> {
        self.restore_where(|t| t >= turn)
    }

    fn restore_where(&self, matches: impl Fn(u64) -> bool) -> Result<Vec<PathBuf>> {
        let mut seen = self.seen.lock().unwrap();
        let records = read_index(&self.index_path());

        // Index lines are appended in execution order, so the first matching
        // record per path is that path's earliest (true) before-state.
        let mut chosen: HashMap<&Path, &SnapshotRecord> = HashMap::new();
        for record in records.iter().filter(|record| matches(record.turn)) {
            chosen.entry(record.path.as_path()).or_insert(record);
        }

        // Check every blob before touching a single file.
        //
        // This loop copies as it goes and `?`s out on the first failure, which
        // for a *destructive* operation is the wrong shape: a rewind that
        // reverts three files, finds the fourth snapshot missing and aborts
        // leaves the tree in a state that is neither before nor after, prunes
        // nothing (the pruning is below this loop), and does exactly the same
        // partial thing on a retry.
        //
        // A record without its blob needs outside interference — `snapshot`
        // copies the blob before it appends the record, and `gc` rewrites the
        // index before it removes the directory, so neither can produce one.
        // But checkpoints live under a `.wizard/` directory that people clean,
        // back up and restore, and "somebody deleted a cache directory" should
        // cost you the ability to rewind, not the contents of your files.
        let missing: Vec<String> = chosen
            .values()
            .filter(|record| record.existed_before)
            .filter(|record| !self.root.join(&record.snap).exists())
            .map(|record| format!("{} (turn {})", record.path.display(), record.turn))
            .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "cannot rewind: {} snapshot(s) are missing from {}, so the restore would be \
                 partial and would leave your files in a state that is neither before nor \
                 after. Nothing has been changed. Missing: {}",
                missing.len(),
                self.root.display(),
                missing.join(", ")
            );
        }

        let mut restored: Vec<PathBuf> = Vec::new();
        for (path, record) in &chosen {
            if record.existed_before {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::copy(self.root.join(&record.snap), path)
                    .with_context(|| format!("restoring {}", path.display()))?;
            } else {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(err).with_context(|| format!("deleting {}", path.display()));
                    }
                }
            }
            restored.push(path.to_path_buf());
        }
        restored.sort();

        // Prune the rewound turns: their history no longer exists.
        let dropped: BTreeSet<u64> = records
            .iter()
            .filter(|record| matches(record.turn))
            .map(|record| record.turn)
            .collect();
        if !dropped.is_empty() {
            let keep: Vec<&SnapshotRecord> = records
                .iter()
                .filter(|record| !matches(record.turn))
                .collect();
            self.rewrite_index(&keep)?;
            for turn in &dropped {
                let _ = std::fs::remove_dir_all(self.root.join(turn.to_string()));
            }
            seen.retain(|(turn, _)| !matches(*turn));
        }
        Ok(restored)
    }

    /// Drop snapshots of all but the most recent `keep_turns` turns.
    /// Returns the number of turns dropped. Cheap when there is nothing to
    /// do (one index read).
    pub fn gc(&self) -> Result<usize> {
        let _seen = self.seen.lock().unwrap();
        let records = read_index(&self.index_path());
        let turns: BTreeSet<u64> = records.iter().map(|record| record.turn).collect();
        if turns.len() <= self.keep_turns {
            return Ok(0);
        }
        let cutoff = turns
            .iter()
            .rev()
            .nth(self.keep_turns.saturating_sub(1))
            .copied()
            .filter(|_| self.keep_turns > 0);
        let dropped: Vec<u64> = match cutoff {
            Some(cutoff) => turns
                .iter()
                .copied()
                .filter(|turn| *turn < cutoff)
                .collect(),
            None => turns.iter().copied().collect(),
        };
        let keep: Vec<&SnapshotRecord> = records
            .iter()
            .filter(|record| !dropped.contains(&record.turn))
            .collect();
        self.rewrite_index(&keep)?;
        for turn in &dropped {
            let _ = std::fs::remove_dir_all(self.root.join(turn.to_string()));
        }
        Ok(dropped.len())
    }

    /// The most recent `limit` snapshotted turns and the files each touched,
    /// newest first (for the `/rewind` picker).
    pub fn recent_turns(&self, limit: usize) -> Vec<TurnFiles> {
        let records = read_index(&self.index_path());
        let mut by_turn: BTreeMap<u64, Vec<PathBuf>> = BTreeMap::new();
        for record in records {
            by_turn.entry(record.turn).or_default().push(record.path);
        }
        by_turn
            .into_iter()
            .rev()
            .take(limit)
            .map(|(turn, files)| TurnFiles { turn, files })
            .collect()
    }

    fn append_record(&self, record: &SnapshotRecord) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        let path = self.index_path();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let line = serde_json::to_string(record).context("serializing snapshot record")?;
        writeln!(file, "{line}").with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn rewrite_index(&self, records: &[&SnapshotRecord]) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        let mut text = String::new();
        for record in records {
            text.push_str(&serde_json::to_string(record).context("serializing snapshot record")?);
            text.push('\n');
        }
        let path = self.index_path();
        let tmp = self.root.join("index.jsonl.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

/// Load all readable records of `index` (corrupt lines are skipped).
fn read_index(index: &Path) -> Vec<SnapshotRecord> {
    let Ok(file) = std::fs::File::open(index) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SnapshotRecord>(&line) {
            Ok(record) => records.push(record),
            Err(err) => tracing::warn!("skipping corrupt checkpoint index line: {err}"),
        }
    }
    records
}

/// Checkpoint seam shared by the dispatcher and the subagent loop: when
/// `name` is an `Edit`-class tool, snapshot its target file (resolved from
/// the `path` argument, or every file an `apply_patch` names, after pre-hooks
/// have had their chance to rewrite it) under the store's current turn. Never
/// fails the tool call — snapshot errors are logged and execution proceeds.
pub fn snapshot_edit_target(registry: &ToolRegistry, name: &str, args: &Value, ctx: &ToolContext) {
    let Some(store) = &ctx.checkpoints else {
        return;
    };
    if registry
        .get(name)
        .is_none_or(|tool| tool.access() != ToolAccess::Edit)
    {
        return;
    }
    // `path` is Wizard's argument; `file_path` and `notebook_path` are the
    // Claude Code names the `compat` tools take.
    let named = ["path", "file_path", "notebook_path"]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str));
    let paths = match named {
        Some(path) => vec![path.to_string()],
        None => crate::tools::patch::targets(args),
    };
    for path in paths {
        let target = crate::tools::resolve_path(ctx, &path);
        if let Err(err) = store.snapshot(store.current_turn(), name, &target) {
            tracing::warn!(
                "checkpoint snapshot of {} failed: {err:#}",
                target.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("wizard-ckpt-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, content: &str) {
        std::fs::write(path, content).expect("write file");
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read file")
    }

    #[test]
    fn snapshot_restore_roundtrip() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 50);
        let file = tmp.0.join("notes.txt");
        write(&file, "before");

        let turn = store.begin_turn();
        assert!(store.snapshot(turn, "write_file", &file).unwrap());
        write(&file, "after");

        let restored = store.restore_turns_from(turn).unwrap();
        assert_eq!(restored, vec![file.clone()]);
        assert_eq!(read(&file), "before");
    }

    #[test]
    fn new_file_is_deleted_on_restore() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 50);
        let file = tmp.0.join("created.txt");

        let turn = store.begin_turn();
        assert!(store.snapshot(turn, "write_file", &file).unwrap());
        write(&file, "fresh content");

        store.restore_turns_from(turn).unwrap();
        assert!(!file.exists(), "rewind deletes a file that did not exist");
    }

    #[test]
    fn a_patch_snapshots_every_file_it_names() {
        let tmp = TempDir::new();
        let store = std::sync::Arc::new(CheckpointStore::open(&tmp.0, 50));
        let ctx = ToolContext::new(&tmp.0).with_checkpoints(store.clone());
        write(&tmp.0.join("a.txt"), "a1");
        write(&tmp.0.join("b.txt"), "b1");

        let turn = store.begin_turn();
        snapshot_edit_target(
            &ToolRegistry::with_native_tools(),
            "apply_patch",
            &serde_json::json!({ "patch": "*** Update File: a.txt\n-a1\n+a2\n*** Update File: b.txt\n-b1\n+b2\n*** Add File: c.txt\n+c\n" }),
            &ctx,
        );
        for name in ["a.txt", "b.txt", "c.txt"] {
            write(&tmp.0.join(name), "changed");
        }

        store.restore_turns_from(turn).unwrap();
        assert_eq!(read(&tmp.0.join("a.txt")), "a1");
        assert_eq!(read(&tmp.0.join("b.txt")), "b1");
        assert!(!tmp.0.join("c.txt").exists());
    }

    #[test]
    fn duplicate_snapshot_within_a_turn_is_skipped() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 50);
        let file = tmp.0.join("notes.txt");
        write(&file, "v1");

        let turn = store.begin_turn();
        assert!(store.snapshot(turn, "write_file", &file).unwrap());
        write(&file, "v2");
        // Second write of the same path within the turn: first wins.
        assert!(!store.snapshot(turn, "edit_file", &file).unwrap());
        write(&file, "v3");

        store.restore_turns_from(turn).unwrap();
        assert_eq!(
            read(&file),
            "v1",
            "the turn's before-state is the first snapshot"
        );
    }

    /// A missing snapshot blob aborts the rewind before anything is written.
    ///
    /// The shape this guards: `restore_where` copies file by file and returns
    /// on the first error, so without a pre-flight check a rewind could revert
    /// some files, fail on the next, and leave the tree in a state that is
    /// neither before nor after — with the index unpruned, so a retry repeats
    /// the same partial write. For the one operation in this program that
    /// overwrites the user's working files, all-or-nothing is worth a stat per
    /// snapshot.
    #[test]
    fn a_missing_snapshot_aborts_the_rewind_without_touching_anything() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 50);
        let kept = tmp.0.join("kept.txt");
        let orphan = tmp.0.join("orphan.txt");
        write(&kept, "before");
        write(&orphan, "before");

        let turn = store.begin_turn();
        store.snapshot(turn, "write_file", &kept).unwrap();
        store.snapshot(turn, "write_file", &orphan).unwrap();
        write(&kept, "after");
        write(&orphan, "after");

        // Something outside Wizard removed the checkpoint blobs — a cleaned
        // cache directory, a restore from backup, a full disk.
        std::fs::remove_dir_all(
            tmp.0
                .join(".wizard")
                .join("checkpoints")
                .join(turn.to_string()),
        )
        .expect("remove the turn's snapshots");

        let err = store
            .restore_turns_from(turn)
            .expect_err("a rewind with no snapshots to restore from must refuse");
        let text = format!("{err:#}");
        assert!(text.contains("Nothing has been changed"), "{text}");
        assert!(
            text.contains("orphan.txt") || text.contains("kept.txt"),
            "{text}"
        );

        // The point of the check: neither file moved.
        assert_eq!(read(&kept), "after", "no file may be touched: {text}");
        assert_eq!(read(&orphan), "after", "no file may be touched: {text}");
    }

    #[test]
    fn restore_turns_from_earliest_snapshot_wins_across_turns() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 50);
        let file = tmp.0.join("notes.txt");
        write(&file, "A");

        let turn1 = store.begin_turn();
        store.snapshot(turn1, "write_file", &file).unwrap();
        write(&file, "B");
        let turn2 = store.begin_turn();
        store.snapshot(turn2, "write_file", &file).unwrap();
        write(&file, "C");

        let restored = store.restore_turns_from(turn1).unwrap();
        assert_eq!(restored, vec![file.clone()]);
        assert_eq!(read(&file), "A", "earliest before-state wins across turns");
    }

    #[test]
    fn restore_turns_from_a_later_turn_keeps_earlier_state() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 50);
        let file = tmp.0.join("notes.txt");
        write(&file, "A");

        let turn1 = store.begin_turn();
        store.snapshot(turn1, "write_file", &file).unwrap();
        write(&file, "B");
        let turn2 = store.begin_turn();
        store.snapshot(turn2, "write_file", &file).unwrap();
        write(&file, "C");

        store.restore_turns_from(turn2).unwrap();
        assert_eq!(read(&file), "B", "only the later turn is rewound");
        // Turn 1's snapshot survives the prune and is still restorable.
        store.restore_turns_from(turn1).unwrap();
        assert_eq!(read(&file), "A");
    }

    #[test]
    fn restore_prunes_records_and_snap_dirs() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 50);
        let file = tmp.0.join("notes.txt");
        write(&file, "A");

        let turn = store.begin_turn();
        store.snapshot(turn, "write_file", &file).unwrap();
        store.restore_turns_from(turn).unwrap();

        assert!(store.recent_turns(10).is_empty(), "rewound turn is pruned");
        assert!(
            !tmp.0
                .join(".wizard/checkpoints")
                .join(turn.to_string())
                .exists(),
            "snap dir is removed"
        );
        // The same path can be snapshotted again in the same turn after a
        // rewind (the dedup entry was pruned with the records).
        assert!(store.snapshot(turn, "write_file", &file).unwrap());
    }

    /// `keep_turns = 0` collects everything, and that is on purpose.
    ///
    /// Pinned because it is the one value a reader is likely to take for
    /// "unlimited", and because `gc` reaches it through a `saturating_sub` and
    /// a `filter` rather than an explicit branch — an easy thing to "tidy" into
    /// keeping one turn, or into keeping all of them. Both config and
    /// `docs/checkpoints.md` say what it means; this makes the code agree.
    #[test]
    fn keeping_zero_turns_collects_every_snapshot() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 0);
        let file = tmp.0.join("notes.txt");
        write(&file, "A");

        let turn = store.begin_turn();
        store.snapshot(turn, "write_file", &file).unwrap();
        write(&file, "B");

        assert_eq!(store.gc().unwrap(), 1, "the only turn is collected");
        assert_eq!(
            store.recent_turns(10).len(),
            0,
            "nothing is left to rewind to"
        );
    }

    #[test]
    fn gc_keeps_the_last_n_turns() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 2);
        let file = tmp.0.join("notes.txt");
        for i in 0..5 {
            write(&file, &format!("v{i}"));
            let turn = store.begin_turn();
            store.snapshot(turn, "write_file", &file).unwrap();
        }

        assert_eq!(store.gc().unwrap(), 3);
        let recent = store.recent_turns(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].turn, 5);
        assert_eq!(recent[1].turn, 4);
        for turn in 1..=3u64 {
            assert!(
                !tmp.0
                    .join(".wizard/checkpoints")
                    .join(turn.to_string())
                    .exists(),
                "gc removed turn {turn}'s snap dir"
            );
        }
        // Nothing further to drop.
        assert_eq!(store.gc().unwrap(), 0);
    }

    #[test]
    fn recent_turns_lists_files_newest_first() {
        let tmp = TempDir::new();
        let store = CheckpointStore::open(&tmp.0, 50);
        let a = tmp.0.join("a.txt");
        let b = tmp.0.join("b.txt");
        write(&a, "a");

        let turn1 = store.begin_turn();
        store.snapshot(turn1, "write_file", &a).unwrap();
        let turn2 = store.begin_turn();
        store.snapshot(turn2, "write_file", &a).unwrap();
        store.snapshot(turn2, "write_file", &b).unwrap();

        let recent = store.recent_turns(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].turn, turn2);
        assert_eq!(recent[0].files, vec![a.clone(), b.clone()]);
        assert_eq!(recent[1].turn, turn1);
    }

    #[test]
    fn open_resumes_the_turn_counter_from_the_index() {
        let tmp = TempDir::new();
        let file = tmp.0.join("notes.txt");
        write(&file, "x");
        {
            let store = CheckpointStore::open(&tmp.0, 50);
            let turn = store.begin_turn();
            assert_eq!(turn, 1);
            store.snapshot(turn, "write_file", &file).unwrap();
        }
        let store = CheckpointStore::open(&tmp.0, 50);
        assert_eq!(store.current_turn(), 1);
        assert_eq!(store.begin_turn(), 2, "turn ids continue across sessions");
    }

    #[test]
    fn corrupt_index_lines_are_skipped() {
        let tmp = TempDir::new();
        let dir = tmp.0.join(".wizard/checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.jsonl"), "{not json\n\n").unwrap();
        let store = CheckpointStore::open(&tmp.0, 50);
        assert_eq!(store.current_turn(), 0);
        assert!(store.recent_turns(10).is_empty());
    }
}
