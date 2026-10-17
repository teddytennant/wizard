//! The `claude` child process: argv, pipes, stderr ring, group kill.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::OnceLock;

const SIGTERM: i32 = libc::SIGTERM;
const SIGKILL: i32 = libc::SIGKILL;

/// `pipe2(O_CLOEXEC)` on Linux. macOS has no `pipe2`; set the flag on a plain pipe.
///
/// # Safety
/// `fds` must point at two `c_int`s.
unsafe fn cloexec_pipe(fds: *mut libc::c_int) -> libc::c_int {
    #[cfg(target_os = "linux")]
    {
        libc::pipe2(fds, libc::O_CLOEXEC)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let rc = libc::pipe(fds);
        if rc != 0 {
            return rc;
        }
        for i in 0..2 {
            let fd = *fds.add(i);
            let fl = libc::fcntl(fd, libc::F_GETFD);
            if fl < 0 || libc::fcntl(fd, libc::F_SETFD, fl | libc::FD_CLOEXEC) < 0 {
                return -1;
            }
        }
        0
    }
}

/// The executable that runs as the guard (`<exe> --guard <bin> <args>`), set once by the
/// frontend, normally to its own path. Unset (tests, other callers) means claude is started
/// directly in its own process group, as before.
static GUARD_EXE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_guard_exe(exe: PathBuf) {
    let _ = GUARD_EXE.set(exe);
}

const STDERR_LINES: usize = 40;

#[derive(Clone, Debug)]
pub enum SessionArg {
    /// First launch of a session id we chose (`--session-id`).
    New(String),
    /// Continue an existing transcript (`--resume`).
    Resume(String),
    /// A copy of `from` cut back to the message `at` (`--resume-session-at`), saved under
    /// `new_id` so the original stays as it was. Used by rewind.
    Fork {
        from: String,
        at: String,
        new_id: String,
    },
}

impl SessionArg {
    pub fn id(&self) -> &str {
        match self {
            SessionArg::New(i) | SessionArg::Resume(i) => i,
            SessionArg::Fork { new_id, .. } => new_id,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Launch {
    pub bin: OsString,
    pub cwd: PathBuf,
    pub session: SessionArg,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: String,
    /// Extra argv from `OPENC_CLAUDE_ARGS`, split on whitespace.
    pub extra: Vec<String>,
    /// Let Agent and Bash run in the background (`OPENC_CLAUDE_BACKGROUND=1`).
    pub background: bool,
}

/// Thinking is only streamed as text when the CLI is told to summarize it; the default
/// sends an empty block that still carries a signature.
pub const SETTINGS: &str = r#"{"showThinkingSummaries":true}"#;

pub fn build_args(l: &Launch) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-mode",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(l.mode.clone());
    // This does not skip anything by itself: it only makes `bypassPermissions` selectable from
    // the mode picker later. The session starts in `l.mode`, and approvals still arrive as
    // `can_use_tool` requests unless the user chooses bypass.
    a.push("--allow-dangerously-skip-permissions".into());
    // Approvals come to us as `can_use_tool` control requests; without this the CLI denies
    // anything that would prompt.
    a.extend(["--permission-prompt-tool".into(), "stdio".into()]);
    a.push("--settings".into());
    a.push(SETTINGS.into());
    match &l.session {
        SessionArg::New(id) => a.extend(["--session-id".into(), id.clone()]),
        SessionArg::Resume(id) => a.extend(["--resume".into(), id.clone()]),
        SessionArg::Fork { from, at, new_id } => a.extend([
            "--resume".into(),
            from.clone(),
            "--resume-session-at".into(),
            at.clone(),
            "--fork-session".into(),
            "--session-id".into(),
            new_id.clone(),
        ]),
    }
    if let Some(m) = &l.model {
        a.extend(["--model".into(), m.clone()]);
    }
    if let Some(e) = &l.effort {
        a.extend(["--effort".into(), e.clone()]);
    }
    a.extend(l.extra.iter().cloned());
    a
}

pub struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    pid: Option<u32>,
    /// Write end of the guard's control pipe. Closing it, by dropping or by openc dying in any
    /// way, tells the guard to end claude and everything claude started.
    ctl: Option<OwnedFd>,
    pub lines: mpsc::UnboundedReceiver<String>,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

impl Proc {
    pub fn start(l: &Launch) -> Result<Proc> {
        check_launchable(l)?;
        let guard = GUARD_EXE.get().filter(|_| cfg!(target_os = "linux"));
        let mut cmd = match guard {
            Some(exe) => {
                let mut c = Command::new(exe);
                c.arg("--guard").arg(&l.bin);
                c
            }
            None => Command::new(&l.bin),
        };
        cmd.args(build_args(l))
            .current_dir(&l.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut ctl = None;
        let mut ctl_read_end: Option<OwnedFd> = None;
        if guard.is_some() {
            // The guard cleans up after claude, so it must not be killed with the rest of
            // the tree when openc drops this handle.
            cmd.kill_on_drop(false);
            let mut fds = [0 as libc::c_int; 2];
            if unsafe { cloexec_pipe(fds.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error()).context("pipe for the guard");
            }
            let (r, w) = (fds[0], fds[1]);
            ctl = Some(unsafe { OwnedFd::from_raw_fd(w) });
            // Own session: the terminal's SIGHUP and ctrl+c are for openc, and the guard has to
            // outlive openc to do its job. claude and its tools inherit the session.
            unsafe {
                cmd.pre_exec(move || {
                    libc::setsid();
                    if r == crate::guard::CTL_FD {
                        let fl = libc::fcntl(r, libc::F_GETFD);
                        libc::fcntl(r, libc::F_SETFD, fl & !libc::FD_CLOEXEC);
                    } else if libc::dup2(r, crate::guard::CTL_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            // Our copy of the read end is closed after the spawn below.
            ctl_read_end = Some(unsafe { OwnedFd::from_raw_fd(r) });
        } else {
            cmd.kill_on_drop(true)
                // Own process group so one signal reaches the tools it spawned, and so a
                // Ctrl-C on the frontend's terminal does not hit it directly.
                .process_group(0);
            // A frontend that dies without running destructors must not leave the CLI behind.
            #[cfg(target_os = "linux")]
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, SIGKILL as libc::c_ulong, 0, 0, 0);
                    Ok(())
                });
            }
        }
        // Without this `rewind_files` has no snapshots to restore from.
        cmd.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "1");
        if !l.background {
            // Background subagents end the turn at "launched" and report later through an
            // unsolicited turn. Foreground keeps one prompt, one result.
            cmd.env("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1");
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to start {}", l.bin.to_string_lossy()))?;
        drop(ctl_read_end);
        let pid = child.id();
        if let Some(p) = pid {
            agent_core::procs::register(p as i32);
        }
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;

        let (tx, lines) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut rd = BufReader::new(stdout).lines();
            while let Ok(Some(l)) = rd.next_line().await {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let r2 = ring.clone();
        tokio::spawn(async move {
            let mut rd = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = rd.next_line().await {
                let mut g = r2.lock().unwrap_or_else(|e| e.into_inner());
                if g.len() == STDERR_LINES {
                    g.pop_front();
                }
                g.push_back(l);
            }
        });
        Ok(Proc {
            child,
            stdin,
            pid,
            ctl,
            lines,
            stderr: ring,
        })
    }

    pub async fn send(&mut self, v: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("stdin closed")?;
        let mut line = serde_json::to_vec(v)?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub fn stderr_tail(&self) -> String {
        let g = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .rev()
            .take(8)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn signal_group(&self, sig: i32) {
        let Some(pid) = self.pid else { return };
        if self.ctl.is_some() {
            // The guard is the process to talk to; it ends claude's whole tree. SIGKILL has no
            // grace period, so it maps to the guard's "now" signal and the guard stays alive to
            // do the killing.
            let sig = if sig == SIGKILL { libc::SIGUSR1 } else { sig };
            unsafe {
                libc::kill(pid as i32, sig);
            }
            return;
        }
        // Negative pid is the process group, which `process_group(0)` made equal to the pid.
        unsafe {
            libc::kill(-(pid as i32), sig);
        }
    }

    /// Exit status text once stdout has closed, for the Fatal message.
    pub async fn exit_text(&mut self) -> String {
        match tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await {
            Ok(Ok(st)) => st.to_string(),
            _ => "status unknown".to_string(),
        }
    }

    /// Close stdin so the CLI finishes and exits, then escalate.
    pub async fn shutdown(&mut self, grace: Duration) {
        self.stdin.take();
        if tokio::time::timeout(grace, self.child.wait()).await.is_ok() {
            return;
        }
        self.signal_group(SIGTERM);
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            self.signal_group(SIGKILL);
            let _ = self.child.wait().await;
        }
    }

    pub async fn kill_now(&mut self) {
        self.stdin.take();
        self.signal_group(SIGKILL);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // kill_on_drop only reaches the direct child.
        if matches!(self.child.try_wait(), Ok(None)) {
            self.signal_group(SIGKILL);
        }
        if let Some(p) = self.pid {
            agent_core::procs::unregister(p as i32);
        }
    }
}

/// Fail before spawning with a message that names the fix. The guard would otherwise report a
/// missing binary only after the fact, and a bad cwd surfaces as a bare "Not a directory".
fn check_launchable(l: &Launch) -> Result<()> {
    if !l.cwd.is_dir() {
        anyhow::bail!("{} is not a directory", l.cwd.display());
    }
    let name = l.bin.to_string_lossy();
    let found = if name.contains('/') {
        is_executable(std::path::Path::new(&l.bin))
    } else {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| is_executable(&d.join(&l.bin))))
    };
    if !found {
        anyhow::bail!(
            "could not find {name} (it is looked up on PATH; set OPENC_CLAUDE_BIN to the claude binary)"
        );
    }
    Ok(())
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(session: SessionArg) -> Launch {
        Launch {
            bin: "claude".into(),
            cwd: "/tmp".into(),
            session,
            model: None,
            effort: None,
            mode: "bypassPermissions".into(),
            extra: vec![],
            background: false,
        }
    }

    #[test]
    fn args_for_new_and_resumed_sessions() {
        let a = build_args(&launch(SessionArg::New("abc".into())));
        assert!(a.windows(2).any(|w| w == ["--session-id", "abc"]));
        assert!(a
            .windows(2)
            .any(|w| w == ["--permission-mode", "bypassPermissions"]));
        assert!(a.contains(&"--include-partial-messages".to_string()));
        assert!(!a.contains(&"--resume".to_string()));

        let mut l = launch(SessionArg::Resume("abc".into()));
        l.model = Some("haiku".into());
        l.effort = Some("low".into());
        let a = build_args(&l);
        assert!(a.windows(2).any(|w| w == ["--resume", "abc"]));
        assert!(a.windows(2).any(|w| w == ["--model", "haiku"]));
        assert!(a.windows(2).any(|w| w == ["--effort", "low"]));
        assert!(!a.contains(&"--session-id".to_string()));

        let a = build_args(&launch(SessionArg::Fork {
            from: "old".into(),
            at: "m1".into(),
            new_id: "new".into(),
        }));
        assert!(a.windows(2).any(|w| w == ["--resume", "old"]));
        assert!(a.windows(2).any(|w| w == ["--resume-session-at", "m1"]));
        assert!(a.windows(2).any(|w| w == ["--session-id", "new"]));
        assert!(a.contains(&"--fork-session".to_string()));
    }
}
