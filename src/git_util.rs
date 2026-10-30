//! Minimal async git helpers shared by fleet: ref resolution, dirty
//! detection, and worktree lifecycle.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Resolve `ref_` to a full commit sha in `repo`
/// (`git rev-parse --verify <ref>^{commit}`).
pub async fn rev_parse(repo: &Path, ref_: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", &format!("{ref_}^{{commit}}")])
        .output()
        .await
        .context("running git rev-parse")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --verify {ref_:?} failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// HEAD sha plus whether the working tree is dirty (`git status --porcelain`
/// non-empty); `None` outside a git repo or on any git failure. Never errors
/// so callers can treat git metadata as best-effort.
pub async fn head_and_dirty(repo: &Path) -> Option<(String, bool)> {
    let head = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()
        .await
        .ok()?;
    if !head.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .await
        .ok()?;
    if !status.status.success() {
        return None;
    }
    let dirty = !String::from_utf8_lossy(&status.stdout).trim().is_empty();
    Some((sha, dirty))
}

/// Create a detached worktree of `ref_` at `dest`.
pub async fn worktree_add(repo: &Path, dest: &Path, ref_: &str) -> Result<()> {
    let result = worktree_add_inner(repo, dest, ref_).await;
    if result.is_ok() {
        crate::hooks::emit_detached(
            crate::hooks::event::HookEvent::WorktreeCreate,
            None,
            serde_json::json!({ "name": dest.file_name().map(|n| n.to_string_lossy().into_owned()), "worktree_path": dest }),
        );
    }
    result
}

async fn worktree_add_inner(repo: &Path, dest: &Path, ref_: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach"])
        .arg(dest)
        .arg(ref_)
        .output()
        .await
        .context("running git worktree add")?;
    if !output.status.success() {
        bail!(
            "git worktree add {} {ref_} failed: {}",
            dest.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Create a detached worktree of `ref_` at `dest`, cloning the tracked working
/// tree copy-on-write (reflink) when the filesystem supports it (btrfs, XFS
/// with reflink=1, APFS, ...). Falls back to a plain full [`worktree_add`]
/// checkout when reflink is unavailable or the fast path errors, so the result
/// is always a clean `ref_` checkout — identical to what `worktree_add`
/// produces, just faster to populate on a CoW filesystem.
///
/// The technique is grok-build's `xai-fast-worktree` (Apache-2.0), reduced to
/// its core: a metadata-only `--no-checkout` worktree, then a reflink clone of
/// the tracked files, then `git reset --hard` to reconcile the index and
/// restore any source-dirty paths to `ref_`.
pub async fn worktree_add_cow(repo: &Path, dest: &Path, ref_: &str) -> Result<()> {
    let (repo_buf, dest_buf, ref_buf) = (repo.to_path_buf(), dest.to_path_buf(), ref_.to_string());
    let fast = tokio::task::spawn_blocking(move || cow_populate(&repo_buf, &dest_buf, &ref_buf))
        .await
        .context("cow worktree task panicked")?;
    if let Err(err) = fast {
        tracing::debug!("cow worktree fast path unavailable ({err:#}); using plain git checkout");
        // Reclaim whatever the fast path registered/wrote (a `--no-checkout`
        // worktree and/or a partial dir) before the plain fallback. On the
        // common no-reflink filesystem the fast path bails before creating
        // anything, so `dest` is absent and there is nothing to clean up.
        if dest.exists() {
            worktree_remove(repo, dest).await;
            let _ = std::fs::remove_dir_all(dest);
        }
        return worktree_add(repo, dest, ref_).await;
    }
    Ok(())
}

/// Blocking reflink build of a detached worktree. Any `Err` triggers the
/// plain-checkout fallback in [`worktree_add_cow`], so it can bail freely.
fn cow_populate(repo: &Path, dest: &Path, ref_: &str) -> Result<()> {
    // Only worth it when reflink actually shares blocks: a whole-tree byte copy
    // is no faster than git's own checkout, so defer to the plain path there.
    if !reflink_supported_in(dest.parent().unwrap_or(dest)) {
        bail!("filesystem does not support reflink");
    }
    // 1. Instant metadata-only worktree: registers `dest` and its index at
    //    `ref_` but writes zero working-tree files.
    let dest_str = dest.to_string_lossy();
    run_git(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            dest_str.as_ref(),
            ref_,
        ],
    )?;
    // 2. Reflink-clone every tracked path from the source working tree.
    let listing = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "-z"])
        .output()
        .context("running git ls-files")?;
    if !listing.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        );
    }
    for rel in listing.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        let rel = Path::new(std::ffi::OsStr::from_bytes(rel));
        let src = repo.join(rel);
        let dst = dest.join(rel);
        // A staged-but-deleted path is listed yet absent on disk; the final
        // `git reset --hard` restores it, so skip it here.
        let Ok(meta) = std::fs::symlink_metadata(&src) else {
            continue;
        };
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&src)?;
            let _ = std::fs::remove_file(&dst);
            std::os::unix::fs::symlink(target, &dst)?;
        } else {
            // Per-file CoW; reflink_or_copy falls back to a byte copy itself if
            // an individual clone can't be shared.
            reflink_copy::reflink_or_copy(&src, &dst)
                .with_context(|| format!("cloning {}", rel.display()))?;
            // FICLONE clones data blocks under the default umask; carry the
            // source mode so the executable bit survives.
            std::fs::set_permissions(&dst, meta.permissions())?;
        }
    }
    // 3. Reconcile the index stat cache and restore any source-dirty files to
    //    `ref_` — yields the same clean checkout as `git worktree add`.
    run_git(dest, &["reset", "--hard", ref_])?;
    Ok(())
}

/// Whether `dir`'s filesystem supports a real (block-sharing) reflink, probed
/// by cloning a scratch file. A strict [`reflink_copy::reflink`] errors when
/// CoW is unsupported, unlike the auto-falling-back `reflink_or_copy`.
fn reflink_supported_in(dir: &Path) -> bool {
    let probe = dir.join(".wizard-reflink-probe");
    let clone = dir.join(".wizard-reflink-probe.clone");
    let _ = std::fs::remove_file(&clone);
    let ok = std::fs::write(&probe, b"x").is_ok() && reflink_copy::reflink(&probe, &clone).is_ok();
    let _ = std::fs::remove_file(&probe);
    let _ = std::fs::remove_file(&clone);
    ok
}

/// Run a git subcommand synchronously in `cwd`, erroring with its stderr.
fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Remove the worktree at `dest`, best-effort: replay cleanup must never
/// mask the actual case result.
pub async fn worktree_remove(repo: &Path, dest: &Path) {
    crate::hooks::emit_detached(
        crate::hooks::event::HookEvent::WorktreeRemove,
        None,
        serde_json::json!({ "worktree_path": dest }),
    );
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(dest)
        .output()
        .await;
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            "git worktree remove {} failed: {}",
            dest.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) => tracing::warn!("git worktree remove {}: {err}", dest.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn git(cwd: &Path, args: &[&str]) {
        run_git(cwd, args).unwrap_or_else(|err| panic!("git {args:?}: {err:#}"));
    }

    /// A repo with a regular file, an executable, and a symlink committed, then
    /// a dirtied working tree (a tracked edit + an untracked file). Returns the
    /// temp holder (kept alive for the test) and the repo path.
    fn dirty_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);

        std::fs::write(repo.join("plain.txt"), "committed\n").unwrap();
        std::fs::write(repo.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("plain.txt", repo.join("link")).unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);

        std::fs::write(repo.join("plain.txt"), "LOCAL EDIT - must not leak\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "scratch\n").unwrap();
        (tmp, repo)
    }

    /// Assert `dest` is a clean HEAD checkout: no dirty leak, no untracked
    /// file, exec bit and symlink preserved.
    fn assert_clean_checkout(dest: &Path) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dest)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "worktree should be a clean HEAD checkout, got: {}",
            String::from_utf8_lossy(&status.stdout)
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("plain.txt")).unwrap(),
            "committed\n",
            "the source's uncommitted edit must not leak into the worktree"
        );
        assert!(
            !dest.join("untracked.txt").exists(),
            "an untracked source file must not be cloned"
        );
        let mode = std::fs::metadata(dest.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "exec bit should survive, mode {mode:o}");
        assert!(
            std::fs::symlink_metadata(dest.join("link"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "a symlink should stay a symlink"
        );
    }

    #[tokio::test]
    async fn head_and_dirty_reports_state_and_never_errors() {
        let (tmp, repo) = dirty_repo();
        let (sha, dirty) = head_and_dirty(&repo).await.expect("inside a repo");
        assert_eq!(sha.len(), 40, "full sha: {sha}");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "{sha}");
        assert!(dirty, "tracked edit + untracked file mean dirty");

        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "absorb"]);
        let (sha_after, dirty) = head_and_dirty(&repo).await.expect("inside a repo");
        assert!(!dirty, "clean after committing everything");
        assert_ne!(sha_after, sha, "HEAD moved with the commit");

        let outside = tmp.path().join("not-a-repo");
        std::fs::create_dir_all(&outside).unwrap();
        assert_eq!(head_and_dirty(&outside).await, None, "no repo, no record");
    }

    #[tokio::test]
    async fn rev_parse_resolves_refs_and_rejects_garbage() {
        let (_tmp, repo) = dirty_repo();
        let sha = rev_parse(&repo, "HEAD").await.expect("HEAD resolves");
        assert_eq!(sha.len(), 40, "full sha: {sha}");
        let by_prefix = rev_parse(&repo, &sha[..12]).await.expect("prefix resolves");
        assert_eq!(by_prefix, sha, "a short prefix expands to the full sha");

        let err = rev_parse(&repo, "no-such-ref")
            .await
            .expect_err("an unknown ref fails");
        assert!(format!("{err:#}").contains("no-such-ref"), "{err:#}");
    }

    #[tokio::test]
    async fn worktree_add_cow_produces_a_clean_checkout() {
        let (tmp, repo) = dirty_repo();
        // Mirror the fleet layout: the worktrees parent dir exists first.
        let parent = tmp.path().join("worktrees");
        std::fs::create_dir_all(&parent).unwrap();
        let dest = parent.join("0");

        // Exercises the reflink path on btrfs/XFS/APFS, the plain fallback on
        // ext4/tmpfs (here) — both must yield the identical clean checkout.
        worktree_add_cow(&repo, &dest, "HEAD")
            .await
            .expect("cow worktree");
        assert_clean_checkout(&dest);
    }

    #[test]
    fn cow_populate_clones_when_reflink_is_supported() {
        let (tmp, repo) = dirty_repo();
        let parent = tmp.path().join("worktrees");
        std::fs::create_dir_all(&parent).unwrap();
        if !reflink_supported_in(&parent) {
            return; // ext4/tmpfs: the fallback is covered by the test above.
        }
        let dest = parent.join("0");
        cow_populate(&repo, &dest, "HEAD").expect("cow populate");
        assert_clean_checkout(&dest);
    }
}
