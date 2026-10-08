//! Private files and directories: the platform's answer to "only this user
//! may read this".
//!
//! On Unix that answer is mode bits, 0600 for a file and 0700 for a directory,
//! and `credentials.toml` at 0600 is the whole of Wizard's secret storage: no
//! keyring, no encryption, just an owner-only file. `~/.wizard` holds session
//! transcripts, logs and OAuth tokens beside it, so the directory modes carry
//! the same weight. Windows expresses none of this with mode bits; the port
//! is an ACL (or DPAPI for the credential file itself), which is why every
//! function here takes a path and a *purpose* rather than a mode.
//!
//! ## Two failure policies, on purpose
//!
//! [`create_private_dir`] treats a `chmod` failure as a warning;
//! [`create_private_dir_strict`] treats it as an error. That split is
//! deliberate and predates this module:
//!
//! - Best effort is for the state tree ([`crate::config::Config::ensure_dirs`]
//!   runs on every load). `WIZARD_HOME` is explicitly supported, and it is how
//!   Termux, CI sandboxes and second installs relocate the tree. On exFAT,
//!   FAT32, a CIFS/NFS mount without POSIX modes, or WSL DrvFs,
//!   `set_permissions` returns EPERM/ENOTSUP: those filesystems cannot express
//!   the hardening at all, and refusing to start in exchange for a mode bit
//!   they will never carry is the wrong trade. The directory is still created,
//!   and *that* failing is still an error.
//! - Strict is for directories whose looseness is worse than their absence:
//!   the update staging dir (its contents are the argument to `sudo install`)
//!   and any directory a secret file is about to be written into. If the
//!   filesystem cannot keep an API key away from other local users, not
//!   writing the key is the correct outcome.
//!
//! Secret *files* ([`create_private_file`], [`write_private_atomic`]) are
//! always strict, for the second reason.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};

/// Mode for a file only its owner may read: `rw-------`.
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Mode for a directory only its owner may enter: `rwx------`.
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;

/// How old a leftover scratch file has to be before a later write treats it as
/// debris rather than as somebody else's write in flight.
///
/// An atomic write takes milliseconds. An hour is not a race; it is a process
/// that died between `create` and `rename`.
const STALE_SCRATCH_AGE: Duration = Duration::from_secs(60 * 60);

/// Serial number for scratch file names, so two writes from one process never
/// pick the same one. Process-global rather than per-path: the name also
/// carries the target's file name, so a single counter cannot collide.
static SCRATCH_SERIAL: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------------

/// Create `dir` and its parents, private to this user. A creation failure is
/// an error; a hardening failure is a logged warning (see the module docs for
/// why).
pub fn create_private_dir(dir: &Path) -> Result<()> {
    create_private_dir_with(dir, restrict_dir)
}

/// Testable core of [`create_private_dir`]: `restrict` tightens the mode of an
/// existing directory, or reports why it could not.
///
/// Injectable because the interesting case, a filesystem that cannot express
/// the hardening, is one no test can produce on a normal box.
pub fn create_private_dir_with(
    dir: &Path,
    restrict: impl Fn(&Path) -> std::io::Result<()>,
) -> Result<()> {
    create_dir_all_private(dir)?;
    if let Err(err) = restrict(dir) {
        tracing::warn!(
            "could not restrict permissions on {} ({err}); \
             the directory is readable by other users on this filesystem",
            dir.display()
        );
    }
    Ok(())
}

/// Create `dir` and its parents, private to this user, refusing to continue
/// when the platform cannot make it private.
///
/// For directories where a loose mode is worse than no directory at all: the
/// update staging dir, and the parent of any secret file.
pub fn create_private_dir_strict(dir: &Path) -> Result<()> {
    create_dir_all_private(dir)?;
    restrict_dir(dir).with_context(|| format!("restricting permissions on {}", dir.display()))?;
    Ok(())
}

/// `create_dir_all`, asking the OS for private permissions at creation time.
///
/// The mode is set as the directory is created rather than chmod'd afterwards
/// so there is no window in which it exists at the process umask (0755 on a
/// stock distro) with a secret already being written into it. `restrict_dir`
/// still runs afterwards, because a *pre-existing* directory keeps whatever
/// mode it had: that is what tightens a tree an older release left loose.
fn create_dir_all_private(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR_MODE)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(())
}

/// Tighten an existing directory to owner-only.
///
/// The Windows arm is not "do nothing": it is a DACL granting the current user
/// and nothing else, replacing the inherited one. Until that exists, reporting
/// success would claim a protection this platform is not providing, so the
/// seam returns `Unsupported` and the caller's policy (warn or fail) decides.
fn restrict_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Err(unsupported())
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Create `path` as a fresh owner-only file and hand back the open handle.
///
/// The file must not already exist (`O_EXCL`): opening it that way refuses to
/// follow a symlink, so a name someone else planted in the destination
/// directory fails the write instead of redirecting it. Callers that overwrite
/// go through [`write_private_atomic`], which removes its own scratch file
/// first.
pub fn create_private_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_FILE_MODE);
    }
    let file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    // The open mode is masked by the process umask (0600 becomes 0400 under an
    // exotic one), so pin the mode explicitly: callers and `wizard doctor`
    // assert on an exact 0600, not on "no wider than 0600".
    restrict_open_file(&file)
        .with_context(|| format!("restricting permissions on {}", path.display()))?;
    Ok(file)
}

/// Open `path` for writing, asking the OS to create it owner-only when it is
/// not there and leaving an existing file's contents alone.
///
/// [`create_private_file`] refuses a name that already exists, which is right
/// for a secret being written once and wrong for a file whose whole job is to
/// outlive the run that made it: a lock file ([`super::lockfile`]) is created
/// by whichever wizard starts first and then reopened by every later one, so
/// `O_EXCL` would mean only the first process could ever take the lock.
///
/// The mode is a *creation-time* request and is deliberately not re-pinned
/// afterwards, unlike [`create_private_file`]. Nothing here holds a secret:
/// the mode only keeps another local user from truncating a lock Wizard is
/// holding, and re-pinning would turn a filesystem that cannot express modes
/// into a filesystem where the lock cannot be taken at all, which is strictly
/// worse than an unprotected lock file.
pub fn open_private_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_FILE_MODE);
    }
    options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

/// Tighten an existing file to owner-only. Errors when the platform cannot:
/// the caller is about to treat the file as secret.
pub fn harden_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .with_context(|| format!("restricting permissions on {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        Err(anyhow!(unsupported())
            .context(format!("restricting permissions on {}", path.display())))
    }
}

/// Tighten an already-open file, by handle rather than by path (no window in
/// which the name could be swapped between create and chmod).
fn restrict_open_file(file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Err(unsupported())
    }
}

/// Write `data` to `path` atomically, owner-only, creating the parent
/// directory (also owner-only) if it is missing.
///
/// The sequence is: private parent, private scratch file beside the target,
/// write, fsync, rename over the target. The scratch file is in the *same*
/// directory so the rename cannot cross a filesystem, and the fsync is what
/// makes the rename safe across a power cut: without it the rename can land
/// while the contents are still only in the page cache. A reader therefore
/// sees either the whole old file or the whole new one, never a truncated
/// half-written secret.
///
/// This is the one private-write primitive: `credentials.toml`, the xAI OAuth
/// tokens and the sync key each used to carry their own copy of it.
pub fn write_private_atomic(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_with(path, data, Visibility::Private, STALE_SCRATCH_AGE)
}

/// The same sequence at ordinary permissions, for state that is not a secret
/// but must never be found half-written.
///
/// `config.toml` is the reason this exists: it was a truncating `fs::write`,
/// run on every `/settings`, `/mode` and `/vim` and on every provider change,
/// and a crash inside that window left a file that does not parse — after
/// which wizard refuses to start, because the config it needs to start is the
/// one that was being written. Everything else that keeps state in `~/.wizard`
/// already writes through a scratch file and a rename.
///
/// Ordinary permissions, not owner-only: the file keeps whatever the umask
/// gives it, so a config the user chmod'd for a shared tool is not silently
/// tightened. The *parent* is still created private, warn-only, because these
/// callers all live under `~/.wizard` — which the strict variant would refuse
/// to create on a platform that cannot express the mode, and losing settings
/// saves entirely there is worse than the loose directory.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_with(path, data, Visibility::Umask, STALE_SCRATCH_AGE)
}

/// Publish `data` as `path` by writing a scratch file beside it and renaming
/// it over the destination. The parent must already exist.
///
/// [`write_atomic`] also tightens that parent to owner-only, which is right
/// for `~/.wizard` and wrong for a project's `.wizard`: `mission.toml` lives
/// next to work the user may be sharing. This leaves the directory's mode
/// alone. A reader that already has the file open keeps the previous inode
/// until the new one is complete, so a kill mid-write cannot leave a
/// truncated file where a whole one was.
pub fn replace_file(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_with(path, data, Visibility::AsIs, STALE_SCRATCH_AGE)
}

/// Whether an atomic write produces an owner-only file or an ordinary one,
/// and whether it may create or chmod the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Private,
    Umask,
    /// Parent already exists. Do not create it and do not chmod it.
    AsIs,
}

/// [`write_private_atomic`] with the scratch-file janitor's threshold
/// injected, because a test cannot wait an hour and cannot set a file's mtime
/// without another dependency.
fn write_atomic_with(
    path: &Path,
    data: &[u8],
    visibility: Visibility,
    stale_after: Duration,
) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("{} has no usable file name", path.display()))?;
    match visibility {
        Visibility::Private => create_private_dir_strict(dir)?,
        Visibility::Umask => create_private_dir(dir)?,
        Visibility::AsIs => {
            if !dir.is_dir() {
                anyhow::bail!(
                    "{} does not exist; replace_file does not create directories",
                    dir.display()
                );
            }
        }
    }

    // Hidden, pid-tagged and serial-tagged. The pid keeps two processes apart
    // (both storing a key at once would otherwise share one scratch name); the
    // serial keeps two *threads of one process* apart, which is not
    // hypothetical: `credentials::store` is reached from an axum handler on
    // the multi-thread runtime, so two Save clicks in two browser tabs run
    // this concurrently with the same pid. Sharing the name there meant one
    // writer could unlink the other's file and rename its own empty one over
    // `credentials.toml`.
    let tmp = dir.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        SCRATCH_SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    // A crash between create and rename leaves the scratch file behind, and
    // `create_new` below would refuse that name forever. Removing it first is
    // safe: no *live* process can be using this name, because it carries our
    // own pid, so the only file this can unlink is debris left by a dead
    // process whose pid the kernel has since handed to us.
    let _ = std::fs::remove_file(&tmp);
    // That only covers the one name we are about to take. Everything else the
    // dead process left behind is swept here.
    sweep_stale_scratch(dir, file_name, stale_after);

    let write = || -> Result<()> {
        use std::io::Write;
        let mut file = match visibility {
            Visibility::Private => create_private_file(&tmp)?,
            // `create_new`, like the private variant, so a symlink planted at
            // this name is refused rather than followed.
            Visibility::Umask | Visibility::AsIs => std::fs::File::create_new(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?,
        };
        file.write_all(data)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        Ok(())
    };
    if let Err(err) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }

    std::fs::rename(&tmp, path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!(err).context(format!("moving {} into place", path.display()))
    })?;
    Ok(())
}

/// Remove scratch files for `file_name` in `dir` that are older than
/// `stale_after`.
///
/// The unique scratch name is what keeps two writers apart, and the price of a
/// name nothing reuses is that a crash between `create` and `rename` leaves a
/// complete copy of the secret on disk with nobody to clean it up. It is mode
/// 0600, so this is not a disclosure, but `wizard doctor` enumerates a fixed
/// list of paths and would never mention it, and ten crashes leave ten copies
/// of every stored API key. So each write sweeps: a write in flight lives for
/// milliseconds, and anything older than `stale_after` belongs to a process
/// that is not coming back.
///
/// Best effort throughout. A directory that cannot be read, or an entry that
/// cannot be removed, is not a reason to fail the write this precedes.
fn sweep_stale_scratch(dir: &Path, file_name: &str, stale_after: Duration) {
    let prefix = format!(".{file_name}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // `.<target>.<something>.tmp`, with the something non-empty: that is
        // this function's own naming and nothing else's. The fixed
        // `.<target>.tmp` an older release used is deliberately *not* matched,
        // because it is also the name another program could be using.
        if !name.starts_with(&prefix)
            || !name.ends_with(".tmp")
            || name.len() <= prefix.len() + ".tmp".len()
        {
            continue;
        }
        // Never follows a link: the age that matters is the scratch entry's,
        // and `remove_file` on a symlink removes the link rather than whatever
        // it points at.
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.is_dir() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= stale_after);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// Whether `path` is protected from other local users.
///
/// The question `wizard doctor` asks, phrased so both platforms can answer it:
/// on Unix "no group or other bits", on Windows "the DACL names this user and
/// nobody else". Follows symlinks, because what matters is the mode of the
/// file the secret is actually stored in. `Err` when the path cannot be
/// stat'd. An unreadable path is not a protected one, but it is also not a
/// question this can answer, and a doctor check that reports "protected" for a
/// path it never saw is worse than one that reports the stat error.
pub fn is_protected(path: &Path) -> Result<bool> {
    let meta = std::fs::metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(meta.permissions().mode() & 0o077 == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Err(anyhow!(unsupported()).context(format!("inspecting {}", path.display())))
    }
}

/// Whether `path` carries exactly the protection [`create_private_file`] gives
/// a secret, rather than merely being no wider than it.
///
/// [`is_protected`] is the question `wizard doctor` asks of a user's tree,
/// where anything with no group or other bits is fine however it got that way.
/// This is the stricter question the tests of a *writer* ask: `credentials.rs`,
/// `trust.rs` and the OAuth token stores all claim to produce an exact 0600,
/// and under a hostile umask an unpinned open produces 0400, which passes
/// "protected" and is a file Wizard can no longer rewrite. Checking the
/// stronger claim is the only way that regression is visible.
pub fn is_private_file(path: &Path) -> Result<bool> {
    let meta = std::fs::metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(meta.permissions().mode() & 0o777 == PRIVATE_FILE_MODE)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Err(anyhow!(unsupported()).context(format!("inspecting {}", path.display())))
    }
}

/// [`is_private_file`] for a directory: exactly the protection
/// [`create_private_dir`] produces, not merely one with no group or other
/// bits.
pub fn is_private_dir(dir: &Path) -> Result<bool> {
    let meta = std::fs::metadata(dir).with_context(|| format!("inspecting {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(meta.permissions().mode() & 0o777 == PRIVATE_DIR_MODE)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Err(anyhow!(unsupported()).context(format!("inspecting {}", dir.display())))
    }
}

/// Hand `path` to other local users: the state an older release, a restored
/// backup or a careless `chmod` leaves behind.
///
/// Test-only, and public because it is the *input* to a dozen tests spread
/// across `credentials`, `doctor`, `logging`, `trust` and `gateway`: every one
/// of them asks "does Wizard notice a path someone else can read, and tighten
/// it?", and every one of them used to spell the setup as a raw
/// `set_permissions(.., 0o644)`. That spelling is the reason a Windows port
/// would have had to read those files at all, since the property under test is
/// "not private", not "0644".
///
/// A directory gets the traversable equivalent, because a directory nobody can
/// enter is not the condition any of those tests mean to create.
#[cfg(test)]
pub fn expose_to_other_users(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta =
            std::fs::metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
        let mode = if meta.is_dir() { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("loosening permissions on {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        Err(anyhow!(unsupported()).context(format!("loosening permissions on {}", path.display())))
    }
}

/// How a failing test should describe `path`'s protection: `"0755"` on Unix,
/// a DACL summary once Windows lands.
///
/// Test-only. It exists so an assertion message can say what it saw without
/// the test itself knowing that "what it saw" is a mode on this platform; a
/// path that cannot be stat'd describes itself as the error, because a message
/// is worth less than the run it would otherwise abort.
#[cfg(test)]
pub fn protection_summary(path: &Path) -> String {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => return format!("cannot inspect {}: {err}", path.display()),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        format!("{:04o}", meta.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        "a DACL this platform cannot yet describe".to_string()
    }
}

/// The error the not-yet-written platform arms report. Kept as one function so
/// the port has one place to grep for.
#[cfg(not(unix))]
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "owner-only permissions are not implemented on this platform yet",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .unwrap_or_else(|err| panic!("stat {}: {err}", path.display()))
            .permissions()
            .mode()
            & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn a_private_dir_is_0700_whether_it_is_new_or_already_loose() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");

        // Fresh, including a parent that does not exist yet: the whole chain
        // is created private, not created at the umask and tightened after.
        let fresh = tmp.path().join("outer").join("inner");
        create_private_dir(&fresh).expect("create");
        assert_eq!(mode_of(&fresh), 0o700);
        assert_eq!(mode_of(&tmp.path().join("outer")), 0o700);

        // Pre-existing and world-readable (an older release's tree): the
        // second call has to tighten it.
        let loose = tmp.path().join("loose");
        std::fs::create_dir(&loose).expect("mkdir");
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        create_private_dir(&loose).expect("re-create");
        assert_eq!(mode_of(&loose), 0o700);
    }

    #[test]
    fn a_chmod_that_cannot_work_is_a_warning_not_a_failure() {
        // The exFAT/CIFS/DrvFs case, which no test can produce for real: the
        // directory must exist afterwards and the call must succeed.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("state").join("nested");
        let result = create_private_dir_with(&dir, |_| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });
        assert!(result.is_ok(), "a chmod failure must not abort startup");
        assert!(dir.is_dir(), "the directory itself must still be created");

        // A creation failure is still an error: `dir` is now a file.
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        assert!(create_private_dir_with(&file, |_| Ok(())).is_err());
        assert!(create_private_dir_strict(&file).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_atomic_private_write_leaves_an_exact_0600_file_and_no_scratch() {
        // Runs at whatever umask the test runner inherited, which is the
        // uninteresting case; the umask this module has to survive is pinned
        // by `the_file_mode_comes_from_this_module_not_the_umask` below, in a
        // child process, because umask is per-process state and this suite is
        // threaded.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nested").join("credentials.toml");

        write_private_atomic(&path, b"first").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"first");
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(path.parent().expect("parent")), 0o700);

        // Overwriting is the common case (every credential store rewrites the
        // whole file) and must not trip over its own scratch file.
        write_private_atomic(&path, b"second").expect("rewrite");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");

        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "credentials.toml")
            .collect();
        assert!(
            leftovers.is_empty(),
            "scratch file left behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_scratch_file_left_by_a_dead_process_is_swept_and_never_wedges_a_write() {
        // The realistic crash: a *different* process died between `create` and
        // `rename` and left a full copy of the secret behind under its own pid.
        // Nothing reuses that name, so without the sweep it is permanent, and
        // ten crashes leave ten copies of every stored key.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("credentials.toml");
        let dead_pid = std::process::id().wrapping_add(1);
        let debris = tmp
            .path()
            .join(format!(".credentials.toml.{dead_pid}.7.tmp"));
        let planted = |file: &Path| std::fs::write(file, b"a whole copy of the key store");
        planted(&debris).expect("plant debris");

        // A fresh scratch file is somebody's write in flight, not debris: the
        // hour-long threshold is what tells them apart, and sweeping the wrong
        // one would break a concurrent writer's rename.
        write_private_atomic(&path, b"first").expect("write");
        assert!(
            debris.exists(),
            "a scratch file younger than the threshold must be left alone"
        );

        // Past the threshold (injected as zero, since the test cannot wait an
        // hour), the same file is debris and goes.
        write_atomic_with(&path, b"second", Visibility::Private, Duration::ZERO).expect("write");
        assert!(!debris.exists(), "stale scratch files must be swept");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");

        // And a neighbour that is not this module's scratch naming is never
        // touched, whatever its age: the sweep only owns names it writes.
        let neighbour = tmp.path().join(".credentials.toml.tmp");
        planted(&neighbour).expect("plant the old fixed name");
        let unrelated = tmp.path().join("notes.txt");
        planted(&unrelated).expect("plant an unrelated file");
        write_atomic_with(&path, b"third", Visibility::Private, Duration::ZERO).expect("write");
        assert!(neighbour.exists(), "the old fixed scratch name is not ours");
        assert!(unrelated.exists());
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_writes_in_one_process_never_publish_a_spliced_file() {
        // `credentials::store` is reached from an axum handler on the
        // multi-thread runtime, so two Save clicks (two browser tabs, or one
        // double-submitted form) run this at the same instant with the same
        // pid. When the scratch name carried only the pid, they shared one
        // file: one writer unlinked the other's, and the loser's `rename`
        // either published a zero-byte `credentials.toml` or failed ENOENT
        // after its key had in fact been written.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("credentials.toml");
        // Large enough that one `write_all` is many syscalls: a splice is then
        // visible as a mixed file rather than needing a lucky interleaving.
        let payloads: Vec<Vec<u8>> = (0..8u8).map(|n| vec![b'a' + n; 256 * 1024]).collect();

        std::thread::scope(|scope| {
            for payload in &payloads {
                scope.spawn(|| write_private_atomic(&path, payload).expect("write"));
            }
        });

        let landed = std::fs::read(&path).expect("read");
        assert!(
            payloads.contains(&landed),
            "the published file is not any one writer's: {} bytes starting {:?}",
            landed.len(),
            landed.first()
        );
        assert_eq!(mode_of(&path), 0o600);

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "credentials.toml")
            .collect();
        assert!(leftovers.is_empty(), "scratch left behind: {leftovers:?}");
    }

    /// The non-secret variant publishes whole files too, and leaves the mode
    /// to the umask.
    ///
    /// `config.toml` is what needs this: it was a truncating `fs::write` run
    /// on every settings change, so a crash inside that window left a file
    /// that does not parse and a wizard that refuses to start.
    #[cfg(unix)]
    #[test]
    fn write_atomic_publishes_whole_files_at_ordinary_permissions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nested").join("config.toml");

        write_atomic(&path, b"model = \"a\"\n").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"model = \"a\"\n");
        // Not owner-only: a config is not a secret, and silently tightening a
        // file the user may have chmod'd for another tool is not this
        // function's business.
        assert_ne!(mode_of(&path), 0o600, "the umask decides, not this module");

        // Overwriting works (the scratch name is removed first) and never
        // leaves the target missing or half-written.
        let payloads: Vec<Vec<u8>> = (0..8u8).map(|n| vec![b'a' + n; 256 * 1024]).collect();
        std::thread::scope(|scope| {
            for payload in &payloads {
                scope.spawn(|| write_atomic(&path, payload).expect("write"));
            }
        });
        let landed = std::fs::read(&path).expect("read");
        assert!(
            payloads.contains(&landed),
            "the published file is not any one writer's: {} bytes",
            landed.len()
        );

        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "scratch left behind: {leftovers:?}");
    }

    /// Child half of [`the_file_mode_comes_from_this_module_not_the_umask`]:
    /// sets a hostile umask and reports what [`create_private_file`] produced
    /// under it. Inert unless the parent set [`UMASK_PROBE_ENV`], because
    /// `umask` is per-process state and this suite runs its tests on threads.
    #[cfg(unix)]
    #[test]
    fn umask_probe() {
        let Some(dir) = std::env::var_os(UMASK_PROBE_ENV) else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        // 0277 clears group and other entirely and takes *write* away from the
        // owner, so an unpinned 0600 open lands as 0400.
        // SAFETY: `umask` takes a mode, returns the old one and cannot fail.
        // This process exists only to run this probe.
        unsafe { libc::umask(0o277) };

        // The control: an ordinary create, which is what the umask does to a
        // file this module does not pin. Without it a probe that reported 0600
        // could mean "the umask never applied" rather than "the mode was
        // pinned".
        let control = dir.join("control");
        drop(std::fs::File::create(&control).expect("create the control file"));
        println!("{UMASK_PROBE_PREFIX}control={:o}", mode_of(&control));

        let secret = dir.join("secret");
        drop(create_private_file(&secret).expect("create the private file"));
        println!("{UMASK_PROBE_PREFIX}secret={:o}", mode_of(&secret));

        // Through the whole atomic write, into a directory it has to create:
        // `DirBuilder::mode(0700)` is masked the same way, so the directory
        // arm needs the same explicit re-pin.
        let nested = dir.join("nested");
        let written = nested.join("credentials.toml");
        write_private_atomic(&written, b"payload").expect("write");
        println!("{UMASK_PROBE_PREFIX}written={:o}", mode_of(&written));
        println!("{UMASK_PROBE_PREFIX}dir={:o}", mode_of(&nested));
    }

    #[cfg(unix)]
    const UMASK_PROBE_ENV: &str = "WIZARD_SECRETS_UMASK_PROBE_DIR";
    #[cfg(unix)]
    const UMASK_PROBE_PREFIX: &str = "umask-probe ";

    #[cfg(unix)]
    #[test]
    fn the_file_mode_comes_from_this_module_not_the_umask() {
        // The reason `restrict_open_file` exists at all (secrets.rs:
        // "the open mode is masked by the process umask, so pin the mode
        // explicitly"). `wizard doctor` and `is_protected` assert an exact
        // 0600, and under a 0277 umask an unpinned open produces 0400, which
        // is a file Wizard cannot rewrite.
        let tmp = tempfile::tempdir().expect("tempdir");
        // The probe also writes into `<dir>/nested`, so the directory arm is
        // covered by the same run.
        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "platform::secrets::tests::umask_probe",
                "--nocapture",
            ])
            .env(UMASK_PROBE_ENV, tmp.path())
            .output()
            .expect("run the umask probe");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "probe failed:\n{stdout}");
        let reported = |key: &str| -> String {
            let needle = format!("{UMASK_PROBE_PREFIX}{key}=");
            stdout
                .lines()
                .find_map(|line| line.strip_prefix(&needle))
                .unwrap_or_else(|| panic!("probe printed no {key}:\n{stdout}"))
                .trim()
                .to_string()
        };

        assert_eq!(
            reported("control"),
            "400",
            "the probe's umask never applied, so nothing below is evidence:\n{stdout}"
        );
        assert_eq!(reported("secret"), "600", "{stdout}");
        assert_eq!(reported("written"), "600", "{stdout}");
        assert_eq!(reported("dir"), "700", "{stdout}");
    }

    #[cfg(unix)]
    #[test]
    fn create_private_file_refuses_an_existing_name() {
        // O_EXCL is the anti-symlink guard: a planted name must fail the write
        // rather than redirect it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("secret");
        let file = create_private_file(&path).expect("create");
        drop(file);
        assert_eq!(mode_of(&path), 0o600);
        assert!(create_private_file(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn harden_file_tightens_a_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("was-loose");
        std::fs::write(&path, b"secret").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(!is_protected(&path).expect("stat"));

        harden_file(&path).expect("harden");
        assert_eq!(mode_of(&path), 0o600);
        assert!(is_protected(&path).expect("stat"));
    }

    #[cfg(unix)]
    #[test]
    fn the_exact_predicates_reject_what_is_protected_accepts() {
        // The distinction the writer-side tests depend on: 0400 has no group
        // or other bits, so `is_protected` says yes, and it is nevertheless a
        // credentials file Wizard can no longer rewrite. A test that could
        // only ask `is_protected` would stay green if the umask re-pin in
        // `create_private_file` were deleted.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("secret");
        drop(create_private_file(&path).expect("create"));
        assert!(is_private_file(&path).expect("stat"));
        assert!(is_protected(&path).expect("stat"));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).expect("chmod");
        assert!(is_protected(&path).expect("stat"), "0400 hides nothing");
        assert!(
            !is_private_file(&path).expect("stat"),
            "0400 is not the mode this module writes"
        );

        let dir = tmp.path().join("private");
        create_private_dir(&dir).expect("create");
        assert!(is_private_dir(&dir).expect("stat"));
        assert!(!is_private_file(&dir).expect("stat"), "0700 is not 0600");

        // Both report the stat failure rather than answering "not private"
        // for a path they never saw.
        let absent = tmp.path().join("absent");
        assert!(is_private_file(&absent).is_err());
        assert!(is_private_dir(&absent).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn exposing_a_path_is_the_inverse_of_hardening_it_for_both_kinds() {
        // The setup step a dozen tests elsewhere need: a file or directory
        // some other local user can read, described as that rather than as a
        // mode. A directory has to stay traversable, or the tests that then
        // read something *through* it would fail for the wrong reason.
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("was-private");
        drop(create_private_file(&file).expect("create"));
        let dir = tmp.path().join("dir");
        create_private_dir(&dir).expect("create");
        let inner = dir.join("reachable");
        std::fs::write(&inner, b"x").expect("write");

        expose_to_other_users(&file).expect("expose the file");
        expose_to_other_users(&dir).expect("expose the dir");
        assert!(!is_protected(&file).expect("stat"));
        assert!(!is_protected(&dir).expect("stat"));
        assert_eq!(std::fs::read(&inner).expect("read through"), b"x");

        harden_file(&file).expect("harden");
        assert!(is_private_file(&file).expect("stat"));
        create_private_dir(&dir).expect("re-tighten");
        assert!(is_private_dir(&dir).expect("stat"));

        // The failure message a test prints has to name what it saw.
        assert_eq!(protection_summary(&file), "0600");
    }

    #[cfg(unix)]
    #[test]
    fn open_private_file_creates_owner_only_and_then_reopens_the_same_file() {
        // The lock-file shape: created once, reopened by every later process.
        // `create_private_file` cannot serve it, because O_EXCL means only the
        // first process could ever open it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("scheduler.lock");
        {
            use std::io::Write;
            let mut file = open_private_file(&path).expect("create");
            file.write_all(b"1234").expect("write");
        }
        assert_eq!(mode_of(&path), 0o600);

        // Reopening neither fails nor truncates: the pid an earlier daemon
        // recorded is still there for the error message that names the holder.
        let reopened = open_private_file(&path).expect("reopen");
        drop(reopened);
        assert_eq!(std::fs::read(&path).expect("read"), b"1234");
    }

    #[cfg(unix)]
    #[test]
    fn is_protected_answers_for_directories_and_reports_a_missing_path() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("private");
        create_private_dir(&dir).expect("create");
        assert!(is_protected(&dir).expect("stat"));

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).expect("chmod");
        assert!(
            !is_protected(&dir).expect("stat"),
            "a group-readable directory is not protected"
        );

        // Never silently "protected": a path that cannot be inspected is an
        // error, so a doctor check cannot pass on a path it never saw.
        let err = is_protected(&tmp.path().join("absent")).expect_err("missing path");
        assert!(format!("{err:#}").contains("inspecting"), "got: {err:#}");
    }
}
