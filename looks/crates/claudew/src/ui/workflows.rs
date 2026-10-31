//! The workflow panel: what `/workflows` opens, drawn the way Claude Code 2.1.314 draws it.
//!
//! Claude's panel replaces the transcript below the spinner row: a ▔ edge, the run's name and
//! description with `done/total agents · elapsed` against the right edge, then one rounded box
//! split in two. The left column lists the phases (`❯ 1 Scan   1/2`), the right the agents of the
//! selected phase (`✔ scan:quick  worker  0s`), and one level further in a single agent's prompt
//! and outcome. A line of key hints sits on the last row.
//!
//! The data is wizard's, read from the files a run keeps in `~/.wizard/workflows/runs/<id>/`:
//! `run.json` for the run and `agents.json` for its agents, which the process running it
//! rewrites every second. A stop is a file too (`stop`), which that process checks as often.
//! Nothing here talks to the backend, so the panel works the same for a run started by this
//! session, another one, or `wizard -p`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use serde_json::Value;

use crate::text::{truncate, width, Row};
use crate::theme::Theme;

/// Width of the phases column, borders excluded.
const LEFT: usize = 16;
/// Runs kept, newest first.
const KEPT: usize = 12;
/// Rows of the transcript left above the panel.
pub const KEEP: u16 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Runs,
    Phases,
    Agents,
    Detail,
}

/// Where the cursor is in the panel. The runs themselves are read fresh on every draw.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    level: Level,
    run: usize,
    phase: usize,
    agent: usize,
    scroll: usize,
    note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct Agent {
    label: String,
    phase: Option<String>,
    state: String,
    kind: String,
    started_ms: Option<u64>,
    ended_ms: Option<u64>,
    prompt: String,
    outcome: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct Run {
    id: String,
    name: String,
    description: String,
    status: String,
    started_ms: u64,
    ended_ms: Option<u64>,
    pid: u64,
    phases: Vec<String>,
    agents: Vec<Agent>,
}

impl Run {
    fn running(&self) -> bool {
        self.status == "running"
    }

    fn phase_list(&self) -> Vec<String> {
        let mut out = self.phases.clone();
        for agent in &self.agents {
            if let Some(phase) = &agent.phase {
                if !out.contains(phase) {
                    out.push(phase.clone());
                }
            }
        }
        if self.agents.iter().any(|a| a.phase.is_none()) {
            out.push("Other".to_string());
        }
        out
    }

    fn agents_of(&self, phase: &str) -> Vec<&Agent> {
        let unnamed = phase == "Other" && !self.phases.iter().any(|p| p == "Other");
        self.agents
            .iter()
            .filter(|a| match &a.phase {
                Some(own) => own == phase,
                None => unnamed,
            })
            .collect()
    }

    fn done(agent: &Agent) -> bool {
        !matches!(agent.state.as_str(), "queued" | "running")
    }

    fn progress(&self) -> (usize, usize) {
        (
            self.agents.iter().filter(|a| Run::done(a)).count(),
            self.agents.len(),
        )
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn root() -> Option<PathBuf> {
    let base = match std::env::var_os("WIZARD_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".wizard"),
    };
    Some(base.join("workflows").join("runs"))
}

fn alive(pid: u64) -> bool {
    pid != 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn read_run(dir: &std::path::Path) -> Option<Run> {
    let run: Value = serde_json::from_slice(&std::fs::read(dir.join("run.json")).ok()?).ok()?;
    let board: Value = std::fs::read(dir.join("agents.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or(Value::Null);
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let num = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64);
    let pid = num(&run, "pid").unwrap_or(0);
    let mut status = text(&run, "status").unwrap_or_default();
    if status == "running" && !alive(pid) {
        status = "interrupted".into();
    }
    let strings = |v: &Value, key: &str| -> Vec<String> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let agents = board
        .get("agents")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|a| Agent {
                    label: text(a, "label").unwrap_or_default(),
                    phase: text(a, "phase"),
                    state: text(a, "state").unwrap_or_default(),
                    kind: text(a, "agent").unwrap_or_default(),
                    started_ms: num(a, "started_ms"),
                    ended_ms: num(a, "ended_ms"),
                    prompt: text(a, "prompt").unwrap_or_default(),
                    outcome: text(a, "outcome"),
                    error: text(a, "error"),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Run {
        id: text(&run, "run_id")?,
        name: text(&run, "name").unwrap_or_default(),
        description: text(&run, "description").unwrap_or_default(),
        status,
        started_ms: num(&run, "started_ms").unwrap_or(0),
        ended_ms: num(&run, "ended_ms"),
        pid,
        phases: strings(&board, "phases"),
        agents,
    })
}

fn load() -> Vec<Run> {
    let Some(root) = root() else {
        return Vec::new();
    };
    let mut runs: Vec<Run> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| read_run(&e.path()))
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_ms));
    runs.truncate(KEPT);
    runs
}

/// The runs, read again when the last read is older than half a second.
fn runs() -> Vec<Run> {
    static CACHE: Mutex<Option<(Instant, Vec<Run>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, runs)) = cache.as_ref() {
        if at.elapsed().as_millis() < 500 {
            return runs.clone();
        }
    }
    let fresh = load();
    *cache = Some((Instant::now(), fresh.clone()));
    fresh
}

impl State {
    /// Open on the only run there is, or the only one running; else on the list of runs.
    pub fn open() -> State {
        let all = runs();
        let live: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, r)| r.running())
            .map(|(i, _)| i)
            .collect();
        let (level, run) = match (all.len(), live.as_slice()) {
            (1, _) => (Level::Phases, 0),
            (_, [one]) => (Level::Phases, *one),
            _ => (Level::Runs, 0),
        };
        State {
            level,
            run,
            phase: 0,
            agent: 0,
            scroll: 0,
            note: None,
        }
    }

    fn clamp(&mut self, all: &[Run]) {
        self.run = self.run.min(all.len().saturating_sub(1));
        if let Some(run) = all.get(self.run) {
            self.phase = self.phase.min(run.phase_list().len().saturating_sub(1));
            let n = run
                .phase_list()
                .get(self.phase)
                .map_or(0, |p| run.agents_of(p).len());
            self.agent = self.agent.min(n.saturating_sub(1));
        }
    }
}

/// Take a key. Returns the new state, or `None` when the panel closes.
pub fn key(mut st: State, key: KeyEvent) -> Option<State> {
    let all = runs();
    st.clamp(&all);
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    st.note = None;
    let run = all.get(st.run);
    let phases = run.map(Run::phase_list).unwrap_or_default();
    let agents = run
        .zip(phases.get(st.phase))
        .map_or(0, |(r, p)| r.agents_of(p).len());
    let step = |at: &mut usize, len: usize, down: bool| {
        if len == 0 {
            *at = 0;
        } else if down {
            *at = (*at + 1).min(len - 1);
        } else {
            *at = at.saturating_sub(1);
        }
    };
    match key.code {
        KeyCode::Esc | KeyCode::Left => match st.level {
            Level::Detail => st.level = Level::Agents,
            Level::Agents => st.level = Level::Phases,
            Level::Phases if all.len() > 1 => st.level = Level::Runs,
            _ => return None,
        },
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return None,
        KeyCode::Up | KeyCode::Down => {
            let down = key.code == KeyCode::Down;
            match st.level {
                Level::Runs => step(&mut st.run, all.len(), down),
                Level::Phases => {
                    step(&mut st.phase, phases.len(), down);
                    st.agent = 0;
                }
                Level::Agents => step(&mut st.agent, agents, down),
                // Claude's detail view moves between the phase's agents with the arrows.
                Level::Detail => {
                    step(&mut st.agent, agents, down);
                    st.scroll = 0;
                }
            }
        }
        KeyCode::Enter | KeyCode::Right => match st.level {
            Level::Runs if !all.is_empty() => {
                st.level = Level::Phases;
                st.phase = 0;
            }
            Level::Phases if agents > 0 => {
                st.level = Level::Agents;
                st.agent = 0;
            }
            Level::Agents if agents > 0 => {
                st.level = Level::Detail;
                st.scroll = 0;
            }
            _ => {}
        },
        KeyCode::Char('x') if plain => {
            if let Some(run) = run.filter(|r| r.running()) {
                st.note = Some(match root().map(|r| r.join(&run.id).join("stop")) {
                    Some(path) if std::fs::write(&path, b"stop\n").is_ok() => {
                        format!("stopping {}", run.id)
                    }
                    _ => format!("could not stop {}", run.id),
                });
            }
        }
        _ => {}
    }
    Some(st)
}

fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

fn agent_clock(a: &Agent) -> String {
    match (a.started_ms, a.ended_ms) {
        (Some(s), Some(e)) => clock(e.saturating_sub(s)),
        (Some(s), None) => clock(now_ms().saturating_sub(s)),
        _ => String::new(),
    }
}

fn glyph(th: &Theme, a: &Agent) -> (&'static str, Style) {
    match a.state.as_str() {
        "done" | "cached" => ("✔", th.fg(th.success)),
        "failed" => ("✘", th.fg(th.error)),
        "stopped" => ("■", th.fg(th.warning)),
        "queued" => ("◌", th.fg(th.subtle)),
        _ => ("●", th.fg(th.subtle)),
    }
}

fn pad(s: &str, w: usize) -> String {
    let have = width(s);
    if have >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - have))
    }
}

/// `text` against the right edge of `w` columns, after `left`.
fn spread(mut left: Row, right: Row, w: usize) -> Row {
    let gap = w.saturating_sub(left.width() + right.width()).max(1);
    left.push(" ".repeat(gap), Style::new());
    left.extend(right);
    left
}

/// The panel's rows: `top` fills the space under the edge, `bottom` is the key hints.
pub fn rows(th: &Theme, st: &State, w: usize, h: usize) -> (Vec<Row>, Vec<Row>) {
    let all = runs();
    let mut st = st.clone();
    st.clamp(&all);
    let box_h = h.saturating_sub(KEEP as usize + 6).max(3);
    let dim = th.dim();
    let mut top = vec![Row::new()];
    let mut hint = String::new();
    if st.level == Level::Runs || all.is_empty() {
        top.push(Row::of("   ", Style::new()).with(
            "Workflows",
            th.fg(th.suggestion).add_modifier(Modifier::BOLD),
        ));
        top.push(Row::new());
        top.push(Row::new());
        let mut body = Vec::new();
        if all.is_empty() {
            body.push(Row::of("No workflow runs yet.", dim));
        }
        for (i, run) in all.iter().enumerate() {
            let on = i == st.run;
            let style = if on { th.fg(th.suggestion) } else { dim };
            let (done, total) = run.progress();
            let left = Row::of(if on { "❯ " } else { "  " }, style)
                .with(format!("{}  {}", run.id, run.name), style);
            let took = run
                .ended_ms
                .unwrap_or_else(now_ms)
                .saturating_sub(run.started_ms);
            body.push(spread(
                left,
                Row::of(
                    format!("{} · {done}/{total} agents · {} ", run.status, clock(took)),
                    dim,
                ),
                w.saturating_sub(10),
            ));
        }
        boxed(th, &mut top, "Runs", body, w, box_h);
        hint = "↑↓ select · enter open · esc close".to_string();
    } else if let Some(run) = all.get(st.run) {
        let (done, total) = run.progress();
        top.push(
            Row::of("   ", Style::new())
                .with(&run.name, th.fg(th.suggestion).add_modifier(Modifier::BOLD)),
        );
        let took = run
            .ended_ms
            .unwrap_or_else(now_ms)
            .saturating_sub(run.started_ms);
        top.push(spread(
            Row::of("   ", Style::new()).with(truncate(&run.description, w / 2), th.dim()),
            Row::of(
                format!("{done}/{total} agents · {} ", clock(took)),
                th.dim(),
            ),
            w.saturating_sub(4),
        ));
        top.push(Row::new());
        let phases = run.phase_list();
        let phase = phases.get(st.phase).cloned().unwrap_or_default();
        let agents = run.agents_of(&phase);
        let stop = if run.running() {
            " · x stop workflow"
        } else {
            ""
        };
        match st.level {
            Level::Detail => {
                let on = st.agent.min(agents.len().saturating_sub(1));
                let left: Vec<Row> = agents
                    .iter()
                    .enumerate()
                    .map(|(i, a)| agent_item(th, a, i == on, false, 0))
                    .collect();
                let right = agents
                    .get(on)
                    .map(|a| detail(th, a, w.saturating_sub(LEFT + 12)))
                    .unwrap_or_default();
                let title = format!(
                    "{phase} · {} agent{}",
                    agents.len(),
                    if agents.len() == 1 { "" } else { "s" }
                );
                let right_title = agents.get(on).map(|a| a.label.clone()).unwrap_or_default();
                split_box(
                    th,
                    &mut top,
                    (&title, &right_title),
                    left,
                    right.into_iter().skip(st.scroll).collect(),
                    w,
                    box_h,
                );
                hint = format!("↑↓ agent{stop} · esc back");
            }
            level => {
                let focus = level == Level::Agents;
                let left: Vec<Row> = phases
                    .iter()
                    .enumerate()
                    .map(|(i, p)| phase_item(th, run, p, i, i == st.phase, !focus))
                    .collect();
                let label_w = agents.iter().map(|a| width(&a.label)).max().unwrap_or(0) + 2;
                let right: Vec<Row> = if agents.is_empty() {
                    vec![Row::of(" Not started yet", dim)]
                } else {
                    agents
                        .iter()
                        .enumerate()
                        .map(|(i, a)| agent_item(th, a, focus && i == st.agent, focus, label_w))
                        .collect()
                };
                let title = format!(
                    "{phase} · {} agent{}",
                    agents.len(),
                    if agents.len() == 1 { "" } else { "s" }
                );
                split_box(th, &mut top, ("Phases", &title), left, right, w, box_h);
                hint = format!("↑↓ select{stop} · esc back");
            }
        }
    }
    let note = st.note.clone().unwrap_or(hint);
    let bottom =
        vec![Row::of("   ", Style::new()).with(note, th.dim().add_modifier(Modifier::ITALIC))];
    (top, bottom)
}

fn phase_item(th: &Theme, run: &Run, phase: &str, at: usize, on: bool, arrow: bool) -> Row {
    let agents = run.agents_of(phase);
    let done = agents.iter().filter(|a| Run::done(a)).count();
    let count = if agents.is_empty() {
        String::new()
    } else {
        format!("{done}/{}", agents.len())
    };
    let accent = th.fg(th.suggestion);
    let (lead, num, name, tail) = if on {
        (accent, accent, accent, accent)
    } else {
        (
            Style::new(),
            th.fg(Color::Indexed(239)),
            if agents.is_empty() {
                th.dim()
            } else {
                Style::new()
            },
            th.fg(Color::Indexed(239)),
        )
    };
    let mut row = Row::new();
    row.push(" ", Style::new());
    row.push(if on && arrow { "❯" } else { " " }, lead);
    row.push(format!(" {}", at + 1), num);
    row.push(" ", Style::new());
    row.push(truncate(phase, LEFT - 9), name);
    let used = row.width();
    let gap = LEFT.saturating_sub(used + width(&count) + 1);
    row.push(" ".repeat(gap), Style::new());
    row.push(count, tail);
    row.push(" ", Style::new());
    row
}

fn agent_item(th: &Theme, a: &Agent, on: bool, focus: bool, label_w: usize) -> Row {
    let accent = th.fg(th.suggestion);
    let (mark, mark_style) = glyph(th, a);
    let mut row = Row::new();
    let label_style = match (on, a.state.as_str()) {
        (true, _) => accent,
        (_, "done" | "cached") if !focus => Style::new(),
        _ => th.dim(),
    };
    let rest_style = if on { accent } else { th.dim() };
    if label_w == 0 {
        // the list beside the detail view
        row.push(" ", Style::new());
        row.push(if on { "❯" } else { " " }, accent);
        row.push(" ", Style::new());
        row.push(mark, mark_style);
        row.push(" ", Style::new());
        row.push(truncate(&a.label, LEFT - 5), label_style);
        let gap = LEFT.saturating_sub(row.width());
        row.push(" ".repeat(gap), Style::new());
        return row;
    }
    row.push(" ", Style::new());
    row.push(if on { "❯" } else { " " }, accent);
    row.push(mark, mark_style);
    row.push(" ", Style::new());
    row.push(pad(&a.label, label_w), label_style);
    row.push(" ", Style::new());
    row.push(a.kind.clone(), rest_style);
    row.push(format!("\0{}", agent_clock(a)), rest_style);
    row
}

fn detail(th: &Theme, a: &Agent, w: usize) -> Vec<Row> {
    let (mark, mark_style) = glyph(th, a);
    let running = !Run::done(a);
    let word = match a.state.as_str() {
        "done" => "Completed",
        "cached" => "Completed (from the resume journal)",
        "failed" => "Failed",
        "stopped" => "Stopped before it finished",
        "queued" => "Waiting for an agent slot",
        _ => "Running",
    };
    let dim = th.dim();
    let mut out = vec![
        Row::of(" ", Style::new())
            .with(mark, mark_style)
            .with(" ", Style::new())
            .with(word, mark_style.add_modifier(Modifier::BOLD))
            .with(format!(" · {}", a.kind), dim),
        if running {
            Row::new()
        } else {
            Row::of(format!(" {}", agent_clock(a)), dim)
        },
    ];
    if !running {
        out.push(Row::new());
    }
    let heading = dim.add_modifier(Modifier::BOLD);
    let mut section = |title: &str, text: &str, indent: usize, style: Style| {
        out.push(Row::of(format!(" {title}"), heading));
        for line in text.lines() {
            let chars: Vec<char> = line.chars().collect();
            for chunk in chars.chunks(w.saturating_sub(indent + 2).max(8)) {
                out.push(Row::of(
                    format!(
                        " {}{}",
                        " ".repeat(indent - 1),
                        chunk.iter().collect::<String>()
                    ),
                    style,
                ));
            }
        }
        out.push(Row::new());
    };
    section("Prompt", &a.prompt, 5, dim);
    section(
        "Activity",
        if running {
            "No tool calls yet."
        } else {
            "No tool calls."
        },
        3,
        dim,
    );
    if let Some(error) = &a.error {
        section("Error", error, 3, th.fg(th.error));
    }
    match &a.outcome {
        Some(text) => section("Outcome", text, 3, Style::new()),
        None if running => section("Outcome", "Still running…", 3, dim),
        None => section("Outcome", "Not available.", 3, dim),
    }
    out
}

/// A box with one column.
fn boxed(th: &Theme, top: &mut Vec<Row>, title: &str, body: Vec<Row>, w: usize, box_h: usize) {
    let line = th.fg(Color::Indexed(231));
    let inner = w.saturating_sub(10);
    let mut edge = Row::of("   ", Style::new());
    let t = format!(" {title} ");
    edge.push("╭", line);
    edge.push(&t, line);
    edge.push("─".repeat(inner.saturating_sub(width(&t))), line);
    edge.push("╮", line);
    top.push(edge);
    for i in 0..box_h.saturating_sub(2) {
        let mut row = Row::of("   ", Style::new());
        row.push("│", line);
        let mut cell = body.get(i).cloned().unwrap_or_default();
        let fill = inner.saturating_sub(cell.width());
        cell.push(" ".repeat(fill), Style::new());
        row.extend(cell);
        row.push("│", line);
        top.push(row);
    }
    top.push(Row::of("   ", Style::new()).with(format!("╰{}╯", "─".repeat(inner)), line));
}

/// The split box: phases (or agents) on the left, the selected one's content on the right.
fn split_box(
    th: &Theme,
    top: &mut Vec<Row>,
    (left_title, right_title): (&str, &str),
    left: Vec<Row>,
    right: Vec<Row>,
    w: usize,
    box_h: usize,
) {
    let line = th.fg(Color::Indexed(231));
    let inner = w.saturating_sub(10);
    let right_w = inner.saturating_sub(LEFT + 1);
    let titled = |t: &str, space: usize| -> String {
        let t = truncate(t, space.saturating_sub(2));
        format!(" {t} {}", "─".repeat(space.saturating_sub(width(&t) + 2)))
    };
    let mut edge = Row::of("   ", Style::new());
    edge.push("╭", line);
    edge.push(titled(left_title, LEFT), line);
    edge.push("┬", line);
    edge.push(titled(right_title, right_w), line);
    edge.push("╮", line);
    top.push(edge);
    for i in 0..box_h.saturating_sub(2) {
        let mut row = Row::of("   ", Style::new());
        row.push("│", line);
        let mut l = left.get(i).cloned().unwrap_or_default();
        l.push(" ".repeat(LEFT.saturating_sub(l.width())), Style::new());
        row.extend(l);
        row.push("│", line);
        let mut r = right.get(i).cloned().unwrap_or_default();
        // an agent row carries its clock after a NUL, to be set against the right edge
        let mut clock_cell: Option<(String, Style)> = None;
        if let Some(at) = r.0.iter().position(|(s, _)| s.starts_with('\0')) {
            let (s, st) = r.0.remove(at);
            clock_cell = Some((s.trim_start_matches('\0').to_string(), st));
        }
        let clock_w = 6;
        match clock_cell {
            Some((text, st)) => {
                let gap = right_w.saturating_sub(r.width() + clock_w + 1);
                r.push(" ".repeat(gap), Style::new());
                r.push(format!("{text:>clock_w$}"), st);
                r.push(" ", Style::new());
            }
            None => {
                let fill = right_w.saturating_sub(r.width());
                r.push(" ".repeat(fill), Style::new());
            }
        }
        row.extend(r);
        row.push("│", line);
        top.push(row);
    }
    top.push(Row::of("   ", Style::new()).with(
        format!("╰{}┴{}╯", "─".repeat(LEFT), "─".repeat(right_w)),
        line,
    ));
}
