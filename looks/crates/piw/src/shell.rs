// OWNER: shell (the `!cmd` runner)
//! Runs a `!cmd` line: output kept in a bounded tail, a wall-clock limit, and the whole process
//! tree stopped on every way out.
//!
//! The command runs as `sh -c <line>` under `piw --guard` when piw knows its own path (the guard
//! is backend-claude's: a child subreaper that ends the tree when it reads end of file on a pipe
//! piw holds, so it works for a quit, a signal, a panic and SIGKILL alike). Without it (tests)
//! the shell leads a process group of its own, and cancel, timeout and drop signal the group.
//!
//! Output is read into [`Tail`], not queued: a command that prints gigabytes costs the tail's
//! size, not the output's. The app is told when there is something new (at most every 40 ms)
//! and reads the tail itself.

use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

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

/// Most output kept, from the end.
pub const TAIL_BYTES: usize = 256 * 1024;
const PING_EVERY: Duration = Duration::from_millis(40);

/// How long a `!cmd` may run before it is stopped, unless the app was given another limit.
pub const DEFAULT_LIMIT: Duration = Duration::from_secs(600);

/// The last [`TAIL_BYTES`] of what the command printed.
#[derive(Default)]
pub struct Tail {
    buf: Vec<u8>,
    dropped: u64,
}

impl Tail {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        // trim in steps so a stream of small reads does not move the buffer every time
        if self.buf.len() > TAIL_BYTES * 2 {
            let cut = self.buf.len() - TAIL_BYTES;
            self.buf.drain(..cut);
            self.dropped += cut as u64;
        }
    }

    /// The tail as text, and whether anything before it was dropped. Starts at a line start when
    /// it had to cut.
    pub fn text(&self) -> (String, bool) {
        let skip = self.buf.len().saturating_sub(TAIL_BYTES);
        let cut = self.dropped > 0 || skip > 0;
        let mut s = String::from_utf8_lossy(&self.buf[skip..]).into_owned();
        if cut {
            if let Some(i) = s.find('\n') {
                s.drain(..=i);
            }
        }
        (s, cut)
    }
}

/// How a run ended.
#[derive(Debug)]
pub struct ShellEnd {
    pub code: Option<i32>,
    pub cancelled: bool,
    pub timed_out: bool,
}

#[derive(Debug)]
pub enum ShellMsg {
    /// There is new output; read [`ShellJob::tail`].
    Output,
    End(ShellEnd),
}

/// A running `!cmd`. Dropping it ends the tree.
pub struct ShellJob {
    /// Process to signal: the guard when there is one, else the shell (a group leader).
    pid: Arc<AtomicI32>,
    guarded: bool,
    /// Write end of the guard's pipe. Closing it, by dropping or by piw dying, ends the tree.
    _ctl: Option<OwnedFd>,
    tail: Arc<Mutex<Tail>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ShellJob {
    /// Start `line` in `cwd`. `sink` is called from tasks: `Output` while it prints, then `End`
    /// once. Needs a tokio runtime.
    pub fn spawn(
        line: &str,
        cwd: &Path,
        guard_exe: Option<&PathBuf>,
        limit: Duration,
        sink: impl Fn(ShellMsg) + Send + Sync + 'static,
    ) -> std::io::Result<ShellJob> {
        let mut cmd;
        let mut ctl: Option<OwnedFd> = None;
        match guard_exe {
            Some(exe) => {
                cmd = tokio::process::Command::new(exe);
                cmd.arg("--guard").arg("sh").arg("-c").arg(line);
                let mut fds = [0 as libc::c_int; 2];
                // SAFETY: cloexec_pipe fills the two descriptors and sets FD_CLOEXEC.
                if unsafe { cloexec_pipe(fds.as_mut_ptr()) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let (r, w) = (fds[0], fds[1]);
                // SAFETY: both are fresh descriptors this function owns.
                ctl = Some(unsafe { OwnedFd::from_raw_fd(w) });
                let read_end = unsafe { OwnedFd::from_raw_fd(r) };
                // SAFETY: only async-signal-safe calls between fork and exec.
                unsafe {
                    cmd.pre_exec(move || {
                        // own session: the terminal's ctrl+c and SIGHUP are for piw, and the
                        // guard must outlive piw to do its job
                        libc::setsid();
                        const CTL_FD: libc::c_int = backend_claude::guard::CTL_FD;
                        if r == CTL_FD {
                            let fl = libc::fcntl(r, libc::F_GETFD);
                            libc::fcntl(r, libc::F_SETFD, fl & !libc::FD_CLOEXEC);
                        } else if libc::dup2(r, CTL_FD) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                cmd.kill_on_drop(false);
                // the read end stays open here until the spawn is done
                let spawned = Self::start(&mut cmd, cwd);
                drop(read_end);
                return Self::run(spawned?, true, ctl, limit, sink);
            }
            None => {
                cmd = tokio::process::Command::new("sh");
                cmd.arg("-c").arg(line).process_group(0).kill_on_drop(true);
                // piw killed outright must not leave the shell behind (its children are the
                // guard's job; this is the fallback without one). macOS has no
                // PR_SET_PDEATHSIG; the process group above is what cancel signals there.
                #[cfg(target_os = "linux")]
                unsafe {
                    cmd.pre_exec(|| {
                        libc::prctl(
                            libc::PR_SET_PDEATHSIG,
                            libc::SIGKILL as libc::c_ulong,
                            0,
                            0,
                            0,
                        );
                        Ok(())
                    });
                }
            }
        }
        let child = Self::start(&mut cmd, cwd)?;
        Self::run(child, false, ctl, limit, sink)
    }

    fn start(
        cmd: &mut tokio::process::Command,
        cwd: &Path,
    ) -> std::io::Result<tokio::process::Child> {
        cmd.current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }

    fn run(
        mut child: tokio::process::Child,
        guarded: bool,
        ctl: Option<OwnedFd>,
        limit: Duration,
        sink: impl Fn(ShellMsg) + Send + Sync + 'static,
    ) -> std::io::Result<ShellJob> {
        let pid = Arc::new(AtomicI32::new(child.id().map_or(0, |p| p as i32)));
        let tail = Arc::new(Mutex::new(Tail::default()));
        let sink = Arc::new(sink);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        // both pipes feed one tail; a ping goes out at most every PING_EVERY
        let last_ping = Arc::new(Mutex::new(Instant::now() - PING_EVERY));
        let mut readers = Vec::new();
        for pipe in [
            child.stdout.take().map(Pipe::Out),
            child.stderr.take().map(Pipe::Err),
        ]
        .into_iter()
        .flatten()
        {
            let (tail, sink, last) = (tail.clone(), sink.clone(), last_ping.clone());
            readers.push(tokio::spawn(async move {
                let mut chunk = vec![0u8; 64 * 1024];
                let mut pipe = pipe;
                loop {
                    match pipe.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            tail.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(&chunk[..n]);
                            let due = {
                                let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
                                let due = l.elapsed() >= PING_EVERY;
                                if due {
                                    *l = Instant::now();
                                }
                                due
                            };
                            if due {
                                sink(ShellMsg::Output);
                            }
                        }
                    }
                }
            }));
        }
        let p = pid.clone();
        let t = tokio::spawn(async move {
            let deadline = tokio::time::sleep(limit);
            tokio::pin!(deadline);
            let (mut cancelled, mut timed_out) = (false, false);
            let status = tokio::select! {
                st = child.wait() => st.ok(),
                _ = &mut deadline => {
                    timed_out = true;
                    terminate(&mut child, &p, guarded).await
                }
                _ = &mut stop_rx => {
                    cancelled = true;
                    terminate(&mut child, &p, guarded).await
                }
            };
            // what a background child still holds open must not keep the result back
            for r in readers {
                let _ = tokio::time::timeout(Duration::from_millis(300), r).await;
            }
            p.store(0, Ordering::SeqCst);
            sink(ShellMsg::Output);
            sink(ShellMsg::End(ShellEnd {
                code: status.and_then(|s| s.code()),
                cancelled,
                timed_out,
            }));
        });
        drop(t);
        Ok(ShellJob {
            pid,
            guarded,
            _ctl: ctl,
            tail,
            stop: Some(stop_tx),
        })
    }

    /// What the command has printed so far, from the end, and whether the start was dropped.
    pub fn tail(&self) -> (String, bool) {
        self.tail.lock().unwrap_or_else(|e| e.into_inner()).text()
    }

    /// Stop it: SIGTERM to the tree now, SIGKILL after a grace period. The run then ends with
    /// `cancelled`.
    pub fn cancel(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    /// Kill the tree now and without waiting (piw is leaving).
    pub fn kill(&self) {
        let p = self.pid.load(Ordering::SeqCst);
        if p > 1 {
            // SAFETY: kill(2) on the process (guard) or group (shell) this job started. The guard
            // is a session leader, so its group is its own too.
            unsafe {
                if self.guarded {
                    libc::kill(p, libc::SIGTERM);
                } else {
                    libc::killpg(p, libc::SIGKILL);
                }
            }
        }
    }
}

impl Drop for ShellJob {
    fn drop(&mut self) {
        // The guard ends the tree when `_ctl` closes just after this; without a guard the
        // group is killed here.
        if !self.guarded {
            self.kill();
        }
    }
}

enum Pipe {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

impl Pipe {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Pipe::Out(p) => p.read(buf).await,
            Pipe::Err(p) => p.read(buf).await,
        }
    }
}

/// SIGTERM the tree (the guard ends it itself, with its own grace), SIGKILL it if the leader is
/// still there after a while.
async fn terminate(
    child: &mut tokio::process::Child,
    pid: &AtomicI32,
    guarded: bool,
) -> Option<std::process::ExitStatus> {
    let p = pid.load(Ordering::SeqCst);
    if p > 1 {
        // SAFETY: as in `kill`.
        unsafe {
            if guarded {
                libc::kill(p, libc::SIGTERM);
            } else {
                libc::killpg(p, libc::SIGTERM);
            }
        }
    }
    let grace = Duration::from_millis(if guarded { 4000 } else { 1500 });
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(st) => st.ok(),
        Err(_) => {
            if p > 1 {
                // SAFETY: as above; the whole group, for the guard's session leader too.
                unsafe { libc::killpg(p, libc::SIGKILL) };
            }
            child.wait().await.ok()
        }
    }
}
