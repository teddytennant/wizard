//! Host-environment detection shared by install-adjacent code paths.
//!
//! Mirrors the early probes in `install.sh` (`is_nixos`, `is_termux`) so the
//! Rust side — self-update, on-demand llama.cpp install, doctor — makes the
//! same choices the installer already made. Pure filesystem/env checks; no I/O
//! beyond reading a few well-known paths and env vars.
//!
//! [`on_path`] and [`local_port`] joined them when the llama.cpp runtime became
//! a plugin. Both were in `src/server.rs`, and neither was ever about
//! llama-server: what they answer is "does this host have that program" and "is
//! that address this host", which is what every other function in this file
//! answers too. Leaving them behind would have meant a build without the local
//! backend losing onboarding's "is Ollama installed" check and the Ollama
//! plugin's "is this my machine to pull a model onto" check, neither of which
//! has anything to do with which local runtime is compiled in.

use std::path::Path;

/// True when running under [Termux](https://termux.dev) on Android.
///
/// Termux is a Linux userspace rooted at `$PREFIX` (typically
/// `/data/data/com.termux/files/usr`). Stock glibc/musl release binaries do not
/// run there (Bionic libc, no FHS dynamic loader), there is no `sudo`, and the
/// only writable install location on `PATH` is `$PREFIX/bin`. Detected the same
/// way `install.sh` does so installer and runtime stay in lockstep.
pub fn is_termux() -> bool {
    if std::env::var_os("TERMUX_VERSION").is_some() || std::env::var_os("TERMUX_APP_PID").is_some()
    {
        return true;
    }
    // `PREFIX` is always set inside a Termux session; require the Termux app
    // data path so a coincidental `PREFIX` on a desktop host does not trip this.
    if let Ok(prefix) = std::env::var("PREFIX")
        && prefix.contains("com.termux")
    {
        return true;
    }
    Path::new("/data/data/com.termux/files/usr").is_dir()
}

/// True on NixOS. Detected the same way `install.sh` / [`crate::update`] do so
/// the musl-vs-gnu asset preference stays consistent.
pub fn is_nixos() -> bool {
    Path::new("/etc/NIXOS").exists()
        || Path::new("/run/current-system").exists()
        || std::fs::read_to_string("/etc/os-release")
            .map(|text| {
                text.lines().any(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower == "id=nixos" || lower.starts_with("id=nixos")
                })
            })
            .unwrap_or(false)
}

/// Short, user-facing note for surfaces that need to explain why a prebuilt
/// Linux asset is unavailable on Termux. Empty when not on Termux so callers
/// can append unconditionally.
pub fn termux_prebuilt_hint() -> Option<&'static str> {
    if is_termux() {
        Some(
            "Termux has no matching prebuilt release asset (Android/Bionic). \
             Install with a source build: \
             curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
             | WIZARD_BUILD_FROM_SOURCE=1 bash \
             (lands in $PREFIX/bin). Update the same way, or rebuild from ~/.wizard/src.",
        )
    } else {
        None
    }
}

/// True when `name` resolves to an executable on `PATH`.
///
/// Asked about `ollama` by onboarding, about `vulkaninfo` by llama.cpp's
/// installer when it is deciding whether a Vulkan build would run here, and
/// about `llama-server` itself by the spawner. A host question, so it is here
/// rather than with any one of the three.
pub fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .any(|candidate| is_executable(&candidate))
    })
}

/// True when `path` is a file this user can execute.
#[cfg(unix)]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// True when `path` is a file. Windows has no execute bit to consult.
#[cfg(not(unix))]
pub fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The port of `base_url` when that URL names *this* machine, [`None`] when it
/// names somebody else's.
///
/// Two callers ask the same question for two reasons and both of them are "is
/// this mine to touch". llama.cpp passes the answer to `llama-server --port`,
/// because Wizard never spawns a process on behalf of a remote host; Ollama
/// only wants the [`Option`] itself, because Wizard never downloads a
/// multi-gigabyte model onto somebody else's disk. One list of loopback
/// spellings rather than two, since the day the two disagree is the day one of
/// those promises quietly stops being kept.
pub fn local_port(base_url: &str) -> Option<u16> {
    let url = reqwest::Url::parse(base_url).ok()?;
    let local = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1" | "0.0.0.0")
    );
    local.then(|| url.port_or_known_default())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn termux_prebuilt_hint_is_none_off_termux() {
        // This host is a normal Linux/macOS CI or dev box, not Termux.
        // is_termux() may still be true if someone runs the suite inside
        // Termux; the hint must track the detector either way.
        if is_termux() {
            assert!(termux_prebuilt_hint().is_some());
            assert!(
                termux_prebuilt_hint()
                    .unwrap()
                    .contains("WIZARD_BUILD_FROM_SOURCE")
            );
        } else {
            assert!(termux_prebuilt_hint().is_none());
        }
    }

    #[test]
    fn is_termux_false_without_termux_markers() {
        // Guard the negative path: without TERMUX_* and without a Termux
        // PREFIX, a desktop/CI host must not be classified as Termux. If the
        // suite itself is running inside Termux this assertion is skipped —
        // the detector is doing the right thing there.
        if std::env::var_os("TERMUX_VERSION").is_some()
            || std::env::var_os("TERMUX_APP_PID").is_some()
            || std::env::var("PREFIX")
                .map(|p| p.contains("com.termux"))
                .unwrap_or(false)
            || Path::new("/data/data/com.termux/files/usr").is_dir()
        {
            assert!(is_termux());
            return;
        }
        assert!(!is_termux());
    }

    /// The loopback list is what keeps two promises: no process spawned on
    /// somebody else's machine, and no multi-gigabyte download onto somebody
    /// else's disk. It travelled here from `src/server.rs` with the function.
    #[test]
    fn local_port_accepts_loopback_hosts_only() {
        assert_eq!(local_port("http://127.0.0.1:8080"), Some(8080));
        assert_eq!(local_port("http://localhost:9000/"), Some(9000));
        assert_eq!(local_port("http://[::1]:8081"), Some(8081));
        assert_eq!(local_port("http://localhost"), Some(80), "known default");
        assert_eq!(local_port("http://10.0.0.5:8080"), None, "remote host");
        assert_eq!(local_port("http://example.com:8080"), None);
        assert_eq!(local_port("not a url"), None);
    }

    /// `on_path` finds something every host running this suite has, and does
    /// not find a name nothing could plausibly be called.
    #[test]
    fn on_path_answers_for_a_program_every_host_has_and_for_one_no_host_does() {
        assert!(on_path("sh") || on_path("cmd.exe"), "no shell on PATH?");
        assert!(!on_path("wizard-no-such-program-9f3c1a"));
    }
}
