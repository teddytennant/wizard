//! `wizard gui`: open Wizard GUI, the desktop app built from `gui/`.
//!
//! Release builds carry the app inside this binary (`build.rs`, `WIZARD_GUI`).
//! The first `wizard gui` after an install or update unpacks it to
//! `~/.wizard/gui/` and every run starts it from there, as a child of the
//! terminal. That is the point of shipping it this way: there is no dmg to
//! mount, no Gatekeeper prompt for a downloaded bundle, and on macOS the app
//! runs under the terminal's own permissions the way any program started from
//! a shell does, instead of asking for Screen Recording and Accessibility as a
//! separate app. A build without it (`cargo install`, Nix, musl) falls back to
//! an installed copy: the dmg, the Linux tarball or the Windows zip.
//!
//! It does not wait for the app: the terminal comes back the way it does after
//! `open` or `code .`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::platform::host::is_executable;

/// Where the installers are.
pub const RELEASES: &str = "https://github.com/teddytennant/wizard/releases/latest";

/// How to start the app on this machine.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// A macOS bundle, opened with `open`.
    Bundle(PathBuf),
    /// The app's binary, run with no arguments, which opens the window.
    Binary(PathBuf),
}

/// The app's bundle names on macOS, current first. Packages before the rename
/// shipped upstream Zeron's `Zeron.app`.
const BUNDLES: [&str; 2] = ["Wizard GUI.app", "Zeron.app"];

/// The app's binary names, current first. The Linux tarball's `install.sh`
/// puts `wizard-gui` in `~/.local/bin`; older ones installed `zeron`.
fn binary_names(os: &str) -> [&'static str; 2] {
    if os == "windows" {
        ["wizard-gui.exe", "zeron.exe"]
    } else {
        ["wizard-gui", "zeron"]
    }
}

/// Find the installed app: on macOS a bundle in `/Applications` or
/// `~/Applications` first, then the binary on `path`, then in `~/.local/bin`,
/// which is where the Linux installer puts it and which is not always on
/// `PATH`. The current name wins over the legacy one wherever both exist.
pub fn find(os: &str, path: Option<&OsStr>, home: Option<&Path>) -> Option<Launch> {
    if os == "macos" {
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(home) = home {
            roots.push(home.join("Applications"));
        }
        let bundle = BUNDLES
            .iter()
            .flat_map(|name| roots.iter().map(move |root| root.join(name)))
            .find(|b| b.is_dir());
        if let Some(bundle) = bundle {
            return Some(Launch::Bundle(bundle));
        }
    }
    let mut dirs: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
    }
    binary_names(os)
        .iter()
        .flat_map(|name| dirs.iter().map(move |dir| dir.join(name)))
        .find(|candidate| is_executable(candidate))
        .map(Launch::Binary)
}

/// The app this binary carries, gzipped, and an id that changes with its
/// bytes. `None` when it was built without `WIZARD_GUI`.
fn embedded() -> Option<(&'static [u8], &'static str)> {
    #[cfg(wizard_gui)]
    {
        Some((
            include_bytes!(concat!(env!("OUT_DIR"), "/wizard-gui.gz")),
            env!("WIZARD_GUI_ID"),
        ))
    }
    #[cfg(not(wizard_gui))]
    {
        None
    }
}

/// Name of the file beside the unpacked binary that records which embedded
/// build it is. The GUI's updater looks for it too: a copy that came out of
/// wizard is updated by `wizard update`, never by itself.
pub const STAMP: &str = "wizard-gui.embedded";

/// Unpack `gz` into `dir` unless the copy there is already build `id`, and
/// return the binary's path. Written beside the final name and renamed, so a
/// second `wizard gui` racing this one, or a GUI still running from the old
/// copy, never sees a half-written file.
fn unpack(dir: &Path, gz: &[u8], id: &str) -> Result<PathBuf> {
    let bin = dir.join(binary_names(std::env::consts::OS)[0]);
    let stamp = dir.join(STAMP);
    if is_executable(&bin) && std::fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == id) {
        return Ok(bin);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let pid = std::process::id();
    let tmp = dir.join(format!(".wizard-gui.{pid}"));
    let result = (|| -> Result<()> {
        let mut out = std::fs::File::create(&tmp)?;
        std::io::copy(&mut flate2::read::GzDecoder::new(gz), &mut out)?;
        out.sync_all()?;
        drop(out);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &bin)?;
        let stamp_tmp = dir.join(format!(".{STAMP}.{pid}"));
        std::fs::write(&stamp_tmp, format!("{id}\n"))?;
        std::fs::rename(&stamp_tmp, &stamp)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("unpacking Wizard GUI into {}", dir.display()));
    }
    Ok(bin)
}

/// Where the embedded app is unpacked: `~/.wizard/gui`.
fn unpack_dir() -> Result<PathBuf> {
    Ok(crate::config::Config::wizard_dir()?.join("gui"))
}

/// How to start the app here: the copy this binary carries, unpacked, or else
/// an installed one. `None` when there is neither.
pub fn locate() -> Result<Option<Launch>> {
    if let Some((gz, id)) = embedded() {
        return unpack(&unpack_dir()?, gz, id).map(|bin| Some(Launch::Binary(bin)));
    }
    let home = dirs::home_dir();
    let path = std::env::var_os("PATH");
    Ok(find(std::env::consts::OS, path.as_deref(), home.as_deref()))
}

/// True when `wizard gui` has something to start, without unpacking it.
pub fn available() -> bool {
    embedded().is_some() || {
        let home = dirs::home_dir();
        let path = std::env::var_os("PATH");
        find(std::env::consts::OS, path.as_deref(), home.as_deref()).is_some()
    }
}

fn not_installed() -> anyhow::Error {
    anyhow::anyhow!(
        "this wizard was built without Wizard GUI and no installed copy was found; \
         install wizard from a release (`wizard update` or install.sh), or get the app from {RELEASES}"
    )
}

/// `wizard gui --path`: print where the app is, unpacking it first if it
/// comes with this binary.
pub fn print_path() -> Result<()> {
    match locate()?.ok_or_else(not_installed)? {
        Launch::Bundle(path) | Launch::Binary(path) => println!("{}", path.display()),
    }
    Ok(())
}

/// Start the app in `cwd` and return without waiting for it.
pub fn open(cwd: Option<&Path>) -> Result<()> {
    let launch = locate()?.ok_or_else(not_installed)?;
    let mut cmd = match &launch {
        Launch::Bundle(bundle) => {
            let mut cmd = Command::new("open");
            cmd.arg(bundle);
            cmd
        }
        Launch::Binary(binary) => {
            let mut cmd = Command::new(binary);
            // The app drives Wizard over ACP; have it drive this one rather
            // than whichever `wizard` it finds on PATH.
            if std::env::var_os("WIZARD_EXECUTABLE").is_none()
                && let Ok(exe) = std::env::current_exe()
            {
                cmd.env("WIZARD_EXECUTABLE", exe);
            }
            // Its own process group, so closing the terminal that ran
            // `wizard gui` does not take the window with it.
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            cmd
        }
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting Wizard GUI ({launch:?})"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// True when the machine running the tests has a system-wide install,
    /// which `find` rightly prefers over anything under a temporary home.
    fn system_bundle_installed() -> bool {
        BUNDLES
            .iter()
            .any(|name| Path::new("/Applications").join(name).is_dir())
    }

    #[test]
    fn the_binary_on_path_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin/wizard-gui");
        executable(&bin);
        let path =
            std::env::join_paths([dir.path().join("empty"), dir.path().join("bin")]).unwrap();
        assert_eq!(find("linux", Some(&path), None), Some(Launch::Binary(bin)));
    }

    /// The Linux installer writes to `~/.local/bin`, which a fresh login
    /// shell does not always have on `PATH`.
    #[test]
    fn the_linux_installers_location_is_found_off_path() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin/wizard-gui");
        executable(&bin);
        assert_eq!(
            find("linux", Some(OsStr::new("")), Some(home.path())),
            Some(Launch::Binary(bin))
        );
    }

    /// Packages from before the rename installed `zeron`.
    #[test]
    fn a_legacy_zeron_binary_is_still_found() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin/zeron");
        executable(&bin);
        assert_eq!(
            find("linux", Some(OsStr::new("")), Some(home.path())),
            Some(Launch::Binary(bin))
        );
    }

    /// With both installed, `wizard-gui` wins even when the legacy binary
    /// sits earlier on `PATH`.
    #[test]
    fn wizard_gui_is_preferred_over_zeron() {
        let dir = tempfile::tempdir().unwrap();
        executable(&dir.path().join("first/zeron"));
        let current = dir.path().join("second/wizard-gui");
        executable(&current);
        let path =
            std::env::join_paths([dir.path().join("first"), dir.path().join("second")]).unwrap();
        assert_eq!(
            find("linux", Some(&path), None),
            Some(Launch::Binary(current))
        );
    }

    #[test]
    fn the_windows_binary_carries_its_extension() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("wizard-gui.exe");
        executable(&bin);
        assert_eq!(
            find("windows", Some(dir.path().as_os_str()), None),
            Some(Launch::Binary(bin))
        );
    }

    #[test]
    fn a_bundle_in_the_users_applications_is_opened_as_a_bundle() {
        if system_bundle_installed() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let bundle = home.path().join("Applications/Wizard GUI.app");
        std::fs::create_dir_all(&bundle).unwrap();
        assert_eq!(
            find("macos", None, Some(home.path())),
            Some(Launch::Bundle(bundle))
        );
    }

    #[test]
    fn the_new_bundle_is_preferred_over_a_legacy_zeron_app() {
        if system_bundle_installed() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("Applications/Zeron.app")).unwrap();
        let bundle = home.path().join("Applications/Wizard GUI.app");
        std::fs::create_dir_all(&bundle).unwrap();
        assert_eq!(
            find("macos", None, Some(home.path())),
            Some(Launch::Bundle(bundle))
        );
    }

    #[test]
    fn a_legacy_zeron_app_is_still_opened() {
        if system_bundle_installed() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let bundle = home.path().join("Applications/Zeron.app");
        std::fs::create_dir_all(&bundle).unwrap();
        assert_eq!(
            find("macos", None, Some(home.path())),
            Some(Launch::Bundle(bundle))
        );
    }

    /// A bundle on a Linux box means nothing; only the binary counts there.
    #[test]
    fn bundles_are_ignored_off_macos() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("Applications/Wizard GUI.app")).unwrap();
        assert_eq!(find("linux", Some(OsStr::new("")), Some(home.path())), None);
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(bytes).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn unpack_writes_an_executable_and_its_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let bin = unpack(dir.path(), &gz(b"#!/bin/sh\n"), "aaaa").unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), b"#!/bin/sh\n");
        assert!(is_executable(&bin));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(STAMP)).unwrap(),
            "aaaa\n"
        );
    }

    /// The same build is not unpacked twice; a different one replaces it.
    #[test]
    fn unpack_only_rewrites_for_a_different_build() {
        let dir = tempfile::tempdir().unwrap();
        let bin = unpack(dir.path(), &gz(b"one"), "aaaa").unwrap();
        // Garbage that would fail to decompress: proves the second call
        // never reads it.
        assert_eq!(unpack(dir.path(), b"not gzip", "aaaa").unwrap(), bin);
        assert_eq!(std::fs::read(&bin).unwrap(), b"one");
        unpack(dir.path(), &gz(b"two"), "bbbb").unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), b"two");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(STAMP)).unwrap(),
            "bbbb\n"
        );
    }

    /// A failed unpack leaves no temporary file and no stamp claiming success.
    #[test]
    fn a_corrupt_payload_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        assert!(unpack(dir.path(), b"not gzip", "aaaa").is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn nothing_installed_is_none() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(find("linux", Some(OsStr::new("")), Some(home.path())), None);
    }

    /// A file that is not executable is not the app.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin/wizard-gui");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "").unwrap();
        assert_eq!(find("linux", Some(OsStr::new("")), Some(home.path())), None);
    }
}
