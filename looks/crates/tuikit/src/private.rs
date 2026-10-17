//! Files a frontend writes for itself: owner-only, and replaced whole or not at all.
//!
//! History holds every pasted body, stash holds drafts, `clips/` holds screenshots, and a
//! prompt can carry a key someone just pasted. The default umask makes all of that readable
//! by every account on the box, so state goes through here instead of `std::fs::write`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `dir` and its parents, with `dir` itself closed to everyone but the owner.
pub fn create_dir_all(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // a directory from an earlier version keeps its old mode otherwise
    if fs::metadata(dir)?.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A new file that did not exist before, mode 0600. Fails if `path` is taken, including by a
/// symlink someone left there.
pub fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Replace `path` with `bytes`: write a 0600 sibling, then rename over it, so a kill in the
/// middle leaves the old file and never half of the new one. The data is synced to disk first.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_replacing(path, bytes, true)
}

/// [`write_atomic`] without the sync: a kill still leaves the old file or the new one, only a
/// power cut can lose the write. For what is rewritten on the UI thread at every prompt
/// (history), where an `fsync` on a busy disk was seconds.
pub fn write_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_replacing(path, bytes, false)
}

fn write_replacing(path: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    let mut f = create_new(&tmp)?;
    let done = f
        .write_all(bytes)
        .and_then(|()| if sync { f.sync_all() } else { Ok(()) });
    drop(f);
    match done.and_then(|()| fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Append `bytes` to `path`, creating it 0600 and closing an older, looser file.
pub fn append(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    if f.metadata()?.permissions().mode() & 0o077 != 0 {
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tuikit-private-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn state_files_and_dirs_are_owner_only_whatever_the_umask() {
        let d = scratch("modes");
        let f = d.join("state/history.jsonl");
        append(&f, b"a\n").unwrap();
        assert_eq!(mode(&f), 0o600);
        assert_eq!(mode(f.parent().unwrap()), 0o700);
        // an older, world-readable file and directory are closed on the next write
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(f.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        append(&f, b"b\n").unwrap();
        assert_eq!(mode(&f), 0o600);
        let g = d.join("state/stash.jsonl");
        write_atomic(&g, b"x").unwrap();
        assert_eq!(mode(&g), 0o600);
        assert_eq!(mode(g.parent().unwrap()), 0o700);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_replacement_is_whole_and_leaves_nothing_behind() {
        let d = scratch("atomic");
        let f = d.join("stash.jsonl");
        write_atomic(&f, b"one").unwrap();
        write_atomic(&f, b"two").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "two");
        assert_eq!(fs::read_dir(&d).unwrap().count(), 1, "a temp file was left");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn create_new_refuses_a_name_that_is_taken_or_a_symlink() {
        let d = scratch("new");
        fs::create_dir_all(&d).unwrap();
        let p = d.join("f");
        create_new(&p).unwrap();
        assert!(create_new(&p).is_err());
        let l = d.join("l");
        std::os::unix::fs::symlink(d.join("elsewhere"), &l).unwrap();
        assert!(create_new(&l).is_err());
        assert!(!d.join("elsewhere").exists());
        let _ = fs::remove_dir_all(&d);
    }
}
