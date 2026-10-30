//! Claude Code's `statusLine` setting: a command that is handed the session as JSON on stdin and
//! prints rows for the footer.
//!
//! ```json
//! { "statusLine": { "type": "command", "command": "~/.claude/statusline.sh", "padding": 0 } }
//! ```
//!
//! The settings are read from `~/.claude/settings.json`, then the project's `settings.json` and
//! `settings.local.json`; the last one to name `statusLine` wins. The command runs in a thread
//! so the interface never waits for it: on start, after each turn, on a mode change and on the
//! `refreshInterval` timer (seconds), debounced to 300 ms, and a newer run replaces one in
//! flight. Each line it prints is a row; ANSI colours are drawn.
//!
//! This file has no dependencies beyond `ratatui`, `serde_json` and the standard library. The
//! wizard TUI uses it through `app::statusline` and the `claude` look in `looks/` compiles the
//! same file (`#[path]`), so both run the command, build its payload and read its colours alike.
//! It is written without `let` chains for the looks workspace, which is edition 2021.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

const DEBOUNCE: Duration = Duration::from_millis(300);
/// A script that has not answered by now is killed; its last rows stay.
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub struct Cfg {
    pub command: String,
    pub padding: usize,
    pub refresh: Option<Duration>,
}

/// The `statusLine` of the settings files, the last file that names one.
pub fn load(home: &Path, project: &Path) -> Option<Cfg> {
    let mut found = None;
    for file in [
        home.join(".claude/settings.json"),
        project.join(".claude/settings.json"),
        project.join(".claude/settings.local.json"),
    ] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(v) = doc.get("statusLine") {
            found = parse(v);
        }
    }
    found
}

pub fn parse(v: &serde_json::Value) -> Option<Cfg> {
    if v.get("type")
        .and_then(|t| t.as_str())
        .is_some_and(|t| t != "command")
    {
        return None;
    }
    let command = v.get("command")?.as_str()?.trim().to_string();
    if command.is_empty() {
        return None;
    }
    Some(Cfg {
        command,
        padding: v.get("padding").and_then(|p| p.as_u64()).unwrap_or(0) as usize,
        refresh: v
            .get("refreshInterval")
            .and_then(|r| r.as_f64())
            .map(|s| Duration::from_secs_f64(s.max(1.0))),
    })
}

/// What the status line has to show and the run behind it.
#[derive(Default, Debug)]
pub struct State {
    pub cfg: Option<Cfg>,
    pub lines: Vec<String>,
    rx: Option<Receiver<Vec<String>>>,
    /// A trigger is waiting out the debounce since this moment.
    wanted_at: Option<Instant>,
    last_run: Option<Instant>,
}

impl State {
    pub fn on(&self) -> bool {
        self.cfg.is_some()
    }

    /// Read the setting again and ask for a run.
    pub fn reload(&mut self, home: &Path, project: &Path) {
        self.cfg = load(home, project);
        self.lines.clear();
        self.trigger();
    }

    /// Something the script may want to show has changed.
    pub fn trigger(&mut self) {
        if self.on() && self.wanted_at.is_none() {
            self.wanted_at = Some(Instant::now());
        }
    }

    /// Call on every tick: take a finished run's rows, start the run a trigger asked for.
    /// `payload` builds the JSON only when a run starts. True when the rows changed.
    pub fn tick(
        &mut self,
        payload: impl FnOnce() -> String,
        run_in: &RunIn,
        max_rows: usize,
    ) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.rx {
            if let Ok(lines) = rx.try_recv() {
                changed = lines != self.lines;
                self.lines = lines;
                self.rx = None;
            }
        }
        let Some(cfg) = self.cfg.clone() else {
            return changed;
        };
        if self.wanted_at.is_none() {
            if let (Some(every), Some(last)) = (cfg.refresh, self.last_run) {
                if last.elapsed() >= every {
                    self.wanted_at = Some(Instant::now() - DEBOUNCE);
                }
            }
        }
        if self.wanted_at.is_some_and(|at| at.elapsed() >= DEBOUNCE) {
            self.wanted_at = None;
            self.last_run = Some(Instant::now());
            let payload = payload();
            let run_in = run_in.clone();
            let (tx, rx) = channel();
            self.rx = Some(rx);
            std::thread::spawn(move || {
                tx.send(run(&cfg.command, &payload, &run_in, max_rows)).ok();
            });
        }
        changed
    }

    /// When the next timer run is due, for a loop that sleeps between events.
    pub fn next_due(&self) -> Option<Instant> {
        let cfg = self.cfg.as_ref()?;
        if let Some(at) = self.wanted_at {
            return Some(at + DEBOUNCE);
        }
        if self.rx.is_some() {
            return Some(Instant::now() + Duration::from_millis(50));
        }
        Some(self.last_run? + cfg.refresh?)
    }
}

/// Where and how the command runs.
#[derive(Debug, Clone)]
pub struct RunIn {
    pub cwd: std::path::PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub session_id: String,
}

/// The session as Claude Code 2.1.314 hands it to the command. Fields Wizard cannot know are
/// left out or zero: no rate limits, no prompt cache, no PR or worktree.
pub struct Session<'a> {
    pub session_id: &'a str,
    pub transcript_path: String,
    pub cwd: &'a str,
    pub model_id: &'a str,
    pub model_name: &'a str,
    /// `low`, `medium`, ...; empty when the model has none.
    pub effort: &'a str,
    pub version: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    /// Tokens in context now.
    pub context_tokens: u64,
    pub window: u64,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub vim_mode: Option<&'a str>,
}

impl Session<'_> {
    pub fn payload(&self) -> serde_json::Value {
        let window = self.window.max(1);
        let used = (self.context_tokens * 100 / window).min(100);
        let total = self.input_tokens + self.output_tokens;
        let started = total > 0;
        let mut v = serde_json::json!({
            "session_id": self.session_id,
            "transcript_path": self.transcript_path,
            "cwd": self.cwd,
            "model": { "id": self.model_id, "display_name": self.model_name },
            "workspace": {
                "current_dir": self.cwd,
                "project_dir": self.cwd,
                "added_dirs": [],
            },
            "version": self.version,
            "output_style": { "name": "default" },
            "cost": {
                "total_cost_usd": self.cost_usd,
                "total_duration_ms": self.duration_ms,
                "total_api_duration_ms": 0,
                "total_lines_added": 0,
                "total_lines_removed": 0,
            },
            "context_window": {
                "total_input_tokens": self.input_tokens,
                "total_output_tokens": self.output_tokens,
                "context_window_size": window,
                "current_usage": if started {
                    serde_json::json!({
                        "input_tokens": self.input_tokens,
                        "output_tokens": self.output_tokens,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": self.cached_tokens,
                    })
                } else {
                    serde_json::Value::Null
                },
                "used_percentage": if started { serde_json::json!(used) } else { serde_json::Value::Null },
                "remaining_percentage": if started { serde_json::json!(100 - used) } else { serde_json::Value::Null },
            },
            "exceeds_200k_tokens": total > 200_000,
            "fast_mode": false,
            "thinking": { "enabled": !self.effort.is_empty() },
        });
        if !self.effort.is_empty() {
            v["effort"] = serde_json::json!({ "level": self.effort });
        }
        if let Some(mode) = self.vim_mode {
            v["vim"] = serde_json::json!({ "mode": mode });
        }
        v
    }
}

/// Run the command with the payload on stdin; its lines (blank ones dropped, trailing space
/// trimmed), or none when it fails.
pub fn run(command: &str, payload: &str, at: &RunIn, max_rows: usize) -> Vec<String> {
    let Ok(mut child) = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&at.cwd)
        .env("COLUMNS", at.cols.to_string())
        .env("LINES", at.rows.to_string())
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
        .env("CLAUDE_PROJECT_DIR", &at.cwd)
        .env("CLAUDE_CODE_SESSION_ID", &at.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes()).ok();
    }
    let mut out = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(o) = out.as_mut() {
            o.read_to_string(&mut text).ok();
        }
        text
    });
    let started = Instant::now();
    let failed = loop {
        match child.try_wait() {
            Ok(Some(status)) => break !status.success(),
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(15))
            }
            _ => {
                child.kill().ok();
                child.wait().ok();
                break true;
            }
        }
    };
    let text = reader.join().unwrap_or_default();
    if failed {
        return Vec::new();
    }
    text.lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .take(max_rows)
        .collect()
}

/// Set (`Some`) or remove (`None`) `statusLine` in the user's settings file.
pub fn save(home: &Path, command: Option<&str>) {
    let path = home.join(".claude/settings.json");
    let mut doc = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(map) = doc.as_object_mut() {
        match command {
            Some(c) => {
                map.insert(
                    "statusLine".into(),
                    serde_json::json!({ "type": "command", "command": c }),
                );
            }
            None => {
                map.remove("statusLine");
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(text) = serde_json::to_string_pretty(&doc) {
        std::fs::write(path, text + "\n").ok();
    }
}

/// A line with ANSI colour codes as styled spans. Other escape sequences (cursor moves, OSC 8
/// links) are dropped, so only text and colour get through.
pub fn ansi_spans(line: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut style = Style::new();
    let mut text = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            text.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                let mut last = ' ';
                for p in chars.by_ref() {
                    if p.is_ascii_alphabetic() {
                        last = p;
                        break;
                    }
                    params.push(p);
                }
                if last == 'm' {
                    if !text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut text), style));
                    }
                    style = sgr(style, &params);
                }
            }
            Some(']') => {
                chars.next();
                while let Some(p) = chars.next() {
                    if p == '\x07' || (p == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    spans
}

fn sgr(mut style: Style, params: &str) -> Style {
    let nums: Vec<u32> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => style = Style::new(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            c @ 30..=37 => style = style.fg(Color::Indexed((c - 30) as u8)),
            39 => style = style.fg(Color::Reset),
            c @ 40..=47 => style = style.bg(Color::Indexed((c - 40) as u8)),
            49 => style = style.bg(Color::Reset),
            c @ 90..=97 => style = style.fg(Color::Indexed((c - 90 + 8) as u8)),
            c @ 100..=107 => style = style.bg(Color::Indexed((c - 100 + 8) as u8)),
            c @ (38 | 48) => {
                let color = match nums.get(i + 1) {
                    Some(5) => {
                        let n = nums.get(i + 2).copied().unwrap_or(0) as u8;
                        i += 2;
                        Some(Color::Indexed(n))
                    }
                    Some(2) => {
                        let at = |k: usize| nums.get(i + k).copied().unwrap_or(0) as u8;
                        let rgb = Color::Rgb(at(2), at(3), at(4));
                        i += 4;
                        Some(rgb)
                    }
                    _ => None,
                };
                if let Some(color) = color {
                    style = if c == 38 {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn at(cols: u16, rows: u16) -> RunIn {
        RunIn {
            cwd: PathBuf::from("/"),
            cols,
            rows,
            session_id: "sid".into(),
        }
    }

    #[test]
    fn the_setting_is_read_and_the_last_file_wins() {
        let dir = std::env::temp_dir().join(format!("wz-sl-{}", std::process::id()));
        let (home, proj) = (dir.join("h"), dir.join("p"));
        std::fs::create_dir_all(home.join(".claude")).expect("home");
        std::fs::create_dir_all(proj.join(".claude")).expect("proj");
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"statusLine":{"type":"command","command":"echo user","padding":2}}"#,
        )
        .expect("write");
        let cfg = load(&home, &proj).expect("user setting");
        assert_eq!((cfg.command.as_str(), cfg.padding), ("echo user", 2));
        std::fs::write(
            proj.join(".claude/settings.local.json"),
            r#"{"statusLine":{"type":"command","command":"echo local","refreshInterval":0.2}}"#,
        )
        .expect("write");
        let cfg = load(&home, &proj).expect("local setting");
        assert_eq!(cfg.command, "echo local");
        // a refresh below a second is a second
        assert_eq!(cfg.refresh, Some(Duration::from_secs(1)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_a_command_is_a_status_line() {
        assert!(parse(&serde_json::json!({"type": "text", "command": "x"})).is_none());
        assert!(parse(&serde_json::json!({"type": "command"})).is_none());
        assert!(parse(&serde_json::json!({"command": "  "})).is_none());
    }

    #[test]
    fn the_command_gets_the_payload_and_the_terminal_size() {
        let lines = run(
            "cat; echo; echo \"$COLUMNS x $LINES\"; echo \"$CLAUDE_CODE_CHILD_SESSION $CLAUDE_CODE_SESSION_ID $CLAUDE_PROJECT_DIR\"",
            "{\"model\":\"m\"}",
            &at(90, 30),
            4,
        );
        assert_eq!(lines, ["{\"model\":\"m\"}", "90 x 30", "1 sid /"]);
    }

    #[test]
    fn a_failing_command_shows_nothing_even_after_printing() {
        assert!(run("exit 3", "{}", &at(80, 24), 4).is_empty());
        assert!(run("echo partial; exit 3", "{}", &at(80, 24), 4).is_empty());
        assert!(run("/nonexistent/thing", "{}", &at(80, 24), 4).is_empty());
    }

    #[test]
    fn blank_lines_go_and_the_row_cap_holds() {
        let lines = run("printf 'a  \\n\\n b\\nc\\nd\\n'", "{}", &at(80, 24), 3);
        assert_eq!(lines, ["a", " b", "c"]);
    }

    #[test]
    fn colours_become_styles_and_other_escapes_go() {
        let spans = ansi_spans(
            "\x1b[1;31mred\x1b[0m plain \x1b]8;;http://x\x07link\x1b]8;;\x07 \x1b[38;5;70mgreen\x1b[2J",
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "red plain link green");
        assert_eq!(spans[0].style.fg, Some(Color::Indexed(1)));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].style, Style::new());
        assert_eq!(
            spans.last().expect("a span").style.fg,
            Some(Color::Indexed(70))
        );
    }

    #[test]
    fn the_payload_has_the_fields_a_script_reads() {
        let mut s = Session {
            session_id: "s1",
            transcript_path: "/t.jsonl".into(),
            cwd: "/p",
            model_id: "claude-haiku-5-5",
            model_name: "Haiku 5.5",
            effort: "medium",
            version: "2.1.314",
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            context_tokens: 0,
            window: 1_000_000,
            cost_usd: 0.0,
            duration_ms: 12,
            vim_mode: None,
        };
        let v = s.payload();
        assert_eq!(v["model"]["display_name"], "Haiku 5.5");
        assert_eq!(v["effort"]["level"], "medium");
        assert_eq!(v["workspace"]["project_dir"], "/p");
        assert!(v["context_window"]["used_percentage"].is_null());
        assert!(v.get("vim").is_none());
        s.input_tokens = 120;
        s.output_tokens = 20;
        s.context_tokens = 140;
        s.vim_mode = Some("INSERT");
        let v = s.payload();
        assert_eq!(v["context_window"]["used_percentage"], 0);
        assert_eq!(v["context_window"]["remaining_percentage"], 100);
        assert_eq!(v["context_window"]["total_input_tokens"], 120);
        assert_eq!(v["vim"]["mode"], "INSERT");
    }
}
