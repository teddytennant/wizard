//! Claude Code's `statusLine` setting: a command that is handed the session as JSON on stdin and
//! prints rows that sit under the composer, above the status bar.
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

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::App;

const DEBOUNCE: Duration = Duration::from_millis(300);
/// A script that has not answered by now is killed; its last rows stay.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Rows a status line may take.
pub const MAX_ROWS: usize = 4;

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

fn parse(v: &serde_json::Value) -> Option<Cfg> {
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
}

impl App {
    /// Read the `statusLine` setting and ask for a first run.
    pub fn load_status_line(&mut self, home: &Path, project: &Path) {
        self.status_line.cfg = load(home, project);
        self.status_line.lines.clear();
        self.statusline_trigger();
    }

    /// Something the script may want to show has changed.
    pub fn statusline_trigger(&mut self) {
        if self.status_line.on() && self.status_line.wanted_at.is_none() {
            self.status_line.wanted_at = Some(Instant::now());
        }
    }

    /// The session as the script reads it.
    pub fn statusline_payload(&self) -> serde_json::Value {
        let cwd = self.project_root.display().to_string();
        let total = self.status.prompt_tokens + self.status.completion_tokens;
        let window = (self.config.max_context_tokens as u64).max(1);
        let mut v = serde_json::json!({
            "session_id": self.session_id,
            "transcript_path": crate::config::Config::sessions_dir()
                .map(|d| d.join(format!("{}.jsonl", self.session_id)).display().to_string())
                .unwrap_or_default(),
            "cwd": cwd,
            "model": { "id": self.status.model, "display_name": self.status.model },
            "workspace": { "current_dir": cwd, "project_dir": cwd },
            "version": env!("CARGO_PKG_VERSION"),
            "output_style": { "name": "default" },
            "cost": { "total_cost_usd": 0.0 },
            "context_window": {
                "total_input_tokens": self.status.prompt_tokens,
                "total_output_tokens": self.status.completion_tokens,
                "context_window_size": window,
                "used_percentage": (self.status.context_tokens * 100 / window).min(100),
                "remaining_percentage": 100 - (self.status.context_tokens * 100 / window).min(100),
            },
            "exceeds_200k_tokens": total > 200_000,
        });
        if let Some(label) = self.vim.label() {
            v["vim"] = serde_json::json!({ "mode": label });
        }
        v
    }

    /// Called on every tick: take a finished run's rows, start the run a trigger asked for.
    /// True when the rows changed.
    pub fn statusline_tick(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.status_line.rx {
            if let Ok(lines) = rx.try_recv() {
                changed = lines != self.status_line.lines;
                self.status_line.lines = lines;
                self.status_line.rx = None;
            }
        }
        let st = &mut self.status_line;
        let Some(cfg) = st.cfg.clone() else {
            return changed;
        };
        if st.wanted_at.is_none() {
            if let (Some(every), Some(last)) = (cfg.refresh, st.last_run) {
                if last.elapsed() >= every {
                    st.wanted_at = Some(Instant::now() - DEBOUNCE);
                }
            }
        }
        if st.wanted_at.is_some_and(|at| at.elapsed() >= DEBOUNCE) {
            st.wanted_at = None;
            st.last_run = Some(Instant::now());
            let payload = self.statusline_payload().to_string();
            let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
            let (tx, rx) = channel();
            self.status_line.rx = Some(rx);
            let cwd = self.project_root.clone();
            std::thread::spawn(move || {
                tx.send(run(&cfg.command, &payload, &cwd, w, h)).ok();
            });
        }
        changed
    }

    /// Rows the status line takes under the composer right now.
    pub fn statusline_rows(&self) -> u16 {
        if !self.status_line.on() || self.picker.is_some() || self.plan_review.is_some() {
            return 0;
        }
        self.status_line.lines.len().min(MAX_ROWS) as u16
    }

    /// The rows, ready to draw: padded, colours applied.
    pub fn statusline_spans(&self) -> Vec<Line<'static>> {
        let pad = " ".repeat(1 + self.status_line.cfg.as_ref().map_or(0, |c| c.padding));
        self.status_line
            .lines
            .iter()
            .take(MAX_ROWS)
            .map(|l| {
                let mut spans = vec![Span::raw(pad.clone())];
                spans.extend(ansi_spans(l));
                Line::from(spans)
            })
            .collect()
    }

    /// `/statusline [command | off]`. With no argument it says what is set.
    pub fn statusline_command(&mut self, arg: &str) {
        match std::env::var_os("HOME") {
            Some(home) => self.statusline_command_in(Path::new(&home), arg),
            None => self.notice("no home directory to keep the setting in"),
        }
    }

    pub(super) fn statusline_command_in(&mut self, home: &Path, arg: &str) {
        let arg = arg.trim();
        let path = home.join(".claude/settings.json");
        match arg {
            "" => {
                let said = match &self.status_line.cfg {
                    Some(c) => format!("status line: {}", c.command),
                    None => {
                        "no status line. `/statusline <command>` sets one: the command gets the \
                             session as JSON on stdin and prints rows for the footer"
                            .to_string()
                    }
                };
                self.notice(said);
            }
            "off" | "remove" | "clear" => {
                edit_settings(&path, |doc| {
                    doc.remove("statusLine");
                });
                self.status_line.cfg = None;
                self.status_line.lines.clear();
                self.notice("status line removed");
            }
            command => {
                edit_settings(&path, |doc| {
                    doc.insert(
                        "statusLine".into(),
                        serde_json::json!({ "type": "command", "command": command }),
                    );
                });
                self.status_line.cfg = parse(&serde_json::json!({ "command": command }));
                self.statusline_trigger();
                self.notice(format!("status line set to: {command}"));
            }
        }
    }
}

fn edit_settings(path: &Path, f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>)) {
    let mut doc = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(map) = doc.as_object_mut() {
        f(map);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(text) = serde_json::to_string_pretty(&doc) {
        std::fs::write(path, text + "\n").ok();
    }
}

/// Run the command with the payload on stdin; its lines, or none when it fails.
fn run(command: &str, payload: &str, cwd: &Path, cols: u16, rows: u16) -> Vec<String> {
    let Ok(mut child) = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("COLUMNS", cols.to_string())
        .env("LINES", rows.to_string())
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
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
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(15))
            }
            _ => {
                child.kill().ok();
                child.wait().ok();
                break;
            }
        }
    }
    reader
        .join()
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .take(MAX_ROWS)
        .collect()
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
            "cat; echo; echo \"$COLUMNS x $LINES\"; echo \"$CLAUDE_CODE_CHILD_SESSION\"",
            "{\"model\":\"m\"}",
            Path::new("/"),
            90,
            30,
        );
        assert_eq!(lines, ["{\"model\":\"m\"}", "90 x 30", "1"]);
    }

    #[test]
    fn a_failing_command_shows_nothing() {
        assert!(run("exit 3", "{}", Path::new("/"), 80, 24).is_empty());
        assert!(run("/nonexistent/thing", "{}", Path::new("/"), 80, 24).is_empty());
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
}
