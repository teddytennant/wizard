//! Git status, diffs and branches for the window's git rail.
//!
//! Shells out to `git` in the chat's workspace (`tokio::process`, never the
//! process's own cwd). Semantics match the TUI's `/diff` sidebar: unstaged +
//! staged numstat merged per path, untracked files counted as pure
//! additions, and Wizard's own `.wizard/` state skipped throughout.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

/// How many diff lines one file's [`diff`] hands back before it is cut off. A
/// regenerated lockfile changes by tens of thousands of lines; past a few
/// hundred screens nobody is reading it, and carrying all of it only stalls
/// the pane that has to draw it.
const MAX_DIFF_LINES: usize = 20_000;

/// The rail's summary of the workspace.
#[derive(Debug, Clone)]
pub struct GitStatus {
    pub branch: String,
    pub dirty: bool,
    pub additions: u64,
    pub deletions: u64,
    pub files: Vec<GitFile>,
}

/// One changed file: `status` is `M` (modified/renamed), `A` (added),
/// `D` (deleted), or `?` (untracked).
#[derive(Debug, Clone, Serialize)]
pub struct GitFile {
    pub path: String,
    pub status: char,
    pub additions: u64,
    pub deletions: u64,
}

/// Compose the git panel for `root`: branch and per-file diffstat from
/// `git status --porcelain=v1 -b` plus unstaged and staged `git diff
/// --numstat`. Untracked files are invisible to `git diff`, so their line
/// counts are read from disk as pure additions.
pub async fn status(root: &Path) -> Result<GitStatus> {
    // `--untracked-files=all` lists every file inside an untracked
    // directory instead of the collapsed `dir/` entry, so new directories
    // count line additions like `git_diff_text`'s `ls-files --others` does.
    let porcelain = git_output(
        root,
        &["status", "--porcelain=v1", "-b", "--untracked-files=all"],
    )
    .await?;
    let (branch, entries) = parse_porcelain(&porcelain);

    let mut counts: HashMap<String, (u64, u64)> = HashMap::new();
    let unstaged = git_output(root, &["diff", "--numstat"]).await?;
    let staged = git_output(root, &["diff", "--numstat", "--cached"]).await?;
    for (path, additions, deletions) in parse_numstat(&unstaged)
        .into_iter()
        .chain(parse_numstat(&staged))
    {
        let entry = counts.entry(path).or_default();
        entry.0 += additions;
        entry.1 += deletions;
    }

    let mut files = Vec::new();
    let mut total = (0u64, 0u64);
    for (path, status) in entries {
        if is_wizard_state_path(&path) {
            continue;
        }
        let (additions, deletions) = if status == '?' {
            // Untracked: the whole file is an addition.
            let bytes = tokio::fs::read(root.join(&path)).await.unwrap_or_default();
            (added_lines(&bytes), 0)
        } else {
            counts.get(&path).copied().unwrap_or((0, 0))
        };
        total.0 += additions;
        total.1 += deletions;
        files.push(GitFile {
            path,
            status,
            additions,
            deletions,
        });
    }

    Ok(GitStatus {
        branch,
        dirty: !files.is_empty(),
        additions: total.0,
        deletions: total.1,
        files,
    })
}

/// One changed file, parsed into hunks so the pane colors it without
/// re-parsing a diff of its own.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    /// The same `M|A|D|?` alphabet [`GitFile`] uses.
    pub status: char,
    pub additions: u64,
    pub deletions: u64,
    /// Git found nothing to diff by lines (an image, a compiled artifact): no
    /// hunks, and nothing to show but the fact itself.
    pub binary: bool,
    /// Cut off at [`MAX_DIFF_LINES`]; the hunks are as much as fits.
    pub truncated: bool,
    pub hunks: Vec<Hunk>,
}

/// One `@@` hunk.
#[derive(Debug, Clone, Serialize)]
pub struct Hunk {
    /// Git's own `@@ -1,4 +1,6 @@ fn main()` line, section heading included.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// One line of a hunk. `text` keeps git's leading marker (`+`, `-`, or the
/// context space), so a line copied out of the view is the diff line git wrote.
#[derive(Debug, Clone, Serialize)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

/// What a hunk line is, tagged for the client rather than left as a character
/// it would have to sniff.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Add,
    Del,
    Ctx,
    /// `\ No newline at end of file` — git's note about the file, not a line of it.
    Meta,
}

/// The diff of one changed file in `root`: the working tree as it stands
/// against HEAD, staged and unstaged changes together — which is exactly what
/// the `+N -M` beside the file in the git panel counts.
///
/// `path` arrives from the client, so it never reaches git until [`status`]
/// vouches for it: only a path git itself just reported as changed in this
/// workspace can be asked for. That refuses `../`, absolute paths and
/// `-`-prefixed pseudo-flags without a special case for any of them.
pub async fn diff(root: &Path, path: &str) -> Result<FileDiff> {
    let file = status(root)
        .await?
        .files
        .into_iter()
        .find(|file| file.path == path)
        .with_context(|| format!("'{path}' is not a changed file in this workspace"))?;

    let text = if file.status == '?' {
        // An untracked file is in neither HEAD nor the index, so `git diff` has
        // nothing to say about it — yet a brand new file is where seeing the
        // diff matters most. Diff it against nothing instead. `--no-index`
        // implies `--exit-code`, and "the files differ" (1) is the whole point
        // of asking, so it is not a failure.
        let args = ["diff", "--no-index", "--", "/dev/null", &file.path];
        git_output_ok(root, &args, &[0, 1]).await?
    } else {
        let base = diff_base(root).await?;
        git_output(root, &["diff", &base, "--", &file.path]).await?
    };

    let (hunks, binary, truncated) = parse_diff(&text);
    Ok(FileDiff {
        path: file.path,
        status: file.status,
        additions: file.additions,
        deletions: file.deletions,
        binary,
        truncated,
        hunks,
    })
}

/// What [`diff`] compares the working tree against: `HEAD`, or — in a repo
/// whose first commit is not written yet — the empty tree. An unborn HEAD is
/// not a revision, and `git diff HEAD` fails on it rather than diffing, which
/// would make a staged file in a fresh `git init` unopenable.
async fn diff_base(root: &Path) -> Result<String> {
    if git_output(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .await
        .is_ok()
    {
        return Ok("HEAD".to_string());
    }
    // Asked for rather than hardcoded: the empty tree's id depends on the
    // repo's hash algorithm (sha1 and sha256 repos disagree about it).
    let empty = git_output(root, &["hash-object", "-t", "tree", "/dev/null"]).await?;
    Ok(empty.trim().to_string())
}

/// Parse a one-file unified diff into hunks.
///
/// Everything before the first `@@` is git's preamble — the `diff --git`,
/// `index`, `---`/`+++` and rename headers — which is not part of any hunk and
/// which the client does not render. A binary file has no hunks at all: git
/// says so in the preamble and stops, and that is what `binary` reports.
fn parse_diff(text: &str) -> (Vec<Hunk>, bool, bool) {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut binary = false;
    let mut truncated = false;
    let mut count = 0usize;
    for line in text.lines() {
        if line.starts_with("@@") {
            hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = hunks.last_mut() else {
            if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
                binary = true;
            }
            continue;
        };
        if count >= MAX_DIFF_LINES {
            truncated = true;
            break;
        }
        let kind = match line.as_bytes().first() {
            Some(b'+') => LineKind::Add,
            Some(b'-') => LineKind::Del,
            Some(b'\\') => LineKind::Meta,
            _ => LineKind::Ctx,
        };
        hunk.lines.push(DiffLine {
            kind,
            text: line.to_string(),
        });
        count += 1;
    }
    (hunks, binary, truncated)
}

/// What the branch switcher lists.
///
/// Only the list. Which one is checked out is [`GitStatus::branch`], read by
/// the same rail from the same refresh — a second `git branch --show-current`
/// here would be a second subprocess to answer a question already answered.
#[derive(Debug, Clone)]
pub struct Branches {
    /// Local branches, most recently committed on first — the ones you are
    /// likely to want are the ones you touched last.
    pub branches: Vec<String>,
}

/// Local branches of `root`.
pub async fn branches(root: &Path) -> Result<Branches> {
    let listing = git_output(
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads",
        ],
    )
    .await?;
    Ok(Branches {
        branches: listing
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

/// Check `branch` out in `root`, creating it from the current HEAD when
/// `create` is set.
///
/// Git's own refusals are the point: an uncommitted change that the switch
/// would overwrite makes this fail, and that error is handed to the user
/// verbatim rather than being papered over with a force-checkout or a stash
/// they did not ask for.
pub async fn checkout(root: &Path, branch: &str, create: bool) -> Result<String> {
    let branch = branch.trim();
    anyhow::ensure!(!branch.is_empty(), "no branch given");
    // A leading dash would be read as a flag; other shapes git validates itself.
    anyhow::ensure!(!branch.starts_with('-'), "'{branch}' is not a branch name");
    let args: Vec<&str> = if create {
        vec!["checkout", "-b", branch]
    } else {
        vec!["checkout", branch]
    };
    git_output(root, &args).await?;
    let current = git_output(root, &["branch", "--show-current"]).await?;
    Ok(current.trim().to_string())
}

/// Run `git <args>` in `root` and return stdout; a nonzero exit is an error
/// carrying git's stderr.
async fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    git_output_ok(root, args, &[0]).await
}

/// [`git_output`], but with the exit codes that count as success spelled out:
/// `git diff --no-index` reports "the files differ" as a 1, which is the
/// ordinary outcome of diffing a new file rather than a failure.
async fn git_output_ok(root: &Path, args: &[&str], codes: &[i32]) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .context("running git")?;
    if !output
        .status
        .code()
        .is_some_and(|code| codes.contains(&code))
    {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `git status --porcelain=v1 -b`: the branch name from the `##`
/// header and one `(path, status)` per entry. Renames report the new path;
/// the status char folds the XY pair down to the protocol's `M|A|D|?`.
fn parse_porcelain(text: &str) -> (String, Vec<(String, char)>) {
    let mut branch = String::new();
    let mut entries = Vec::new();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            branch = parse_branch(header);
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let mut path = line[3..].to_string();
        // Renames/copies list `old -> new`; the new path is the live one.
        if let Some((_, new)) = path.split_once(" -> ") {
            path = new.to_string();
        }
        let status = if xy == "??" {
            '?'
        } else if xy.contains('D') {
            'D'
        } else if xy.contains('A') {
            'A'
        } else {
            'M'
        };
        entries.push((path, status));
    }
    (branch, entries)
}

/// The branch name out of a porcelain `##` header: `main...origin/main
/// [ahead 1]` → `main`, `HEAD (no branch)` → `HEAD`, `No commits yet on
/// main` → `main`.
fn parse_branch(header: &str) -> String {
    if let Some(name) = header.strip_prefix("No commits yet on ") {
        return name.to_string();
    }
    let name = header.split("...").next().unwrap_or(header);
    if name.starts_with("HEAD") {
        return "HEAD".to_string();
    }
    name.to_string()
}

/// Parse `git diff --numstat` lines (`added<TAB>deleted<TAB>path`) into
/// `(path, additions, deletions)`. Binary files report `-` counts and map
/// to zero; rename paths (`old => new`, brace form included) resolve to the
/// new path.
fn parse_numstat(text: &str) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let additions = added.trim().parse::<u64>().unwrap_or(0);
        let deletions = deleted.trim().parse::<u64>().unwrap_or(0);
        out.push((numstat_path(path), additions, deletions));
    }
    out
}

/// Resolve a numstat rename path to the post-rename name: the brace form
/// `src/{old => new}/mod.rs` substitutes in place, the plain form
/// `old.rs => new.rs` takes the right side. Plain paths pass through.
fn numstat_path(raw: &str) -> String {
    if let (Some(open), Some(close)) = (raw.find('{'), raw.find('}'))
        && open < close
        && let Some(arrow) = raw[open..close].find(" => ")
    {
        let new = &raw[open + arrow + 4..close];
        let joined = format!("{}{}{}", &raw[..open], new, &raw[close + 1..]);
        return joined.replace("//", "/");
    }
    if let Some((_, new)) = raw.split_once(" => ") {
        return new.to_string();
    }
    raw.to_string()
}

/// Is this repo-relative path inside Wizard's own state dir (`.wizard/`)?
/// Checkpoints and snapshots are Wizard internals, not the user's changes,
/// so the git panel omits them (same rule as the TUI's `/diff`).
fn is_wizard_state_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path == ".wizard" || path.starts_with(".wizard/") || path.contains("/.wizard/")
}

/// Lines an untracked file adds: newline count, plus one for a final
/// unterminated line. Binary content (NUL byte) counts zero, mirroring
/// numstat's `-`.
fn added_lines(bytes: &[u8]) -> u64 {
    if bytes.contains(&0) {
        return 0;
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway repo with one commit on `main`.
    async fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git_output(root, &["init", "-q", "-b", "main"])
            .await
            .unwrap();
        git_output(root, &["config", "user.email", "t@example.test"])
            .await
            .unwrap();
        git_output(root, &["config", "user.name", "Test"])
            .await
            .unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git_output(root, &["add", "-A"]).await.unwrap();
        git_output(root, &["commit", "-qm", "init"]).await.unwrap();
        dir
    }

    #[tokio::test]
    async fn branches_lists_the_locals_and_status_names_the_checked_out_one() {
        let dir = repo().await;
        let root = dir.path();
        git_output(root, &["branch", "feat/one"]).await.unwrap();

        let listing = branches(root).await.unwrap();
        assert!(listing.branches.contains(&"main".to_string()));
        assert!(listing.branches.contains(&"feat/one".to_string()));
        assert_eq!(status(root).await.unwrap().branch, "main");
    }

    #[tokio::test]
    async fn checkout_switches_and_creates() {
        let dir = repo().await;
        let root = dir.path();
        git_output(root, &["branch", "feat/one"]).await.unwrap();

        assert_eq!(checkout(root, "feat/one", false).await.unwrap(), "feat/one");
        assert_eq!(checkout(root, "wip/new", true).await.unwrap(), "wip/new");
        assert_eq!(status(root).await.unwrap().branch, "wip/new");

        // Git's refusals are the guard rail: an uncommitted change the switch
        // would overwrite must fail, not be forced or stashed behind the user.
        std::fs::write(root.join("a.txt"), "uncommitted\n").unwrap();
        git_output(root, &["add", "-A"]).await.unwrap();
        git_output(root, &["commit", "-qm", "on wip"])
            .await
            .unwrap();
        std::fs::write(root.join("a.txt"), "dirty\n").unwrap();
        let err = checkout(root, "main", false).await.unwrap_err().to_string();
        assert!(err.contains("would be overwritten"), "unexpected: {err}");
        assert_eq!(
            status(root).await.unwrap().branch,
            "wip/new",
            "a refused checkout leaves the branch alone"
        );

        assert!(checkout(root, "", false).await.is_err());
        assert!(checkout(root, "--force", false).await.is_err());
    }

    /// Every line of the diff, hunks flattened, as `(kind, text)`.
    fn diff_lines(diff: &FileDiff) -> Vec<(LineKind, &str)> {
        diff.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| (line.kind, line.text.as_str()))
            .collect()
    }

    #[tokio::test]
    async fn diff_shows_staged_and_unstaged_changes_together() {
        let dir = repo().await;
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        git_output(root, &["add", "a.txt"]).await.unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();

        let diff = diff(root, "a.txt").await.unwrap();
        assert_eq!(diff.status, 'M');
        assert!(!diff.binary && !diff.truncated);
        assert!(diff.hunks[0].header.starts_with("@@"), "{:?}", diff.hunks);
        assert_eq!(
            diff_lines(&diff),
            vec![
                (LineKind::Ctx, " one"),
                (LineKind::Add, "+two"),
                (LineKind::Add, "+three"),
            ],
            "the staged line and the unstaged one, in one diff"
        );
        // The panel's `+N -M` for this file counts both too: the diff the row
        // opens is the diff the row promised.
        assert_eq!((diff.additions, diff.deletions), (2, 0));
    }

    #[tokio::test]
    async fn diff_shows_an_untracked_file_as_all_additions() {
        let dir = repo().await;
        let root = dir.path();
        std::fs::write(root.join("new.txt"), "alpha\nbeta\n").unwrap();

        let diff = diff(root, "new.txt").await.unwrap();
        assert_eq!(diff.status, '?');
        assert_eq!((diff.additions, diff.deletions), (2, 0));
        assert_eq!(
            diff_lines(&diff),
            vec![(LineKind::Add, "+alpha"), (LineKind::Add, "+beta")]
        );
    }

    #[tokio::test]
    async fn diff_shows_a_deleted_file_as_all_deletions() {
        let dir = repo().await;
        let root = dir.path();
        std::fs::remove_file(root.join("a.txt")).unwrap();

        let diff = diff(root, "a.txt").await.unwrap();
        assert_eq!(diff.status, 'D');
        assert_eq!((diff.additions, diff.deletions), (0, 1));
        assert_eq!(diff_lines(&diff), vec![(LineKind::Del, "-one")]);
    }

    #[tokio::test]
    async fn diff_names_binary_files_instead_of_dumping_them() {
        let dir = repo().await;
        let root = dir.path();
        std::fs::write(root.join("logo.bin"), [0u8, 1, 2, 3]).unwrap();
        git_output(root, &["add", "-A"]).await.unwrap();
        git_output(root, &["commit", "-qm", "binary"])
            .await
            .unwrap();
        std::fs::write(root.join("logo.bin"), [0u8, 9, 9, 9, 9]).unwrap();
        std::fs::write(root.join("new.bin"), [0u8, 7]).unwrap();

        let tracked = diff(root, "logo.bin").await.unwrap();
        assert!(tracked.binary);
        assert!(tracked.hunks.is_empty());

        let untracked = diff(root, "new.bin").await.unwrap();
        assert!(untracked.binary, "a new binary file is binary too");
        assert!(untracked.hunks.is_empty());
    }

    #[tokio::test]
    async fn diff_refuses_paths_the_workspace_does_not_report_as_changed() {
        let dir = repo().await;
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();

        // Nothing the client sends reaches git unless `status` just named it:
        // an escape out of the workspace, an absolute path, a flag in path's
        // clothing, and a file that simply is not changed all stop here.
        for path in [
            "../outside.txt",
            "/etc/passwd",
            "-p",
            "--output=/tmp/pwned",
            "a.txt/../a.txt",
            "b.txt",
        ] {
            let err = diff(root, path).await.unwrap_err().to_string();
            assert!(err.contains("not a changed file"), "{path}: {err}");
        }
        assert!(!diff(root, "a.txt").await.unwrap().hunks.is_empty());
    }

    #[tokio::test]
    async fn a_huge_diff_is_truncated_rather_than_shipped_whole() {
        let dir = repo().await;
        let root = dir.path();
        let body: String = (0..MAX_DIFF_LINES + 500)
            .map(|i| format!("{i}\n"))
            .collect();
        std::fs::write(root.join("lock.txt"), body).unwrap();

        let diff = diff(root, "lock.txt").await.unwrap();
        assert!(diff.truncated);
        assert_eq!(diff_lines(&diff).len(), MAX_DIFF_LINES);
    }

    /// A change git records but has no lines for: the file's diff is honestly
    /// empty rather than an error the client would have to interpret.
    ///
    /// Unix-only because the *premise* is: git records an execute-bit flip as
    /// a change only where the filesystem has one, so on a platform where
    /// [`crate::platform::exe_swap::set_executable`] is a no-op there is no
    /// mode-only change to ask about.
    #[cfg(unix)]
    #[tokio::test]
    async fn diff_of_a_mode_only_change_is_empty() {
        let dir = repo().await;
        let root = dir.path();
        crate::platform::exe_swap::set_executable(&root.join("a.txt")).unwrap();

        let diff = diff(root, "a.txt").await.unwrap();
        assert_eq!((diff.additions, diff.deletions), (0, 0));
        assert!(diff.hunks.is_empty() && !diff.binary);
    }

    #[test]
    fn porcelain_parses_branch_and_entries() {
        let text = "## feat/gui...origin/feat/gui [ahead 2]\n\
                    M  src/lib.rs\n \
                    M src/cli.rs\n\
                    A  src/gui/mod.rs\n \
                    D old.rs\n\
                    ?? notes.txt\n\
                    R  old-name.rs -> new-name.rs\n";
        let (branch, entries) = parse_porcelain(text);
        assert_eq!(branch, "feat/gui");
        assert_eq!(
            entries,
            vec![
                ("src/lib.rs".to_string(), 'M'),
                ("src/cli.rs".to_string(), 'M'),
                ("src/gui/mod.rs".to_string(), 'A'),
                ("old.rs".to_string(), 'D'),
                ("notes.txt".to_string(), '?'),
                ("new-name.rs".to_string(), 'M'),
            ]
        );
    }

    #[test]
    fn porcelain_branch_headers_cover_detached_and_unborn() {
        assert_eq!(parse_branch("main...origin/main"), "main");
        assert_eq!(parse_branch("HEAD (no branch)"), "HEAD");
        assert_eq!(parse_branch("No commits yet on trunk"), "trunk");
        assert_eq!(parse_branch("feat/x"), "feat/x");
    }

    #[test]
    fn numstat_parses_counts_binaries_and_renames() {
        let text = "10\t2\tsrc/gui/mod.rs\n\
                    -\t-\tassets/logo.png\n\
                    3\t1\tsrc/{old => new}/mod.rs\n\
                    0\t0\ta.rs => b.rs\n";
        assert_eq!(
            parse_numstat(text),
            vec![
                ("src/gui/mod.rs".to_string(), 10, 2),
                ("assets/logo.png".to_string(), 0, 0),
                ("src/new/mod.rs".to_string(), 3, 1),
                ("b.rs".to_string(), 0, 0),
            ]
        );
    }

    #[test]
    fn numstat_rename_with_empty_segment_collapses_slashes() {
        assert_eq!(numstat_path("src/{gui => }/mod.rs"), "src/mod.rs");
        assert_eq!(numstat_path("plain/path.rs"), "plain/path.rs");
    }

    #[test]
    fn wizard_state_paths_are_recognized() {
        assert!(is_wizard_state_path(".wizard/checkpoints/1/0.snap"));
        assert!(is_wizard_state_path("sub/.wizard/x"));
        assert!(is_wizard_state_path(".wizard"));
        assert!(!is_wizard_state_path("src/wizard.rs"));
        assert!(!is_wizard_state_path(".wizardrc"));
    }

    #[test]
    fn added_lines_counts_text_and_skips_binary() {
        assert_eq!(added_lines(b"one\ntwo\n"), 2);
        assert_eq!(added_lines(b"one\ntwo"), 2);
        assert_eq!(added_lines(b""), 0);
        assert_eq!(added_lines(b"bin\0ary"), 0);
    }
}
