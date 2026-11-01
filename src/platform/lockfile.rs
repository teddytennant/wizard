//! Cross-process exclusive locks, held on a named file.
//!
//! Two wizards on one machine share `~/.wizard`, and more than one thing in
//! there is rewritten whole: the trust store is read entirely, edited, and
//! renamed into place. Nothing in that sequence is atomic *across* processes,
//! so the second rename drops whatever the first one added, and a user who
//! answered "yes, trust this project" in one terminal is asked again in the
//! other with no explanation. An in-process `Mutex` cannot help; the two
//! writers are not in one process.
//!
//! Unix answers with `flock(2)`: an advisory lock attached to an open file
//! description, released when the descriptor closes, which the kernel does on
//! exit, on panic and on SIGKILL alike. That last property is why this is a
//! lock and not a lock *file*: there is no stale-lock cleanup to get wrong,
//! because a dead holder cannot leave one behind. Windows has `LockFileEx`,
//! which is the same shape (a handle held open, released when it closes) with
//! a different spelling, so the surface here is "take a lock named by a path,
//! hold it in a value, drop the value to release".
//!
//! The lock deliberately sits on a *sidecar* file rather than on the store it
//! guards. The store is replaced by `rename`, and a lock held on the replaced
//! inode guards nothing: the next process opens the new inode and takes a lock
//! nobody else is holding.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often [`exclusive`] retries while it waits.
///
/// Public so a caller writing its own wait loop (a test observing contention,
/// say) polls at the same cadence rather than inventing a second one. Short
/// enough that a lock released mid-wait is picked up promptly; long enough
/// that a half-second wait is fifty syscalls rather than a spin.
pub const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Whether this platform has cross-process locking wired up yet.
///
/// `false` off Unix, where [`exclusive`] gives up at once rather than sitting
/// out a caller's whole timeout waiting for something nothing can grant. Every
/// caller already has to handle "no lock" (a wedged wizard elsewhere on the
/// machine must not stop this one from starting), so an unported platform
/// takes the path that already exists instead of a new one.
pub const SUPPORTED: bool = cfg!(unix);

/// A held lock. Dropping it releases the lock, including on panic and on
/// process death, because the release is the descriptor closing rather than
/// anything this type has to remember to do.
#[derive(Debug)]
pub struct Guard {
    /// Held for its side effect. The lock lives on the open file description,
    /// so this field being unread is the point: it must not be dropped early.
    _file: std::fs::File,
    /// Only for diagnostics; the lock is not identified by its path.
    path: PathBuf,
}

impl Guard {
    /// The file the lock is held on, for an error message that has to name it.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Take the lock named by `path` if it is free right now, without waiting.
///
/// `None` covers both "somebody else holds it" and "this platform cannot take
/// it"; a caller that needs to tell those apart reads [`SUPPORTED`].
pub fn try_exclusive(path: &Path) -> Option<Guard> {
    if !SUPPORTED {
        return None;
    }
    let file = open(path)?;
    if !take(&file) {
        return None;
    }
    Some(Guard {
        _file: file,
        path: path.to_path_buf(),
    })
}

/// Take the lock named by `path`, waiting up to `wait` for whoever holds it.
///
/// `None` when the wait ran out, when the lock file could not be opened, or
/// when the platform has no locking yet. Callers treat all three the same way
/// and proceed unlocked, so this warns rather than returning an error: the
/// alternative is a wizard that will not start because something unrelated on
/// the machine is wedged holding a lock.
pub fn exclusive(path: &Path, wait: Duration) -> Option<Guard> {
    if !SUPPORTED {
        return None;
    }
    let file = open(path)?;
    let deadline = Instant::now() + wait;
    loop {
        if take(&file) {
            return Some(Guard {
                _file: file,
                path: path.to_path_buf(),
            });
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "gave up waiting for the lock on {} after {wait:?}; \
                 proceeding without it",
                path.display()
            );
            return None;
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}

/// Open (creating if needed) the file the lock is taken on.
///
/// Owner-only, through [`super::secrets::open_private_file`]: the file carries
/// nothing secret, but another local user who can truncate or replace it can
/// hand themselves a lock Wizard believes it is holding. Its contents are left
/// alone, because a caller may have recorded who the holder is.
fn open(path: &Path) -> Option<std::fs::File> {
    match super::secrets::open_private_file(path) {
        Ok(file) => Some(file),
        Err(err) => {
            // Not fatal, by the same argument as the timeout above: an
            // unopenable lock file must not be the reason Wizard refuses to
            // record a decision the user just made.
            tracing::warn!("could not open the lock file {}: {err:#}", path.display());
            None
        }
    }
}

/// Try once to take an exclusive lock on an already-open file.
#[cfg(unix)]
fn take(file: &std::fs::File) -> bool {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `file` owns the descriptor and outlives the call; `flock` takes
    // no pointer and mutates nothing this process owns. `LOCK_NB` is what
    // keeps this from blocking inside the syscall, where the caller's timeout
    // could not reach it.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// The seam: `LockFileEx` with `LOCKFILE_EXCLUSIVE_LOCK |
/// LOCKFILE_FAIL_IMMEDIATELY` over the whole file, and `UnlockFileEx` (or the
/// handle closing) to release. Until that exists, [`SUPPORTED`] is `false` and
/// nothing reaches here.
#[cfg(not(unix))]
fn take(file: &std::fs::File) -> bool {
    let _ = file;
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_second_holder_waits_and_gets_the_lock_once_the_first_drops_it() {
        // `flock` is per open file description, so two handles in one process
        // contend exactly as two processes do: this is the real property, not
        // a stand-in for it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("store.lock");

        let held = exclusive(&path, Duration::ZERO).expect("take the lock");
        assert_eq!(held.path(), path);
        assert!(
            try_exclusive(&path).is_none(),
            "a second holder must not get a lock the first one has"
        );

        // A bounded wait that expires is a `None`, not a hang and not an
        // error: the caller proceeds unlocked.
        let started = Instant::now();
        assert!(exclusive(&path, RETRY_INTERVAL * 5).is_none());
        assert!(
            started.elapsed() >= RETRY_INTERVAL * 5,
            "the wait must actually wait: {:?}",
            started.elapsed()
        );

        // Release is the descriptor closing, so nothing has to be cleaned up.
        // Still a wait: a process another test is spawning holds a copy of the
        // descriptor, and so the lock, until it reaches `exec`.
        drop(held);
        assert!(
            exclusive(&path, Duration::from_secs(5)).is_some(),
            "dropping the guard must release the lock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_created_owner_only_and_never_truncated() {
        // Its contents are the caller's (the scheduler records the holding
        // pid so a second daemon can name it), so reopening must not wipe
        // them, and another local user must not be able to swap the file out
        // from under a lock Wizard is holding.
        use std::io::Write;

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("daemon.lock");
        {
            let mut file = crate::platform::secrets::open_private_file(&path).expect("create");
            file.write_all(b"4242").expect("write the holder pid");
        }
        let held = exclusive(&path, Duration::ZERO).expect("take the lock");
        drop(held);
        assert_eq!(std::fs::read(&path).expect("read"), b"4242");
        assert!(
            crate::platform::secrets::is_private_file(&path).expect("stat"),
            "the lock file is {}",
            crate::platform::secrets::protection_summary(&path)
        );
    }

    #[test]
    fn a_lock_file_that_cannot_be_opened_is_a_warning_not_a_failure() {
        // The parent is a regular file, so the open fails. Every caller's
        // fallback is "proceed unlocked", which is exactly what `None` means.
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("write");
        assert!(exclusive(&blocker.join("store.lock"), Duration::ZERO).is_none());
        assert!(try_exclusive(&blocker.join("store.lock")).is_none());
    }
}
