//! The only way the frontends run `git`.
//!
//! The working directory is whatever the user opened the app in, and its `.git/config` is
//! not trusted: `git status` runs the program named by `core.fsmonitor`, and hooks run from
//! `core.hooksPath` or `.git/hooks`. Every call goes through [`command`], which overrides
//! those on the command line (which wins over repository config, and is inherited by
//! submodule children) and keeps git from taking the index lock the user's own `git`
//! might want.

use std::path::Path;
use std::process::{Command, Stdio};

/// `git` with repository-configured commands switched off. Add the subcommand with `.args`.
pub fn command(cwd: &Path) -> Command {
    let mut c = Command::new("git");
    c.args([
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        // Nothing here wants a pager, a credential prompt or an external diff.
        "-c",
        "core.pager=cat",
        "-c",
        "core.quotepath=false",
    ])
    .current_dir(cwd)
    .env("GIT_OPTIONAL_LOCKS", "0")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_PAGER", "cat")
    .env_remove("GIT_EXTERNAL_DIFF")
    .stdin(Stdio::null());
    c
}

/// Tracked and untracked, not ignored, relative to `cwd`. `None` outside a repository or when
/// git fails. NUL separated, because git quotes non-ASCII names otherwise and the popup would
/// show `"caf\303\251.txt"`.
pub fn list_files(cwd: &Path) -> Option<Vec<String>> {
    let out = command(cwd)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(
        out.stdout
            .split(|b| *b == 0)
            .filter(|n| !n.is_empty())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect(),
    )
}

/// The files an `@` popup offers: git's view of the tree, or a walk outside a repository, cut
/// at `max` (a result of exactly `max` entries means the list was cut). The walk goes four
/// directories deep, reaches dot files and directories (`.github`, `.env.example`), skips VCS
/// metadata and the usual dependency and build trees, and does not follow links, so a link
/// back up the tree is not a loop.
pub fn project_files(cwd: &Path, max: usize) -> Vec<String> {
    if let Some(mut files) = list_files(cwd) {
        files.truncate(max);
        return files;
    }
    fn walk(dir: &Path, rel: &str, depth: usize, max: usize, v: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if matches!(
                name.as_str(),
                ".git" | ".hg" | ".svn" | "node_modules" | "target"
            ) {
                continue;
            }
            let path = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let ty = e.file_type();
            if ty.as_ref().is_ok_and(|t| t.is_dir()) {
                if depth < 4 {
                    walk(&e.path(), &path, depth + 1, max, v);
                }
            } else if v.len() < max {
                v.push(path);
            }
        }
    }
    let mut v = Vec::new();
    walk(cwd, "", 0, max, &mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn repo(tag: &str) -> std::path::PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "tuikit-git-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let g = |args: &[&str]| {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&d)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap()
                .status
                .success());
        };
        g(&["init", "-q"]);
        std::fs::write(d.join("a.txt"), "x").unwrap();
        g(&["add", "a.txt"]);
        g(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "i",
        ]);
        d
    }

    /// A repository whose own config names a program to run on `git status`.
    fn booby_trapped() -> (std::path::PathBuf, std::path::PathBuf) {
        let d = repo("rce");
        let marker = d.join("PWNED");
        let script = d.join("evil.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ok = Command::new("git")
            .args(["config", "core.fsmonitor", script.to_str().unwrap()])
            .current_dir(&d)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        (d, marker)
    }

    #[test]
    fn plain_git_status_runs_the_repo_configured_fsmonitor() {
        // The control: without this the next test proves nothing.
        let (d, marker) = booby_trapped();
        Command::new("git")
            .args(["status", "--porcelain", "-uno"])
            .current_dir(&d)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(marker.exists(), "this git does not run core.fsmonitor");
    }

    #[test]
    fn hardened_git_does_not_run_the_repo_fsmonitor() {
        let (d, marker) = booby_trapped();
        let o = command(&d)
            .args(["status", "--porcelain", "-uno"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(o.status.success());
        assert!(!marker.exists(), "core.fsmonitor ran");
        assert!(list_files(&d).is_some());
        assert!(!marker.exists(), "core.fsmonitor ran for ls-files");
    }

    #[test]
    fn list_files_returns_unquoted_non_ascii_names() {
        let d = repo("names");
        std::fs::write(d.join("café.txt"), "x").unwrap();
        std::fs::write(d.join("日本語.md"), "x").unwrap();
        std::fs::write(d.join("a b.txt"), "x").unwrap();
        let mut f = list_files(&d).unwrap();
        f.sort();
        assert_eq!(f, ["a b.txt", "a.txt", "café.txt", "日本語.md"]);
        assert_eq!(
            list_files(&std::env::temp_dir().join("no-such-dir-xyz")),
            None
        );
    }
}
