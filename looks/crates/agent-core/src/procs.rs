//! Process groups that backends start, so a frontend that is told to die can take them with it.
//!
//! A backend runs its CLI in its own process group (so one signal reaches the tools it spawned)
//! and sets `PR_SET_PDEATHSIG` so the CLI dies with the frontend. That only reaches the direct
//! child: a `sleep 300` the CLI started in the same group survives. `Drop` handles a normal
//! quit, but a frontend killed by SIGHUP (the common case over SSH) or SIGTERM exits from a
//! signal thread and never runs destructors. That thread calls [`kill_all`].

use std::sync::Mutex;

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGKILL: i32 = 9;

static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

fn groups() -> std::sync::MutexGuard<'static, Vec<i32>> {
    GROUPS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Remember a process group the frontend started (the pid, when the child was spawned with
/// `process_group(0)`).
pub fn register(pgid: i32) {
    if pgid > 1 {
        let mut g = groups();
        if !g.contains(&pgid) {
            g.push(pgid);
        }
    }
}

/// The group has been reaped; forget it so a recycled pid is never signalled.
pub fn unregister(pgid: i32) {
    groups().retain(|p| *p != pgid);
}

/// Process groups currently registered.
pub fn registered() -> Vec<i32> {
    groups().clone()
}

/// SIGKILL every registered group. Safe to call from a signal-handling thread or a panic hook.
pub fn kill_all() {
    let pgids = std::mem::take(&mut *groups());
    for p in pgids {
        // SAFETY: kill(2) with a negative pid signals a process group and has no memory effects.
        unsafe {
            kill(-p, SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::Command;

    #[test]
    fn kill_all_takes_down_a_registered_group_and_its_children() {
        // `sh` starts a `sleep` in the same group and waits for it.
        let mut sh = Command::new("sh")
            .args(["-c", "sleep 300 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = sh.id() as i32;
        register(pgid);
        register(pgid);
        assert_eq!(registered().iter().filter(|p| **p == pgid).count(), 1);
        kill_all();
        let st = sh.wait().unwrap();
        assert_eq!(st.signal(), Some(9), "{st:?}");
        // The group's other members are gone too: nothing answers for the group any more.
        let mut alive = true;
        for _ in 0..50 {
            alive = unsafe { kill(-pgid, 0) } == 0;
            if !alive {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!alive, "the sleep survived");
        assert!(!registered().contains(&pgid));
    }

    #[test]
    fn unregister_forgets_a_group_and_nonsense_pids_are_ignored() {
        register(4_000_000);
        register(0);
        register(1);
        register(-5);
        assert_eq!(
            registered().into_iter().filter(|p| *p == 4_000_000).count(),
            1
        );
        unregister(4_000_000);
        assert!(!registered().contains(&4_000_000));
        assert!(!registered().iter().any(|p| *p <= 1));
    }
}
