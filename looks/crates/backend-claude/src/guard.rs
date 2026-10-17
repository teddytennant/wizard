//! The guard: a small process that sits between openc and `claude` and owns the whole tool
//! tree.
//!
//! claude's Bash tool starts every command in its own session, so a signal to claude's process
//! group never reaches a build or a `sleep` it started, and `PR_SET_PDEATHSIG` on claude only
//! kills claude itself (with SIGKILL, so it cannot clean up after itself either). Nothing in
//! openc can run when openc is SIGKILLed. So the cleanup lives here, in a process that does not
//! die with openc:
//!
//! * It is a child subreaper, so when claude dies or a tool's parent exits, the orphans come to
//!   it instead of to init. Every descendant stays reachable through `/proc`.
//! * openc holds the write end of a pipe (fd 3 here). End of file on it means openc is gone, by
//!   any route: quit, signal, SIGKILL, closed pty.
//! * claude exiting on its own, or a SIGTERM or SIGUSR1 from openc, end the same way.
//!
//! The end is: SIGTERM to every descendant, a short wait, SIGKILL to what is left, exit with
//! claude's status. Started as `openc --guard <bin> <args...>`.

use std::os::fd::RawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The control pipe's read end in the guard process.
pub const CTL_FD: RawFd = 3;
/// How long descendants get after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_millis(1500);

enum Why {
    /// openc closed the pipe, or asked politely with SIGTERM.
    Gone,
    /// openc asked for no grace (SIGUSR1).
    Kill,
    /// claude exited with this status.
    Exited(i32),
}

/// Entry point for `openc --guard`. `args` is `<bin> <claude args...>`. Returns the exit code.
pub fn run(args: Vec<std::ffi::OsString>) -> i32 {
    let mut it = args.into_iter();
    let Some(bin) = it.next() else {
        eprintln!("openc --guard: no command");
        return 2;
    };
    // A name `pkill openc` does not match, so a user killing openc cannot take the guard down
    // before it has cleaned up.
    unsafe {
        // Subreaper, pdeathsig and prctl(PR_SET_NAME) are Linux. On other unix the
        // guard still starts the child and watches the control pipe; it just cannot
        // adopt orphans through /proc, which does not exist there either.
        #[cfg(target_os = "linux")]
        {
            libc::prctl(
                libc::PR_SET_NAME,
                c"openc-guard".as_ptr() as libc::c_ulong,
                0,
                0,
                0,
            );
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1 as libc::c_ulong, 0, 0, 0);
        }
        // Not inherited by claude or its tools: fd 3 means something else to them.
        let fl = libc::fcntl(CTL_FD, libc::F_GETFD);
        if fl >= 0 {
            libc::fcntl(CTL_FD, libc::F_SETFD, fl | libc::FD_CLOEXEC);
        }
    }
    let (tx, rx) = mpsc::channel::<Why>();

    // Signals first, so none is lost between spawning claude and listening.
    match signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGUSR1,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
    ]) {
        Ok(mut sigs) => {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for s in sigs.forever() {
                    let why = if s == signal_hook::consts::SIGUSR1 {
                        Why::Kill
                    } else {
                        Why::Gone
                    };
                    if tx.send(why).is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => {
            eprintln!("openc --guard: signals: {e}");
            return 1;
        }
    }

    let mut cmd = Command::new(&bin);
    cmd.args(it)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // If the guard is killed outright, claude still goes. The guard's main thread never exits
    // before the guard does, so the "parent thread died" gotcha of this flag does not apply.
    // macOS has no PR_SET_PDEATHSIG; the process group is the fallback there.
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
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to start {}: {e}", bin.to_string_lossy());
            return 127;
        }
    };

    // End of file on the control pipe.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 64];
            loop {
                let n = unsafe { libc::read(CTL_FD, b.as_mut_ptr().cast(), b.len()) };
                if n == 0
                    || (n < 0
                        && std::io::Error::last_os_error().kind()
                            != std::io::ErrorKind::Interrupted)
                {
                    let _ = tx.send(Why::Gone);
                    return;
                }
            }
        });
    }
    // claude exiting by itself. This thread owns the Child, so it cannot be reaped twice.
    let (code_tx, code_rx) = mpsc::channel::<i32>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let code = match child.wait() {
                Ok(st) => st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0)),
                Err(_) => 1,
            };
            let _ = code_tx.send(code);
            let _ = tx.send(Why::Exited(code));
        });
    }

    let why = rx.recv().unwrap_or(Why::Gone);
    let exit = match why {
        Why::Exited(c) => c,
        _ => 0,
    };
    let grace = match why {
        Why::Kill => Duration::ZERO,
        // claude is gone already; only tools it left behind are, and they get no long wait.
        Why::Exited(_) => Duration::from_millis(300),
        Why::Gone => TERM_GRACE,
    };
    reap_all(grace);
    // Report claude's own status when it exited by itself, or what it did when we ended it.
    code_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or(exit)
}

/// Every pid whose ancestry leads to this process, from `/proc`. The subreaper flag is what
/// makes orphans part of that set.
pub fn descendants(root: u32) -> Vec<u32> {
    let mut parent_of: Vec<(u32, u32)> = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        // "pid (comm) S ppid ...": comm can hold spaces and parens, so cut at the last ')'.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let mut f = rest.split_whitespace();
        let (_state, ppid) = (f.next(), f.next().and_then(|p| p.parse::<u32>().ok()));
        if let Some(pp) = ppid {
            parent_of.push((pid, pp));
        }
    }
    let mut out = Vec::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for &(pid, pp) in &parent_of {
            if pp == p && !out.contains(&pid) {
                out.push(pid);
                frontier.push(pid);
            }
        }
    }
    out
}

fn signal_all(sig: libc::c_int) -> usize {
    let me = std::process::id();
    let pids = descendants(me);
    for &p in &pids {
        unsafe {
            libc::kill(p as libc::pid_t, sig);
        }
    }
    pids.len()
}

/// SIGTERM everything under this process, wait up to `grace`, SIGKILL what is left, and keep
/// going until nothing is. Reaps as it goes, since a zombie still counts as a descendant.
fn reap_all(grace: Duration) {
    let start = Instant::now();
    if !grace.is_zero() {
        signal_all(libc::SIGTERM);
    }
    let mut killed = grace.is_zero();
    let hard_stop = start + grace + Duration::from_secs(5);
    loop {
        reap_zombies();
        if descendants(std::process::id()).is_empty() {
            return;
        }
        if !killed && start.elapsed() >= grace {
            killed = true;
        }
        if killed {
            // A tool that forks as it dies needs another pass, so this repeats.
            signal_all(libc::SIGKILL);
        }
        if Instant::now() > hard_stop {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn reap_zombies() {
    loop {
        let r = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if r <= 0 {
            return;
        }
    }
}
