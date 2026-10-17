//! Real-terminal behaviour in a private tmux server: the conversation stays in the terminal's
//! own scrollback after exit, a resize replays it at the new width, Ctrl+C follows Codex's
//! order, the exit summary is printed after the terminal is restored, and the chat never
//! touches the alternate screen. Skipped when tmux or script(1) is missing.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

struct Tmux {
    sock: String,
    home: PathBuf,
    typescript: PathBuf,
}

fn have(cmd: &str) -> bool {
    Command::new(cmd).arg("--version").output().is_ok()
        || Command::new("which")
            .arg(cmd)
            .output()
            .is_ok_and(|o| o.status.success())
}

impl Tmux {
    fn new(name: &str, cols: u16, rows: u16) -> Option<Tmux> {
        if !have("tmux") || !have("script") {
            eprintln!("skipping: tmux or script missing");
            return None;
        }
        let sock = format!("cxwtest-{}-{name}", std::process::id());
        let home = std::env::temp_dir().join(format!("cxwtest-home-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("proj")).unwrap();
        std::fs::create_dir_all(home.join(".wizard/sessions")).unwrap();
        // The mock backend's session id is `mock-1`; a non-empty file makes the resume hint appear.
        std::fs::write(home.join(".wizard/sessions/mock-1.jsonl"), "{}\n").unwrap();
        let typescript = home.join("typescript");
        let bin = env!("CARGO_BIN_EXE_codexw");
        let inner = format!(
            "cd {home}/proj; env HOME={home} CODEXW_BACKEND=mock CODEXW_PLACEHOLDER=7 TERM=xterm-256color COLORTERM=truecolor {bin} -m gpt-5.5; echo \"[codexw exited $?]\"; sleep 20",
            home = home.display()
        );
        let cmd = format!(
            "script -q -f -c '{}' {}",
            inner.replace('\'', "'\\''"),
            typescript.display()
        );
        let t = Tmux {
            sock,
            home,
            typescript,
        };
        let ok = t
            .run(&[
                "new-session",
                "-d",
                "-s",
                "a",
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
                &cmd,
            ])
            .status
            .success();
        assert!(ok, "tmux new-session failed");
        t.run(&["set", "-s", "extended-keys", "on"]);
        t.run(&["set", "-s", "escape-time", "0"]);
        t.run(&["set", "-g", "window-style", "fg=#e6e6e6,bg=#000000"]);
        Some(t)
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .arg("-L")
            .arg(&self.sock)
            .args(args)
            .output()
            .expect("tmux")
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.run(&["capture-pane", "-p", "-t", "a"]).stdout).to_string()
    }

    /// Screen and scrollback, trailing spaces trimmed.
    fn full(&self) -> String {
        let s = String::from_utf8_lossy(
            &self
                .run(&["capture-pane", "-p", "-S", "-", "-t", "a"])
                .stdout,
        )
        .to_string();
        s.lines()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn keys(&self, k: &[&str]) {
        let mut a = vec!["send-keys", "-t", "a"];
        a.extend_from_slice(k);
        self.run(&a);
    }

    fn type_(&self, s: &str) {
        self.run(&["send-keys", "-t", "a", "-l", s]);
    }

    fn wait(&self, what: &str, f: impl Fn(&str) -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if f(&self.screen()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}; screen:\n{}", self.screen());
    }

    fn submit(&self, text: &str) {
        self.type_(text);
        std::thread::sleep(Duration::from_millis(400));
        self.keys(&["Enter"]);
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        self.run(&["kill-server"]);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn history_stays_in_scrollback_after_exit() {
    let Some(t) = Tmux::new("exit", 120, 36) else {
        return;
    };
    t.wait("header", |s| s.contains("Wizard"));
    t.submit("hello there");
    t.wait("the turn to finish", |s| {
        s.contains("hello there") && !s.contains("esc to interrupt") && s.contains("•")
    });
    t.keys(&["C-c"]);
    t.wait("exit", |s| s.contains("[codexw exited 0]"));
    let full = t.full();
    assert!(full.contains("Wizard (v0.0)"), "{full}");
    assert!(full.contains("› hello there"), "{full}");
    assert!(
        full.contains("To continue this session, run wizard-ui-codex resume mock-1"),
        "{full}"
    );
    // The summary sits directly under the last history row, with no composer left behind.
    // (tmux keeps a stale copy of the placeholder frame in scrollback, as it does for Codex.)
    let screen = t.screen();
    assert!(
        !screen.contains("gpt-5.5 default"),
        "composer rows must be cleared on exit:\n{screen}"
    );
    let lines: Vec<&str> = full.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.starts_with("To continue"))
        .unwrap();
    assert!(
        !lines[at - 1].is_empty(),
        "no blank row between history and summary:\n{full}"
    );
}

#[test]
fn resize_replays_history_at_the_new_width() {
    let Some(t) = Tmux::new("resize", 120, 36) else {
        return;
    };
    t.wait("header", |s| s.contains("Wizard"));
    t.submit("hello there");
    t.wait("the turn to finish", |s| {
        s.contains("hello there") && !s.contains("esc to interrupt") && s.contains("•")
    });
    t.run(&["resize-window", "-t", "a", "-x", "80", "-y", "24"]);
    t.wait("the replay", |s| s.contains("› hello there"));
    std::thread::sleep(Duration::from_millis(600));
    let full = t.full();
    let width = full.lines().map(|l| l.chars().count()).max().unwrap();
    assert!(
        width <= 80,
        "a row is {width} wide after the resize:\n{full}"
    );
    assert_eq!(full.matches("Wizard (v0.0)").count(), 1, "{full}");
    assert_eq!(full.matches("› hello there").count(), 1, "{full}");
    // The tip re-wrapped for 80 columns.
    let tip_rows = full
        .lines()
        .skip_while(|l| !l.contains("Tip:"))
        .take_while(|l| l.starts_with("  "))
        .count();
    assert!(tip_rows >= 2, "tip rows {tip_rows}\n{full}");
}

#[test]
fn ctrl_c_follows_codex_order() {
    let Some(t) = Tmux::new("ctrlc", 120, 36) else {
        return;
    };
    t.wait("header", |s| s.contains("Wizard"));
    // 1. a draft is cleared, the app keeps running
    t.type_("draft text");
    t.wait("draft", |s| s.contains("› draft text"));
    t.keys(&["C-c"]);
    t.wait("cleared draft", |s| {
        !s.contains("draft text") && s.contains("Use /skills")
    });
    assert!(!t.screen().contains("[codexw exited"));
    // 2. during a turn it interrupts and the app stays alive
    t.submit("slow please");
    t.wait("working", |s| s.contains("esc to interrupt"));
    t.keys(&["C-c"]);
    t.wait("interrupted", |s| s.contains("Conversation interrupted"));
    assert!(!t.screen().contains("[codexw exited"));
    // 3. idle and empty: quits at once
    t.keys(&["C-c"]);
    t.wait("exit", |s| s.contains("[codexw exited 0]"));
}

#[test]
fn chat_never_uses_the_alternate_screen() {
    let Some(t) = Tmux::new("alt", 120, 36) else {
        return;
    };
    t.wait("header", |s| s.contains("Wizard"));
    t.submit("hello there");
    t.wait("the turn to finish", |s| {
        s.contains("hello there") && !s.contains("esc to interrupt") && s.contains("•")
    });
    t.run(&["resize-window", "-t", "a", "-x", "90", "-y", "28"]);
    std::thread::sleep(Duration::from_millis(500));
    t.keys(&["C-d"]);
    t.wait("exit", |s| s.contains("[codexw exited 0]"));
    let bytes = std::fs::read(&t.typescript).unwrap();
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(!has(b"\x1b[?1049h"), "alt screen entered");
    assert!(!has(b"\x1b[?47h"));
    assert!(
        has(b"\x1b[?2026h") && has(b"\x1b[?2026l"),
        "frames are not synchronized"
    );
    assert!(
        has(b"\x1b[?2004h") && has(b"\x1b[?2004l"),
        "bracketed paste not paired"
    );
    assert!(
        has(b"\x1b[?1004h") && has(b"\x1b[?1004l"),
        "focus reporting not paired"
    );
    assert!(!has(b"\x1b[?1000h"), "mouse capture must stay off");
    // A resize wipes and replays: one scrollback purge.
    assert!(has(b"\x1b[3J"), "no scrollback purge on resize");
}

/// CPU ticks (utime + stime) of the codexw process that carries `marker` in its environment.
fn cpu_ticks(marker: &str) -> Option<u64> {
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let comm = std::fs::read_to_string(format!("/proc/{name}/comm")).unwrap_or_default();
        if comm.trim() != "codexw" {
            continue;
        }
        let env = std::fs::read(format!("/proc/{name}/environ")).unwrap_or_default();
        if !String::from_utf8_lossy(&env).contains(marker) {
            continue;
        }
        let stat = std::fs::read_to_string(format!("/proc/{name}/stat")).ok()?;
        let rest = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        // utime and stime are fields 14 and 15 of the line, 11 and 12 after the command.
        return Some(rest.get(11)?.parse::<u64>().ok()? + rest.get(12)?.parse::<u64>().ok()?);
    }
    None
}

#[test]
fn idle_codexw_uses_no_cpu() {
    let Some(t) = Tmux::new("idle", 120, 36) else {
        return;
    };
    t.wait("header", |s| s.contains("Wizard"));
    t.type_("a draft");
    t.wait("draft", |s| s.contains("› a draft"));
    std::thread::sleep(Duration::from_millis(500));
    // The pane's process has the test's temp HOME in its environment.
    let marker = t.home.display().to_string();
    let Some(a) = cpu_ticks(&marker) else { return };
    std::thread::sleep(Duration::from_secs(5));
    let b = cpu_ticks(&marker).unwrap();
    assert_eq!(b - a, 0, "an idle composer burned {} ticks in 5 s", b - a);
}
