// OWNER: input
//! The `@` file index: listed once in the background and in chunks, searched on a worker thread.
//!
//! A repository can have a million paths, so nothing here runs on the UI thread past a size
//! where scoring every path would blow the 16 ms frame budget. The listing streams (the popup
//! works on what has arrived and later keys see more), and a query over a big index is
//! answered by the worker, newest request winning, with the old rows kept on screen until the
//! new ones land.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

use crate::ui::popup::PItem;

/// At or under this many entries a query is scored inline, in well under a frame.
pub const SYNC_LIMIT: usize = 20_000;
/// Most entries the index holds. A home directory of 750k files fits; this only stops a
/// runaway walk from eating memory.
const MAX_ENTRIES: usize = 4_000_000;
/// Entries per chunk sent to the app while listing.
const CHUNK: usize = 8192;

/// What a hit in the file name is worth over the same hit in a directory.
const NAME_BONUS: i32 = 40;

/// Byte offset where the last path component starts.
fn name_start(path: &str) -> usize {
    path.trim_end_matches('/').rfind('/').map_or(0, |i| i + 1)
}

/// Score `q` against a path. A hit in the file name beats the same hit in a directory, so
/// `lib` finds `src/lib.rs` before `library/notes.txt`.
pub fn path_match(q: &str, path: &str) -> Option<(i32, Vec<usize>)> {
    let full = tuikit::fuzzy::score(q, path);
    let base_at = path
        .trim_end_matches('/')
        .rfind('/')
        .map_or(0, |i| path[..=i].chars().count());
    let base: String = path.chars().skip(base_at).collect();
    let in_base = tuikit::fuzzy::score(q, &base).map(|m| {
        (
            m.score + NAME_BONUS,
            m.indices
                .into_iter()
                .map(|i| i + base_at)
                .collect::<Vec<_>>(),
        )
    });
    match (full, in_base) {
        (Some(f), Some(b)) if b.0 >= f.score => Some(b),
        (Some(f), _) => Some((f.score, f.indices)),
        (None, b) => b,
    }
}

/// Cheap reject before the scorer allocates: are the query's characters in the path, in
/// order? Same case rule as the scorer (all lowercase ignores case).
fn subsequence(q: &[char], case_sensitive: bool, path: &str) -> bool {
    let mut i = 0;
    for c in path.chars() {
        let c = if case_sensitive {
            c
        } else {
            c.to_lowercase().next().unwrap_or(c)
        };
        if c == q[i] {
            i += 1;
            if i == q.len() {
                return true;
            }
        }
    }
    false
}

fn depth(path: &str) -> usize {
    path.trim_end_matches('/').matches('/').count()
}

/// Files for `@q`: fuzzy on the path with the file name weighted, shallow files first when
/// there is no query yet. `cancelled` is polled every few thousand paths; a cancelled search
/// returns `None`.
pub fn search(
    files: &[String],
    q: &str,
    limit: usize,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<PItem>> {
    let item = |f: &String, idx: Vec<usize>| PItem {
        label: f.clone(),
        insert: f.clone(),
        indices: idx,
        ..PItem::default()
    };
    // A folder typed out in full is done: what is inside it is what comes next.
    let skip = |f: &String| f.ends_with('/') && f.as_str() == q;
    if q.is_empty() {
        // The `limit` shallowest, in listing order among equals.
        let mut best: Vec<(usize, usize)> = Vec::with_capacity(limit + 1);
        for (i, f) in files.iter().enumerate() {
            if i % 4096 == 0 && cancelled() {
                return None;
            }
            let d = depth(f);
            if best.len() == limit && best.last().is_some_and(|&(bd, _)| d >= bd) {
                continue;
            }
            let at = best.partition_point(|&(bd, _)| bd <= d);
            best.insert(at, (d, i));
            best.truncate(limit);
        }
        return Some(
            best.into_iter()
                .map(|(_, i)| &files[i])
                .filter(|f| !skip(f))
                .map(|f| item(f, Vec::new()))
                .collect(),
        );
    }
    let qc: Vec<char> = q.chars().collect();
    let cs = qc.iter().any(|c| c.is_uppercase());
    let qf: Vec<char> = qc
        .iter()
        .map(|&c| {
            if cs {
                c
            } else {
                c.to_lowercase().next().unwrap_or(c)
            }
        })
        .collect();
    // (score, length, position): best first means high score, then short, then listing order.
    let mut top: Vec<(i32, usize, usize)> = Vec::new();
    let pq = tuikit::fuzzy::Prepared::new(q);
    for (i, f) in files.iter().enumerate() {
        if i % 4096 == 0 && cancelled() {
            return None;
        }
        if skip(f) || !subsequence(&qf, cs, f) {
            continue;
        }
        // The score alone, with no allocation; only the rows that are shown pay for positions.
        if let Some(s) = pq.score_path_only(f, name_start(f), NAME_BONUS) {
            top.push((s, f.len(), i));
            if top.len() >= limit * 8 + 64 {
                top.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
                top.truncate(limit);
            }
        }
    }
    top.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    top.truncate(limit);
    Some(
        top.into_iter()
            .map(|(_, _, i)| {
                let idx = path_match(q, &files[i]).map(|m| m.1).unwrap_or_default();
                item(&files[i], idx)
            })
            .collect(),
    )
}

// ---- listing -----------------------------------------------------------------------------

/// Directories as `a/b/` entries so `@src` can complete to a folder; `seen` is shared across
/// chunks so each folder is listed once.
fn with_dirs(files: Vec<String>, seen: &mut HashSet<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(files.len() + files.len() / 8);
    for f in files {
        let mut end = 0;
        while let Some(i) = f[end..].find('/') {
            end += i + 1;
            if !seen.contains(&f[..end]) {
                seen.insert(f[..end].to_string());
                out.push(f[..end].to_string());
            }
        }
        out.push(f);
    }
    out
}

/// Tracked and untracked, not ignored: `git ls-files -z` streamed to `sink` in chunks. `false`
/// when git is not there or this is not a repository, so the caller can walk instead.
pub fn stream_git(cwd: &Path, sink: &mut dyn FnMut(Vec<String>)) -> bool {
    let Ok(mut child) = tuikit::git::command(cwd)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut out) = child.stdout.take() else {
        return false;
    };
    let mut seen = HashSet::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    let mut batch: Vec<String> = Vec::new();
    let mut total = 0usize;
    loop {
        let n = match out.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        carry.extend_from_slice(&buf[..n]);
        let mut start = 0;
        while let Some(z) = carry[start..].iter().position(|b| *b == 0) {
            let name = &carry[start..start + z];
            if !name.is_empty() {
                batch.push(String::from_utf8_lossy(name).into_owned());
            }
            start += z + 1;
        }
        carry.drain(..start);
        if batch.len() >= CHUNK {
            total += batch.len();
            sink(with_dirs(std::mem::take(&mut batch), &mut seen));
            if total >= MAX_ENTRIES {
                let _ = child.kill();
                break;
            }
        }
    }
    if !carry.is_empty() {
        batch.push(String::from_utf8_lossy(&carry).into_owned());
    }
    if !batch.is_empty() {
        sink(with_dirs(batch, &mut seen));
    }
    // An empty repository is a success with nothing to list; "not a repository" is a failure.
    child.wait().is_ok_and(|s| s.success()) || total >= MAX_ENTRIES
}

/// Without git: every file under `dir` except dot files and what the root `.gitignore` names.
/// Symlinked directories are not entered (a link to `/` would list the machine), and symlinked
/// files are listed as the files they are.
pub fn walk(root: &Path) -> Vec<String> {
    let ignore = read_ignore(root);
    let mut out = Vec::new();
    walk_with(root, root, 0, &ignore, &mut out);
    out
}

fn read_ignore(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(".gitignore"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
        .map(|l| l.trim_matches('/').to_string())
        .collect()
}

/// A name or `*.ext` pattern from the root `.gitignore`. Anything fancier is not honoured;
/// git does the real work when it is there.
fn ignored(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| match p.strip_prefix("*.") {
        Some(ext) => name.ends_with(&format!(".{ext}")),
        None => p == name,
    })
}

fn walk_with(root: &Path, dir: &Path, depth: usize, ignore: &[String], out: &mut Vec<String>) {
    if depth > 8 || out.len() >= MAX_ENTRIES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.')
            || name == "target"
            || name == "node_modules"
            || (depth == 0 && ignored(&name, ignore))
        {
            continue;
        }
        let path = e.path();
        // `file_type` does not follow links, `Path::is_dir` does.
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk_with(root, &path, depth + 1, ignore, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            if ft.is_file() || path.is_file() {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
}

/// Everything under `cwd` as chunks to `sink`: git when it works, a walk otherwise.
pub fn list(cwd: &Path, sink: &mut dyn FnMut(Vec<String>)) {
    if stream_git(cwd, sink) {
        return;
    }
    let mut seen = HashSet::new();
    let files = walk(cwd);
    for c in files.chunks(CHUNK) {
        sink(with_dirs(c.to_vec(), &mut seen));
    }
}

// ---- the search worker -------------------------------------------------------------------

enum Job {
    Add(Vec<String>),
    Reset,
    Query(u64, String),
}

/// Owns the full index on a worker thread and answers queries without touching the UI thread.
pub struct FileSearch {
    tx: mpsc::Sender<Job>,
    latest: Arc<AtomicU64>,
    total: Arc<AtomicUsize>,
    gen: u64,
}

impl FileSearch {
    /// `reply` gets `(query, items)` for each query that was not superseded.
    pub fn spawn(limit: usize, reply: impl Fn(String, Vec<PItem>) + Send + 'static) -> FileSearch {
        let (tx, rx) = mpsc::channel::<Job>();
        let latest = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        let (l, t) = (latest.clone(), total.clone());
        std::thread::Builder::new()
            .name("openc-files".into())
            .spawn(move || {
                let mut files: Vec<String> = Vec::new();
                let mut want: Option<(u64, String)> = None;
                loop {
                    // Take everything queued; only the newest query matters.
                    let job = match want.take() {
                        Some(w) => {
                            // A query is waiting; look for newer work first.
                            match rx.try_recv() {
                                Ok(j) => {
                                    want = Some(w);
                                    j
                                }
                                Err(mpsc::TryRecvError::Empty) => {
                                    let (g, q) = w;
                                    let cancel = || l.load(Ordering::Relaxed) != g;
                                    if let Some(items) = search(&files, &q, limit, &cancel) {
                                        reply(q, items);
                                    }
                                    continue;
                                }
                                Err(mpsc::TryRecvError::Disconnected) => return,
                            }
                        }
                        None => match rx.recv() {
                            Ok(j) => j,
                            Err(_) => return,
                        },
                    };
                    match job {
                        Job::Add(mut c) => {
                            if files.len() < MAX_ENTRIES {
                                files.append(&mut c);
                            }
                            t.store(files.len(), Ordering::Relaxed);
                        }
                        Job::Reset => {
                            files.clear();
                            t.store(0, Ordering::Relaxed);
                        }
                        Job::Query(g, q) => want = Some((g, q)),
                    }
                }
            })
            .ok();
        FileSearch {
            tx,
            latest,
            total,
            gen: 0,
        }
    }

    pub fn add(&self, chunk: Vec<String>) {
        let _ = self.tx.send(Job::Add(chunk));
    }

    pub fn reset(&self) {
        let _ = self.tx.send(Job::Reset);
    }

    /// Entries the worker has seen (after the chunks it has processed).
    pub fn total(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    /// Ask for `q`'s rows; any search still running for an older query stops.
    pub fn query(&mut self, q: String) {
        self.gen += 1;
        self.latest.store(self.gen, Ordering::Relaxed);
        let _ = self.tx.send(Job::Query(self.gen, q));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[PItem]) -> Vec<&str> {
        v.iter().map(|i| i.insert.as_str()).collect()
    }

    /// Ranking with the allocation-free score must give the rows ranking with `path_match` gave:
    /// score every file, best first, then shorter, then listing order.
    #[test]
    fn the_score_only_ranking_matches_the_scored_positions_ranking() {
        let mut files: Vec<String> = (0..3000)
            .map(|i| {
                format!(
                    "crates/module{}/src/sub{}/file_{}_handler.rs",
                    i % 97,
                    i % 13,
                    i
                )
            })
            .collect();
        files.extend((0..97).map(|i| format!("crates/module{i}/")));
        files.extend(["README.md", "src/", "docs/Handler.md", "naïve/café.txt"].map(String::from));
        for q in [
            "h",
            "handler",
            "handler_9",
            "Handler",
            "src/m",
            "mod1",
            "café",
            "zzz",
            "rs",
            "e/s",
        ] {
            let mut want: Vec<(i32, usize, usize, Vec<usize>)> = files
                .iter()
                .enumerate()
                .filter(|(_, f)| !(f.ends_with('/') && f.as_str() == q))
                .filter_map(|(i, f)| path_match(q, f).map(|(s, idx)| (s, f.len(), i, idx)))
                .collect();
            want.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
            want.truncate(8);
            let got = search(&files, q, 8, &|| false).unwrap();
            let want: Vec<(&str, &Vec<usize>)> = want
                .iter()
                .map(|(_, _, i, idx)| (files[*i].as_str(), idx))
                .collect();
            let got: Vec<(&str, &Vec<usize>)> = got
                .iter()
                .map(|i| (i.insert.as_str(), &i.indices))
                .collect();
            assert_eq!(got, want, "query {q:?}");
        }
    }

    #[test]
    fn non_ascii_and_cjk_names_are_found_by_their_characters() {
        let files: Vec<String> = [
            "café.txt",
            "日本語 ファイル.md",
            "sub dir/deep/x y.rs",
            "a.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let never = || false;
        assert_eq!(
            names(&search(&files, "caf", 10, &never).unwrap()),
            ["café.txt"]
        );
        assert_eq!(
            names(&search(&files, "ファ", 10, &never).unwrap()),
            ["日本語 ファイル.md"]
        );
        assert_eq!(
            names(&search(&files, "日", 10, &never).unwrap()),
            ["日本語 ファイル.md"]
        );
        assert_eq!(
            names(&search(&files, "x y", 10, &never).unwrap()),
            ["sub dir/deep/x y.rs"]
        );
    }

    #[test]
    fn the_file_name_beats_a_directory_and_the_empty_query_lists_shallow_first() {
        let files: Vec<String> = [
            "library/notes.txt",
            "src/lib.rs",
            "deep/er/est/a.rs",
            "z.md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let never = || false;
        let r = search(&files, "lib", 10, &never).unwrap();
        assert_eq!(names(&r)[0], "src/lib.rs");
        let r = search(&files, "", 2, &never).unwrap();
        assert_eq!(names(&r)[0], "z.md");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn a_cancelled_search_returns_nothing() {
        let files: Vec<String> = (0..10_000).map(|i| format!("a/f{i}.txt")).collect();
        assert!(search(&files, "f", 10, &|| true).is_none());
    }

    #[test]
    fn dirs_are_listed_once_across_chunks() {
        let mut seen = HashSet::new();
        let a = with_dirs(vec!["a/b/c.rs".into(), "a/d.rs".into()], &mut seen);
        let b = with_dirs(vec!["a/b/e.rs".into(), "f/g.rs".into()], &mut seen);
        assert_eq!(a, ["a/", "a/b/", "a/b/c.rs", "a/d.rs"]);
        assert_eq!(b, ["a/b/e.rs", "f/", "f/g.rs"]);
    }

    #[test]
    fn the_walk_honours_gitignore_and_does_not_follow_a_link_out_of_the_tree() {
        let root = std::env::temp_dir().join(format!("openc-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("src/a.rs"), "x").unwrap();
        std::fs::write(root.join("b.log"), "x").unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("outside/secret.txt"), "x").unwrap();
        let tree = root.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("in.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&root, tree.join("loop")).unwrap();
        std::os::unix::fs::symlink("/", tree.join("rootlink")).unwrap();
        let mut got = walk(&tree);
        got.sort();
        assert_eq!(got, ["in.txt"]);
        let mut got = walk(&root);
        got.sort();
        assert_eq!(got, ["outside/secret.txt", "src/a.rs", "tree/in.txt"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_listing_streams_every_file_unquoted_with_no_cut() {
        let root = std::env::temp_dir().join(format!("openc-gitlist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).unwrap();
        for i in 0..40_001 {
            std::fs::write(root.join(format!("a/f{i:05}.txt")), "").unwrap();
        }
        std::fs::create_dir_all(root.join("z")).unwrap();
        std::fs::write(root.join("z/needle_unique.txt"), "").unwrap();
        std::fs::write(root.join("café.txt"), "").unwrap();
        std::fs::write(root.join("日本語 ファイル.md"), "").unwrap();
        let git = |args: &[&str]| {
            tuikit::git::command(&root).args(args).status().unwrap();
        };
        git(&["init", "-q"]);
        let mut all: Vec<String> = Vec::new();
        let mut chunks = 0;
        let ok = stream_git(&root, &mut |c| {
            chunks += 1;
            all.extend(c)
        });
        assert!(ok);
        assert!(chunks > 1, "streamed in chunks, got {chunks}");
        let files = all.iter().filter(|f| !f.ends_with('/')).count();
        assert_eq!(files, 40_004);
        assert!(all.contains(&"z/needle_unique.txt".to_string()));
        assert!(all.contains(&"café.txt".to_string()));
        assert!(all.contains(&"日本語 ファイル.md".to_string()));
        assert!(all.contains(&"a/".to_string()) && all.contains(&"z/".to_string()));
        // Past the old 30,000 cut.
        let hit = search(&all, "f39999", 5, &|| false).unwrap();
        assert_eq!(names(&hit)[0], "a/f39999.txt");
        let hit = search(&all, "needle_uni", 5, &|| false).unwrap();
        assert_eq!(names(&hit)[0], "z/needle_unique.txt");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_worker_answers_the_newest_query_and_drops_the_stale_ones() {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let mut fs = FileSearch::spawn(5, move |q, items| {
            let _ = tx.lock().unwrap().send((q, items));
        });
        fs.add((0..50_000).map(|i| format!("d/file{i}.rs")).collect());
        fs.add(vec!["needle.txt".into()]);
        fs.query("f".into());
        fs.query("fi".into());
        fs.query("needle".into());
        let t = std::time::Instant::now();
        let mut last = None;
        while t.elapsed() < std::time::Duration::from_secs(10) {
            if let Ok(r) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                let done = r.0 == "needle";
                last = Some(r);
                if done {
                    break;
                }
            }
        }
        let (q, items) = last.expect("a reply");
        assert_eq!(q, "needle");
        assert_eq!(names(&items), ["needle.txt"]);
        assert_eq!(fs.total(), 50_001);
    }
}
