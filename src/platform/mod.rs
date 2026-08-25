//! The one place Wizard is allowed to know what operating system it is on.
//!
//! Every OS assumption used to be scattered across the tree. Counted with
//! `git grep -l <pattern> <the commit this module landed on> -- 'src/*'`, so
//! the numbers reproduce: `std::os::unix` imports in seventeen files, POSIX
//! mode bits (`PermissionsExt`, `OpenOptionsExt`, `DirBuilderExt`,
//! `from_mode`, `.mode(0o…)`) in thirteen, a hand-written `Command::new("sh")`
//! in five, `libc::kill` in three, and `std::env::temp_dir()` in thirty-six
//! (mostly test scaffolding, but the update staging area and the
//! clipboard-image spool were in there too). Each one is a separate decision
//! to re-make on a second platform, and a decision made seventeen times is a
//! decision made seventeen ways.
//!
//! This module owns those decisions, one submodule per concern:
//!
//! - [`secrets`]: private files and directories (POSIX 0600/0700 today;
//!   an ACL or DPAPI on Windows). `credentials.toml` at 0600 *is* Wizard's
//!   entire secret-storage model, so this is the load-bearing one.
//! - [`process`]: own-process-group spawn and whole-group kill (`setpgid` +
//!   `kill(-pgid)` today; a Job Object on Windows), plus becoming another
//!   binary outright (`exec`, which Windows has no equivalent of at all).
//! - [`lockfile`]: cross-process exclusive locks (`flock` today; `LockFileEx`
//!   on Windows). Two wizards sharing one `~/.wizard` both rewrite the trust
//!   store whole, and without a lock the second rename drops the first one's
//!   decision.
//! - [`shell`]: running a command line through the platform shell (`sh -c`
//!   today; `cmd /c` or PowerShell on Windows), and naming that shell for the
//!   system prompt.
//! - [`paths`]: where state, config, cache, logs, scratch and user-local
//!   binaries live (one `~/.wizard` tree today; `%APPDATA%`/`%LOCALAPPDATA%`
//!   would split it), and symbolic links, which are a path that names another
//!   path and which Windows splits in two and gates behind a privilege.
//! - [`exe_swap`]: replacing a possibly-running executable atomically
//!   (rename-over today; Windows cannot do that and needs the running image
//!   renamed aside first).
//! - [`service`]: supervising a long-running surface in the background (a
//!   systemd **user** unit on Linux, a launchd LaunchAgent on macOS; a refusal
//!   naming `termux-services` or the host's own supervisor everywhere else).
//!   The gateway, the scheduler and eventually the mesh listener all want the
//!   same thing, and a unit file pasted into a doc for the reader to edit is
//!   how they each got it slightly wrong.
//!
//! Host *detection* (Termux, NixOS) predates all of this and lives in
//! [`host`], re-exported here so `crate::platform::is_termux()` keeps working.
//!
//! ## The rule
//!
//! **`#[cfg]` lives inside these functions, never at a call site.** Each
//! function has one cross-platform signature and a `#[cfg(unix)]` body, with
//! the `#[cfg(not(unix))]` arm sitting right next to it as the seam a Windows
//! port fills in. A caller that has to write `#[cfg(unix)]` to use this module
//! is an extraction that did not happen: it just moved the conditional
//! compilation somewhere with a worse view of the problem.
//!
//! Everything here is Unix-only in behaviour today, by design. The port is a
//! separate change; an extraction reviewed at the same time as a port is an
//! extraction nobody can review.
//!
//! ## What has not moved yet
//!
//! The rule above is the goal, not yet a description of the tree. Fourteen
//! files still reach for `std::os::unix` directly: `git_util`, `server`,
//! `local_setup`, `schedule`, `instructions`, `tools/lua`, `gui/ws`,
//! `gui/server`, `gateway/telegram`, `mesh/node`, `llm/xai_oauth`,
//! `plugins/chatgpt/oauth`, `agent/mod` and `app/mod`. Every one of them has a
//! home here now, and the conversions are mechanical:
//!
//! - `std::os::unix::fs::symlink` becomes [`paths::symlink`].
//! - `set_permissions(.., 0o600 | 0o700)` becomes [`secrets::harden_file`],
//!   [`secrets::create_private_dir`] or [`secrets::write_private_atomic`];
//!   `0o755` becomes [`exe_swap::set_executable`]; a test loosening a path
//!   becomes `secrets::expose_to_other_users` (test-only, so rustdoc cannot
//!   link it), and one reading a mode back becomes [`secrets::is_protected`]
//!   or [`secrets::is_private_file`].
//! - `std::os::unix::process::CommandExt::exec` becomes
//!   [`process::exec_replace`].
//! - `libc::flock` becomes [`lockfile::exclusive`]. (`schedule` also records
//!   the holding pid inside the lock file, which needs an accessor on
//!   [`lockfile::Guard`] that has no caller yet and so has not been written.)
//!
//! One gap has no home here and wants one before its callers move: `git_util`
//! uses `std::os::unix::ffi::OsStrExt` to read git's byte-oriented output as a
//! path, which Windows spells `OsStrExt::encode_wide` over UTF-16 and is a
//! genuinely different conversion.

pub mod exe_swap;
pub mod host;
pub mod lockfile;
pub mod paths;
pub mod process;
pub mod secrets;
pub mod service;
pub mod shell;

pub use host::{is_nixos, is_termux, termux_prebuilt_hint};
