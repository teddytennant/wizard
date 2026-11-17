//! Where this copy of Wizard GUI lives, and how a verified release replaces it.
//!
//! - **MacApp**: an `.app` bundle in a directory this account can write. The
//!   dmg is mounted, the new bundle copied out and checked (bundle id and
//!   version), then copied next to the installed one and swapped in with one
//!   atomic rename. The old bundle stays beside it as a hidden backup.
//! - **Linux**: the layout the tarball's `install.sh` writes, versioned
//!   directories under `~/.local/share/wizard-gui` with a `current` symlink
//!   that `~/.local/bin/wizard-gui` points through. An update unpacks the next
//!   version beside the running one and repoints `current`; the previous
//!   version is kept. A copy installed by the older `install.sh` (a plain file
//!   in `~/.local/bin`) is moved to this layout by its first update.
//! - **WindowsPortable**: the portable zip's `zeron.exe` (see `windows.rs`).
//! - **Manual**: anything else, with the reason. The UI offers the release
//!   page instead of failing quietly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};

use crate::feed::{self, Release};

/// A staged update, verified and ready for [`InstallKind::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub version: String,
    /// The staged bundle (macOS), version directory (Linux) or executable
    /// (Windows).
    pub path: PathBuf,
}

/// Download progress for the UI's install bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub status: String,
    /// The whole update, 0.0 to 1.0.
    pub fraction: f32,
    pub detail: Option<String>,
}

impl Progress {
    fn new(status: &str, fraction: f32) -> Self {
        Self {
            status: status.to_string(),
            fraction,
            detail: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    MacApp {
        bundle: PathBuf,
    },
    Linux {
        /// Holds `<version>/` directories and the `current` symlink.
        root: PathBuf,
        /// A pre-updater `~/.local/bin/wizard-gui` to turn into a symlink.
        legacy_binary: Option<PathBuf>,
    },
    #[cfg(windows)]
    WindowsPortable {
        directory: PathBuf,
    },
    /// Can't update itself; `reason` says why, in words for the user.
    Manual {
        reason: String,
    },
}

impl InstallKind {
    pub fn can_self_update(&self) -> bool {
        !matches!(self, Self::Manual { .. })
    }

    pub fn manual_reason(&self) -> Option<&str> {
        match self {
            Self::Manual { reason } => Some(reason),
            _ => None,
        }
    }

    /// Where the app is installed, for Settings.
    pub fn location(&self) -> Option<&Path> {
        match self {
            Self::MacApp { bundle } => Some(bundle),
            Self::Linux {
                legacy_binary: Some(binary),
                ..
            } => Some(binary),
            Self::Linux { root, .. } => Some(root),
            #[cfg(windows)]
            Self::WindowsPortable { directory } => Some(directory),
            Self::Manual { .. } => None,
        }
    }

    /// Download `release` for this platform, verify it, and unpack it where
    /// [`Self::apply`] expects it. Re-running after a finished stage is cheap.
    pub async fn stage(
        &self,
        release: &Release,
        data_dir: &Path,
        progress: &(dyn Fn(Progress) + Send + Sync),
    ) -> Result<Staged> {
        if let Self::Manual { reason } = self {
            bail!("{reason}");
        }
        let version = release.version.to_string();
        if let Some(staged) = self.already_staged(&version, data_dir).await {
            return Ok(staged);
        }
        let asset = release.asset_for_this_platform()?;
        let downloads = data_dir.join("updates").join(&version);
        std::fs::create_dir_all(&downloads)
            .with_context(|| format!("creating {}", downloads.display()))?;
        let archive = downloads.join(&asset.name);
        progress(Progress::new("Downloading…", 0.0));
        feed::download(&asset, &archive, |seen, total| {
            progress(Progress {
                status: "Downloading…".into(),
                fraction: total.map_or(0.0, |total| 0.8 * seen as f32 / total.max(1) as f32),
                detail: Some(match total {
                    Some(total) => format!("{} of {}", megabytes(seen), megabytes(total)),
                    None => megabytes(seen),
                }),
            })
        })
        .await?;
        progress(Progress::new("Verifying and unpacking…", 0.85));
        let kind = self.clone();
        let staged = tokio::task::spawn_blocking({
            let archive = archive.clone();
            let downloads = downloads.clone();
            let version = version.clone();
            move || kind.unpack(&archive, &downloads, &version)
        })
        .await
        .context("the unpack task panicked")?;
        let _ = std::fs::remove_file(&archive);
        let staged = staged?;
        progress(Progress::new("Ready to install", 1.0));
        Ok(staged)
    }

    async fn already_staged(&self, version: &str, data_dir: &Path) -> Option<Staged> {
        let kind = self.clone();
        let (version, data_dir) = (version.to_string(), data_dir.to_path_buf());
        tokio::task::spawn_blocking(move || match &kind {
            Self::MacApp { bundle } => {
                let staged = data_dir
                    .join("updates")
                    .join(&version)
                    .join(bundle_name(bundle));
                let id = read_bundle_info(bundle).ok()?.id;
                verify_bundle(&staged, &id, &version).ok()?;
                Some(Staged {
                    version,
                    path: staged,
                })
            }
            Self::Linux { root, .. } => {
                let dir = root.join(&version);
                verify_linux_build(&dir, &version).ok()?;
                Some(Staged { version, path: dir })
            }
            _ => None,
        })
        .await
        .ok()
        .flatten()
    }

    fn unpack(&self, archive: &Path, downloads: &Path, version: &str) -> Result<Staged> {
        match self {
            Self::MacApp { bundle } => {
                let id = read_bundle_info(bundle)
                    .context("reading the installed app's Info.plist")?
                    .id;
                let path = stage_mac_dmg(archive, downloads, &bundle_name(bundle), &id, version)?;
                Ok(Staged {
                    version: version.into(),
                    path,
                })
            }
            Self::Linux { root, .. } => Ok(Staged {
                version: version.into(),
                path: stage_linux_tarball(archive, root, version)?,
            }),
            #[cfg(windows)]
            Self::WindowsPortable { directory } => Ok(Staged {
                version: version.into(),
                path: crate::windows::stage(archive, directory, version)?,
            }),
            Self::Manual { reason } => bail!("{reason}"),
        }
    }

    /// Put `staged` in place. With `relaunch`, a detached helper waits for
    /// this process to exit and starts the new version; the caller must quit
    /// after this returns.
    pub fn apply(&self, staged: &Staged, relaunch: bool) -> Result<()> {
        match self {
            Self::MacApp { bundle } => {
                let id = read_bundle_info(bundle)?.id;
                let check = |path: &Path| verify_bundle(path, &id, &staged.version);
                apply_mac(&staged.path, bundle, &check)?;
                // The staged copy under updates/<version> is spent.
                if let Some(dir) = staged.path.parent() {
                    let _ = std::fs::remove_dir_all(dir);
                }
                if relaunch {
                    relaunch_after_exit(&["/usr/bin/open".as_ref(), bundle.as_os_str()]);
                }
                Ok(())
            }
            Self::Linux {
                root,
                legacy_binary,
            } => {
                let check = |dir: &Path| verify_linux_build(dir, &staged.version);
                apply_linux(root, &staged.version, legacy_binary.as_deref(), &check)?;
                if relaunch {
                    let exe = root.join("current").join(LINUX_BINARY);
                    relaunch_after_exit(&[exe.as_os_str()]);
                }
                Ok(())
            }
            #[cfg(windows)]
            Self::WindowsPortable { directory } => {
                crate::windows::apply(&staged.path, directory, relaunch)
            }
            Self::Manual { reason } => bail!("{reason}"),
        }
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

const LINUX_BINARY: &str = "wizard-gui";

/// Written beside the binary by `wizard gui` when it unpacks the copy built
/// into wizard (`src/desktop_app.rs`, `STAMP`). That copy is wizard's to
/// replace: `wizard update` brings a new one and the next `wizard gui`
/// unpacks it.
const WIZARD_STAMP: &str = "wizard-gui.embedded";

pub fn detect_install() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Manual {
            reason: "Wizard GUI can't tell where it is installed.".into(),
        };
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.as_ref().map(|home| home.join(".local/share")));
    detect(
        &exe,
        &Probe {
            os: std::env::consts::OS,
            home: home.as_deref(),
            data_home: data_home.as_deref(),
            appimage: std::env::var_os("APPIMAGE").is_some(),
            writable: &dir_writable,
        },
    )
}

/// Where the Linux installer puts versions: `$XDG_DATA_HOME/wizard-gui`.
pub fn linux_root(data_home: &Path) -> PathBuf {
    data_home.join("wizard-gui")
}

pub(crate) struct Probe<'a> {
    pub os: &'a str,
    pub home: Option<&'a Path>,
    pub data_home: Option<&'a Path>,
    pub appimage: bool,
    pub writable: &'a dyn Fn(&Path) -> bool,
}

fn manual(reason: impl Into<String>) -> InstallKind {
    InstallKind::Manual {
        reason: reason.into(),
    }
}

pub(crate) fn detect(exe: &Path, probe: &Probe<'_>) -> InstallKind {
    if exe
        .parent()
        .is_some_and(|dir| dir.join(WIZARD_STAMP).is_file())
    {
        return manual("Wizard GUI came with wizard; `wizard update` updates both.");
    }
    match probe.os {
        "macos" => detect_mac(exe, probe),
        "linux" => detect_linux(exe, probe),
        #[cfg(windows)]
        "windows" if crate::windows::is_managed(exe) => {
            let directory = exe.parent().unwrap().to_owned();
            if (probe.writable)(&directory) {
                InstallKind::WindowsPortable { directory }
            } else {
                manual(format!(
                    "Wizard GUI is in {}, which this account can't write to.",
                    directory.display()
                ))
            }
        }
        "windows" => manual(
            "This copy of Wizard GUI isn't the portable package, so it can't replace itself.",
        ),
        os => manual(format!("Wizard GUI can't update itself on {os}.")),
    }
}

fn detect_mac(exe: &Path, probe: &Probe<'_>) -> InstallKind {
    let Some(bundle) = exe
        .ancestors()
        .find(|dir| {
            dir.extension().is_some_and(|ext| ext == "app")
                && exe.starts_with(dir.join("Contents").join("MacOS"))
        })
        .map(Path::to_path_buf)
    else {
        return manual(
            "This copy of Wizard GUI isn't an app bundle (a source build?), so it can't replace itself.",
        );
    };
    let parent = bundle.parent().unwrap_or(Path::new("/"));
    if bundle.to_string_lossy().contains("/AppTranslocation/") {
        return manual(
            "macOS is running Wizard GUI from a temporary read-only copy. Move it to \
             Applications and open it from there, and it can update itself.",
        );
    }
    if (probe.writable)(parent) {
        return InstallKind::MacApp { bundle };
    }
    if parent.starts_with("/Volumes") {
        manual("Wizard GUI is running from its disk image. Drag it to Applications first.")
    } else {
        manual(format!(
            "Wizard GUI is in {}, which this account can't write to.",
            parent.display()
        ))
    }
}

fn detect_linux(exe: &Path, probe: &Probe<'_>) -> InstallKind {
    if probe.appimage {
        return manual("Wizard GUI is running as an AppImage, which it can't update in place.");
    }
    if exe.starts_with("/nix/store") {
        return manual("Wizard GUI was installed with Nix; update it through Nix.");
    }
    // `current_exe` resolves symlinks, so a managed install shows up as
    // `<root>/<version>/wizard-gui` with `<root>/current` beside it.
    if let Some(version_dir) = exe.parent()
        && let Some(root) = version_dir.parent()
        && exe.file_name().is_some_and(|name| name == LINUX_BINARY)
        && version_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| semver::Version::parse(name).is_ok())
        && root.join("current").is_symlink()
    {
        return if (probe.writable)(root) {
            InstallKind::Linux {
                root: root.to_path_buf(),
                legacy_binary: None,
            }
        } else {
            manual(format!(
                "Wizard GUI is in {}, which this account can't write to.",
                root.display()
            ))
        };
    }
    // The first install.sh copied the binary straight to ~/.local/bin.
    if let (Some(home), Some(data_home)) = (probe.home, probe.data_home)
        && exe == home.join(".local/bin").join(LINUX_BINARY)
        && !exe.is_symlink()
        && (probe.writable)(&home.join(".local/bin"))
    {
        return InstallKind::Linux {
            root: linux_root(data_home),
            legacy_binary: Some(exe.to_path_buf()),
        };
    }
    if exe.starts_with("/usr") || exe.starts_with("/opt") {
        return manual(
            "Wizard GUI was installed system-wide, probably by a package manager; update it the same way.",
        );
    }
    manual(
        "This copy of Wizard GUI wasn't put there by its installer (a source build?), so it can't replace itself.",
    )
}

fn dir_writable(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".wizard-gui-write-probe")
        .tempfile_in(dir)
        .is_ok()
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

fn bundle_name(bundle: &Path) -> String {
    bundle
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Wizard GUI.app".into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BundleInfo {
    pub id: String,
    pub version: String,
    pub executable: String,
}

pub(crate) fn read_bundle_info(bundle: &Path) -> Result<BundleInfo> {
    let path = bundle.join("Contents/Info.plist");
    let value =
        plist::Value::from_file(&path).with_context(|| format!("reading {}", path.display()))?;
    let dict = value
        .as_dictionary()
        .with_context(|| format!("{} is not a dictionary", path.display()))?;
    let field = |key: &str| {
        dict.get(key)
            .and_then(plist::Value::as_string)
            .map(str::to_string)
            .with_context(|| format!("{} has no {key}", path.display()))
    };
    Ok(BundleInfo {
        id: field("CFBundleIdentifier")?,
        version: field("CFBundleShortVersionString")?,
        executable: field("CFBundleExecutable")?,
    })
}

/// A bundle is acceptable when it is the same app (bundle id), the version
/// the signed checksums named, and its executable is there. On macOS its code
/// signature has to hold too.
pub(crate) fn verify_bundle(bundle: &Path, id: &str, version: &str) -> Result<()> {
    let info = read_bundle_info(bundle)?;
    ensure!(
        info.id == id,
        "the downloaded app is {}, not {id}; nothing was installed",
        info.id
    );
    ensure!(
        info.version == version,
        "the downloaded app says it is version {}, not {version}; nothing was installed",
        info.version
    );
    let exe = bundle.join("Contents/MacOS").join(&info.executable);
    ensure!(
        is_executable(&exe),
        "the downloaded app has no executable at {}",
        exe.display()
    );
    #[cfg(target_os = "macos")]
    run(
        "/usr/bin/codesign",
        &["--verify".as_ref(), "--strict".as_ref(), bundle.as_os_str()],
    )
    .context("checking the downloaded app's code signature")?;
    Ok(())
}

/// Mount the dmg read-only, copy the app out into `dest_dir`, unmount, and
/// check the copy. Returns the staged bundle.
fn stage_mac_dmg(
    dmg: &Path,
    dest_dir: &Path,
    name: &str,
    id: &str,
    version: &str,
) -> Result<PathBuf> {
    let mount = tempfile::Builder::new()
        .prefix("wizard-gui-dmg-")
        .tempdir()
        .context("creating a mount point")?;
    run(
        "/usr/bin/hdiutil",
        &[
            "attach".as_ref(),
            "-nobrowse".as_ref(),
            "-readonly".as_ref(),
            "-noautoopen".as_ref(),
            "-mountpoint".as_ref(),
            mount.path().as_os_str(),
            dmg.as_os_str(),
        ],
    )
    .context("mounting the downloaded disk image")?;
    let copied = (|| {
        let source = std::fs::read_dir(mount.path())?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.extension().is_some_and(|ext| ext == "app"))
            .context("the disk image holds no app")?;
        let staged = dest_dir.join(name);
        let _ = std::fs::remove_dir_all(&staged);
        copy_tree(&source, &staged)?;
        Ok::<_, anyhow::Error>(staged)
    })();
    let _ = run(
        "/usr/bin/hdiutil",
        &[
            "detach".as_ref(),
            "-force".as_ref(),
            mount.path().as_os_str(),
        ],
    );
    let staged = copied?;
    if let Err(err) = verify_bundle(&staged, id, version) {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(err);
    }
    Ok(staged)
}

/// Replace `bundle` with `staged`: copy it beside the installed bundle (same
/// volume, so the swap is a rename), check the copy, swap, and check again.
/// The old bundle is kept as `.<name>.previous` next to it; any failure
/// leaves the old bundle where it was.
pub(crate) fn apply_mac(
    staged: &Path,
    bundle: &Path,
    check: &dyn Fn(&Path) -> Result<()>,
) -> Result<()> {
    let parent = bundle.parent().context("the app has no parent directory")?;
    let name = bundle_name(bundle);
    let fresh = parent.join(format!(".{name}.new-{}", std::process::id()));
    let backup = backup_path(bundle);
    let _ = std::fs::remove_dir_all(&fresh);
    if let Err(err) = copy_tree(staged, &fresh).and_then(|()| check(&fresh)) {
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err).context("preparing the new app; the installed one is unchanged");
    }
    if backup.exists() {
        std::fs::remove_dir_all(&backup)
            .with_context(|| format!("removing the old backup {}", backup.display()))?;
    }
    if let Err(err) = swap_into(&fresh, bundle, &backup) {
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err);
    }
    if let Err(err) = check(bundle) {
        // Put the old bundle back; the new one is dropped.
        let failed = parent.join(format!(".{name}.failed-{}", std::process::id()));
        let restored =
            std::fs::rename(bundle, &failed).and_then(|()| std::fs::rename(&backup, bundle));
        let _ = std::fs::remove_dir_all(&failed);
        restored.context("restoring the previous app after a failed update")?;
        return Err(err).context("the installed update failed its check; the previous app is back");
    }
    Ok(())
}

/// The previous bundle after an update: hidden, and without the `.app`
/// extension so Launch Services doesn't list it as a second copy.
pub fn backup_path(bundle: &Path) -> PathBuf {
    bundle.with_file_name(format!(".{}.previous", bundle_name(bundle)))
}

/// Move `fresh` to `bundle` and the old `bundle` to `backup`. On macOS the
/// first step is one `renamex_np(RENAME_SWAP)`, so there is no moment with no
/// app at `bundle`.
fn swap_into(fresh: &Path, bundle: &Path, backup: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let from = std::ffi::CString::new(fresh.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(bundle.as_os_str().as_bytes())?;
        // SAFETY: both are valid NUL-terminated paths for the call's duration.
        if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } == 0 {
            // `fresh` now holds the old bundle.
            return std::fs::rename(fresh, backup).context("keeping the previous app as a backup");
        }
        tracing::debug!(
            error = %std::io::Error::last_os_error(),
            "atomic bundle swap unavailable; renaming in two steps"
        );
    }
    std::fs::rename(bundle, backup).context("moving the installed app aside")?;
    if let Err(err) = std::fs::rename(fresh, bundle) {
        std::fs::rename(backup, bundle).context("restoring the app after a failed swap")?;
        return Err(err).context("moving the new app into place");
    }
    Ok(())
}

/// Copy a directory tree preserving modes and symlinks: `ditto` on macOS
/// (keeps signatures and extended attributes), `cp -a` elsewhere.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        run("/usr/bin/ditto", &[from.as_os_str(), to.as_os_str()])
    } else {
        run("cp", &["-a".as_ref(), from.as_os_str(), to.as_os_str()])
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

/// A version directory is acceptable when its `wizard-gui` runs and prints
/// the version the signed checksums named.
pub(crate) fn verify_linux_build(dir: &Path, version: &str) -> Result<()> {
    let exe = dir.join(LINUX_BINARY);
    ensure!(
        is_executable(&exe),
        "{} has no executable {LINUX_BINARY}",
        dir.display()
    );
    let reported = run_version(&exe)?;
    ensure!(
        reported.split_whitespace().last() == Some(version),
        "the downloaded wizard-gui reports {reported:?}, not {version}; nothing was installed"
    );
    Ok(())
}

fn run_version(exe: &Path) -> Result<String> {
    let spawn = || {
        std::process::Command::new(exe)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
    };
    // ETXTBSY: a file just written can still be open for writing in a child
    // some other thread forked a moment ago. It clears once that child execs.
    let mut attempt = 0;
    let mut child = loop {
        match spawn() {
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 50 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            result => {
                break result.with_context(|| format!("running {} --version", exe.display()))?;
            }
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait()? {
            let mut out = String::new();
            use std::io::Read as _;
            child.stdout.take().unwrap().read_to_string(&mut out)?;
            ensure!(status.success(), "{} --version failed", exe.display());
            return Ok(out.trim().to_string());
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            bail!("{} --version did not finish", exe.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Unpack the tarball (rooted at `wizard-gui-<ver>-linux-<arch>/`) into
/// `root/<version>`, through a private stage dir so a half-unpacked copy is
/// never at the final path.
fn stage_linux_tarball(tarball: &Path, root: &Path, version: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let dest = root.join(version);
    let stage = tempfile::Builder::new()
        .prefix(&format!(".stage-{version}-"))
        .tempdir_in(root)
        .context("creating a staging directory")?;
    run(
        "tar",
        &[
            "-xzf".as_ref(),
            tarball.as_os_str(),
            "-C".as_ref(),
            stage.path().as_os_str(),
            "--strip-components=1".as_ref(),
        ],
    )
    .context("unpacking the download")?;
    verify_linux_build(stage.path(), version)?;
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("replacing an incomplete {}", dest.display()))?;
    }
    let stage = stage.keep();
    std::fs::rename(&stage, &dest)
        .with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(dest)
}

/// Point `root/current` at `version`, keep the version it pointed at before,
/// and drop any older ones. `legacy_binary` (an old plain-copy install) is
/// first preserved as `root/<running version>` and then replaced by a symlink
/// through `current`. The swap is checked with `check`; a failure puts
/// `current` back.
pub(crate) fn apply_linux(
    root: &Path,
    version: &str,
    legacy_binary: Option<&Path>,
    check: &dyn Fn(&Path) -> Result<()>,
) -> Result<()> {
    let target = root.join(version);
    check(&target)?;
    let current = root.join("current");
    let mut previous = std::fs::read_link(&current).ok();
    if let Some(binary) = legacy_binary {
        let running = root.join(crate::current_version());
        if running != target && !running.join(LINUX_BINARY).exists() {
            std::fs::create_dir_all(&running)?;
            std::fs::copy(binary, running.join(LINUX_BINARY))
                .context("keeping the running version as the previous one")?;
        }
        previous = previous.or_else(|| Some(PathBuf::from(crate::current_version())));
    }
    point_symlink(&current, Path::new(version))?;
    let result = check(&current).and_then(|()| match legacy_binary {
        Some(binary) => point_symlink(binary, &current.join(LINUX_BINARY)),
        None => Ok(()),
    });
    if let Err(err) = result {
        match &previous {
            Some(previous) => point_symlink(&current, previous)?,
            None => std::fs::remove_file(&current)?,
        }
        return Err(err).context("the update failed its check; the previous version is back");
    }
    prune_versions(root, version, previous.as_deref());
    Ok(())
}

/// Atomically make `link` a symlink to `target`: a temp symlink renamed over it.
fn point_symlink(link: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = link.parent().context("symlink has no parent")?;
        let tmp = parent.join(format!(
            ".{}.new-{}",
            link.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(target, &tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        std::fs::rename(&tmp, link).with_context(|| format!("replacing {}", link.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (link, target);
        bail!("symlink installs are Unix-only")
    }
}

/// Remove version directories other than the new one and the previous one.
fn prune_versions(root: &Path, keep: &str, previous: Option<&Path>) {
    let previous = previous
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned());
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == keep || Some(&name) == previous.as_ref() {
            continue;
        }
        if semver::Version::parse(&name).is_ok() && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Start `command` once this process has exited, detached from it. Starting
/// earlier would race the engine's single-instance lock and IPC port.
fn relaunch_after_exit(command: &[&std::ffi::OsStr]) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let script = format!(
            "while /bin/kill -0 {} 2>/dev/null; do sleep 0.2; done; exec \"$@\"",
            std::process::id()
        );
        let mut child = std::process::Command::new("/bin/sh");
        child
            .arg("-c")
            .arg(script)
            .arg("wizard-gui-relaunch")
            .args(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        if let Err(err) = child.spawn() {
            tracing::error!(error = %err, "could not start the relaunch helper");
        }
    }
    #[cfg(not(unix))]
    let _ = command;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn always(_: &Path) -> bool {
        true
    }
    fn never(_: &Path) -> bool {
        false
    }

    fn probe<'a>(os: &'a str, writable: &'a dyn Fn(&Path) -> bool) -> Probe<'a> {
        Probe {
            os,
            home: Some(Path::new("/home/u")),
            data_home: Some(Path::new("/home/u/.local/share")),
            appimage: false,
            writable,
        }
    }

    #[test]
    fn mac_bundles_update_only_where_they_can_be_written() {
        let exe = Path::new("/Applications/Wizard GUI.app/Contents/MacOS/wizard-gui");
        assert_eq!(
            detect(exe, &probe("macos", &always)),
            InstallKind::MacApp {
                bundle: "/Applications/Wizard GUI.app".into()
            }
        );
        let kind = detect(exe, &probe("macos", &never));
        assert!(
            kind.manual_reason().unwrap().contains("/Applications"),
            "{kind:?}"
        );
        let dmg = detect(
            Path::new("/Volumes/Wizard GUI/Wizard GUI.app/Contents/MacOS/wizard-gui"),
            &probe("macos", &never),
        );
        assert!(dmg.manual_reason().unwrap().contains("disk image"));
        let translocated = detect(
            Path::new(
                "/private/var/folders/x/AppTranslocation/ABC/d/Wizard GUI.app/Contents/MacOS/wizard-gui",
            ),
            &probe("macos", &always),
        );
        assert!(
            translocated
                .manual_reason()
                .unwrap()
                .contains("read-only copy")
        );
        assert!(
            !detect(
                Path::new("/tmp/foo.app/wizard-gui"),
                &probe("macos", &always)
            )
            .can_self_update()
        );
    }

    #[test]
    fn a_copy_unpacked_by_wizard_is_left_to_wizard_update() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(LINUX_BINARY);
        std::fs::write(&exe, "").unwrap();
        std::fs::write(dir.path().join(WIZARD_STAMP), "aaaa\n").unwrap();
        for os in ["linux", "macos"] {
            let InstallKind::Manual { reason } = detect(&exe, &probe(os, &always)) else {
                panic!("{os}: an unpacked copy must not update itself");
            };
            assert!(reason.contains("wizard update"), "{reason}");
        }
    }

    #[test]
    fn linux_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("wizard-gui");
        std::fs::create_dir_all(root.join("3.6.1")).unwrap();
        std::os::unix::fs::symlink("3.6.1", root.join("current")).unwrap();
        let exe = root.join("3.6.1/wizard-gui");
        assert_eq!(
            detect(&exe, &probe("linux", &always)),
            InstallKind::Linux {
                root: root.clone(),
                legacy_binary: None
            }
        );
        assert!(!detect(&exe, &probe("linux", &never)).can_self_update());
        // Without `current` it is just some directory.
        std::fs::remove_file(root.join("current")).unwrap();
        assert!(!detect(&exe, &probe("linux", &always)).can_self_update());

        assert_eq!(
            detect(
                Path::new("/home/u/.local/bin/wizard-gui"),
                &probe("linux", &always)
            ),
            InstallKind::Linux {
                root: "/home/u/.local/share/wizard-gui".into(),
                legacy_binary: Some("/home/u/.local/bin/wizard-gui".into())
            }
        );
        for (exe, words) in [
            ("/usr/bin/wizard-gui", "package manager"),
            ("/nix/store/abc-wizard-gui/bin/wizard-gui", "Nix"),
            ("/src/gui/target/release/zeron", "source build"),
        ] {
            let reason = detect(Path::new(exe), &probe("linux", &always))
                .manual_reason()
                .unwrap()
                .to_string();
            assert!(reason.contains(words), "{exe}: {reason}");
        }
        let appimage = Probe {
            appimage: true,
            ..probe("linux", &always)
        };
        assert!(
            detect(Path::new("/tmp/.mount_x/wizard-gui"), &appimage)
                .manual_reason()
                .unwrap()
                .contains("AppImage")
        );
    }

    /// A fake `wizard-gui` that prints `zeron <version>` like the real one.
    fn fake_build(dir: &Path, version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let exe = dir.join(LINUX_BINARY);
        std::fs::write(&exe, format!("#!/bin/sh\necho 'zeron {version}'\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn current_of(root: &Path) -> String {
        std::fs::read_link(root.join("current"))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn check_version(version: &str) -> impl Fn(&Path) -> Result<()> + '_ {
        move |dir| verify_linux_build(dir, version)
    }

    #[test]
    fn linux_swap_keeps_the_previous_version_and_prunes_older_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for version in ["3.5.0", "3.6.0", "3.6.1"] {
            fake_build(&root.join(version), version);
        }
        std::os::unix::fs::symlink("3.6.0", root.join("current")).unwrap();

        apply_linux(root, "3.6.1", None, &check_version("3.6.1")).unwrap();
        assert_eq!(current_of(root), "3.6.1");
        assert!(root.join("3.6.0").exists(), "previous version kept");
        assert!(!root.join("3.5.0").exists(), "older version pruned");
        assert_eq!(
            run_version(&root.join("current/wizard-gui")).unwrap(),
            "zeron 3.6.1"
        );
    }

    #[test]
    fn linux_rolls_back_when_the_new_version_fails_its_check() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fake_build(&root.join("3.6.0"), "3.6.0");
        // The directory says 3.6.1 but the binary inside is something else.
        fake_build(&root.join("3.6.1"), "3.5.9");
        std::os::unix::fs::symlink("3.6.0", root.join("current")).unwrap();
        let err = apply_linux(root, "3.6.1", None, &check_version("3.6.1")).unwrap_err();
        assert!(format!("{err:#}").contains("reports"), "{err:#}");
        assert_eq!(current_of(root), "3.6.0");

        // A check that passes before the swap and fails after it (the link
        // resolving somewhere unexpected) restores `current` too.
        fake_build(&root.join("3.6.1"), "3.6.1");
        let calls = std::cell::Cell::new(0);
        let flaky = |dir: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                verify_linux_build(dir, "3.6.1")
            } else {
                bail!("injected failure")
            }
        };
        let err = apply_linux(root, "3.6.1", None, &flaky).unwrap_err();
        assert!(format!("{err:#}").contains("previous version is back"));
        assert_eq!(current_of(root), "3.6.0");
    }

    #[test]
    fn a_legacy_linux_install_moves_to_the_versioned_layout() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fake_build(&bin, crate::current_version());
        let legacy = bin.join(LINUX_BINARY);
        let root = dir.path().join("share/wizard-gui");
        fake_build(&root.join("99.0.0"), "99.0.0");

        apply_linux(&root, "99.0.0", Some(&legacy), &check_version("99.0.0")).unwrap();
        assert_eq!(current_of(&root), "99.0.0");
        assert_eq!(
            std::fs::read_link(&legacy).unwrap(),
            root.join("current/wizard-gui")
        );
        assert_eq!(run_version(&legacy).unwrap(), "zeron 99.0.0");
        // The running version is kept as the previous one.
        assert_eq!(
            run_version(&root.join(crate::current_version()).join(LINUX_BINARY)).unwrap(),
            format!("zeron {}", crate::current_version())
        );
    }

    #[test]
    fn linux_tarballs_unpack_beside_the_running_version() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("wizard-gui-9.1.0-linux-x86_64");
        fake_build(&src, "9.1.0");
        std::fs::write(src.join("install.sh"), "").unwrap();
        let tarball = dir.path().join("x.tar.gz");
        run(
            "tar",
            &[
                "-czf".as_ref(),
                tarball.as_os_str(),
                "-C".as_ref(),
                dir.path().as_os_str(),
                "wizard-gui-9.1.0-linux-x86_64".as_ref(),
            ],
        )
        .unwrap();
        let root = dir.path().join("root");
        let staged = stage_linux_tarball(&tarball, &root, "9.1.0").unwrap();
        assert_eq!(staged, root.join("9.1.0"));
        assert!(staged.join("install.sh").exists());
        // A tarball whose binary claims another version is refused and leaves
        // nothing behind.
        let err = stage_linux_tarball(&tarball, &root, "9.2.0").unwrap_err();
        assert!(err.to_string().contains("reports"), "{err}");
        assert!(!root.join("9.2.0").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["9.1.0"]);
    }

    /// A fake `.app`: Info.plist with id and version, and an executable.
    fn fake_bundle(path: &Path, id: &str, version: &str) {
        let macos = path.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleExecutable</key><string>wizard-gui</string>
</dict></plist>"#
        );
        std::fs::write(path.join("Contents/Info.plist"), plist).unwrap();
        std::fs::write(macos.join("wizard-gui"), version).unwrap();
        std::fs::set_permissions(
            macos.join("wizard-gui"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    fn bundle_version(bundle: &Path) -> String {
        read_bundle_info(bundle).unwrap().version
    }

    /// `apply_mac` runs `codesign` on macOS, which a hand-made fake bundle
    /// fails, so these checks stop at the plist there.
    fn plist_check<'a>(id: &'a str, version: &'a str) -> impl Fn(&Path) -> Result<()> + 'a {
        move |path| {
            let info = read_bundle_info(path)?;
            ensure!(info.id == id && info.version == version, "wrong bundle");
            Ok(())
        }
    }

    #[test]
    fn mac_swap_installs_the_new_bundle_and_keeps_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("Applications/Wizard GUI.app");
        let staged = dir.path().join("updates/9.1.0/Wizard GUI.app");
        fake_bundle(&installed, "sh.zeron.app", "9.0.0");
        fake_bundle(&staged, "sh.zeron.app", "9.1.0");

        apply_mac(&staged, &installed, &plist_check("sh.zeron.app", "9.1.0")).unwrap();
        assert_eq!(bundle_version(&installed), "9.1.0");
        assert_eq!(bundle_version(&backup_path(&installed)), "9.0.0");
        let names: Vec<_> = std::fs::read_dir(installed.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");

        // The next update replaces the backup rather than piling them up.
        fake_bundle(&staged, "sh.zeron.app", "9.2.0");
        apply_mac(&staged, &installed, &plist_check("sh.zeron.app", "9.2.0")).unwrap();
        assert_eq!(bundle_version(&backup_path(&installed)), "9.1.0");
    }

    #[test]
    fn a_mac_bundle_that_fails_verification_is_never_swapped_in() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("Wizard GUI.app");
        let staged = dir.path().join("staged/Wizard GUI.app");
        fake_bundle(&installed, "sh.zeron.app", "9.0.0");

        for (id, version) in [("com.example.other", "9.1.0"), ("sh.zeron.app", "9.0.5")] {
            fake_bundle(&staged, id, version);
            if !cfg!(target_os = "macos") {
                let err = verify_bundle(&staged, "sh.zeron.app", "9.1.0").unwrap_err();
                assert!(err.to_string().contains("nothing was installed"), "{err}");
            }
            assert!(apply_mac(&staged, &installed, &plist_check("sh.zeron.app", "9.1.0")).is_err());
            assert_eq!(bundle_version(&installed), "9.0.0");
            assert!(!backup_path(&installed).exists());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        }
    }

    #[test]
    fn a_mac_swap_that_fails_its_final_check_restores_the_old_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("Wizard GUI.app");
        let staged = dir.path().join("staged/Wizard GUI.app");
        fake_bundle(&installed, "sh.zeron.app", "9.0.0");
        fake_bundle(&staged, "sh.zeron.app", "9.1.0");
        let calls = std::cell::Cell::new(0);
        let flaky = |_: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(())
            } else {
                bail!("injected failure")
            }
        };
        let err = apply_mac(&staged, &installed, &flaky).unwrap_err();
        assert!(
            format!("{err:#}").contains("previous app is back"),
            "{err:#}"
        );
        assert_eq!(bundle_version(&installed), "9.0.0");
        assert!(!backup_path(&installed).exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn bundle_info_reads_the_packaged_plist() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("Wizard GUI.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        let template =
            include_str!("../../../dist/macos/Info.plist").replace("__VERSION__", "3.6.1");
        std::fs::write(bundle.join("Contents/Info.plist"), template).unwrap();
        assert_eq!(
            read_bundle_info(&bundle).unwrap(),
            BundleInfo {
                id: "sh.zeron.app".into(),
                version: "3.6.1".into(),
                executable: "wizard-gui".into()
            }
        );
    }
}
