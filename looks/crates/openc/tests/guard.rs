//! The guard owns claude's whole tool tree. claude's Bash tool starts each command in its own
//! session, so these tests start the stand-in tool with `setsid` the same way and then take
//! openc away: control pipe closed (what any death of openc looks like), claude exiting by
//! itself, and the polite and the immediate signal from openc.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // A zombie is dead; its parent has not reaped it yet.
        Ok(s) => s
            .rsplit(')')
            .next()
            .is_some_and(|r| !r.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

fn wait_dead(pid: u32, within: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < within {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    !alive(pid)
}

struct Rig {
    guard: Child,
    pids: Vec<u32>,
}

/// Start `openc --guard sh -c <script>` with its stdin as the control pipe (fd 3 is a copy of
/// it), where the script records the pids of the two tools it starts.
fn rig(script_tail: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!(
        "openc-guard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile: PathBuf = dir.join("pids");
    let script = format!(
        "setsid sleep 3987 & echo $! >> {p}; sleep 3986 & echo $! >> {p}; {script_tail}",
        p = pidfile.display()
    );
    let guard = Command::new("sh")
        .arg("-c")
        .arg(r#"exec "$0" --guard sh -c "$1" 3<&0"#)
        .arg(env!("CARGO_BIN_EXE_openc"))
        .arg(script)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let t = Instant::now();
    let pids = loop {
        let s = std::fs::read_to_string(&pidfile).unwrap_or_default();
        let v: Vec<u32> = s.lines().filter_map(|l| l.trim().parse().ok()).collect();
        if v.len() == 2 {
            break v;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "tools never started");
        std::thread::sleep(Duration::from_millis(20));
    };
    Rig { guard, pids }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.guard.kill();
        for p in &self.pids {
            unsafe {
                libc_kill(*p as i32, 9);
            }
        }
    }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

#[test]
fn closed_control_pipe_kills_tools_in_other_sessions() {
    let mut r = rig("wait");
    for p in &r.pids {
        assert!(alive(*p), "tool {p} should be running");
    }
    // What a SIGKILLed openc looks like to the guard: the write end of the pipe closes.
    drop(r.guard.stdin.take());
    for p in r.pids.clone() {
        assert!(wait_dead(p, Duration::from_secs(5)), "tool {p} survived");
    }
    r.guard.wait().unwrap();
}

#[test]
fn claude_exiting_takes_its_orphans_with_it_and_keeps_its_status() {
    // The script ends by itself, leaving both tools running.
    let mut r = rig("sleep 0.3; exit 7");
    // `wait` closes a child's stdin first, which here is the control pipe.
    let _ctl = r.guard.stdin.take();
    let st = r.guard.wait().unwrap();
    assert_eq!(st.code(), Some(7), "the guard reports claude's own status");
    for p in r.pids.clone() {
        assert!(wait_dead(p, Duration::from_secs(5)), "tool {p} survived");
    }
}

#[test]
fn sigterm_and_sigusr1_from_openc_end_the_tree() {
    for sig in ["TERM", "USR1"] {
        let r = rig("wait");
        let pid = r.guard.id();
        Command::new("kill")
            .args([&format!("-{sig}"), &pid.to_string()])
            .status()
            .unwrap();
        for p in r.pids.clone() {
            assert!(
                wait_dead(p, Duration::from_secs(5)),
                "{sig}: tool {p} survived"
            );
        }
    }
}

#[test]
fn a_missing_binary_is_an_exit_code_not_a_hang() {
    let out = Command::new(env!("CARGO_BIN_EXE_openc"))
        .args(["--guard", "/nonexistent/claude-for-test"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed to start"));
}
