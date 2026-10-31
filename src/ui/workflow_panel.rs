//! The workflow panel: what `/workflows` opens in the terminal UI.
//!
//! Laid out the way Claude Code's is: the run's name and progress on top, its
//! phases on the left and the agents of the selected phase on the right, one
//! level deeper for a single agent's prompt and outcome. It reads the run
//! records and boards [`crate::workflow`] keeps on disk, so it shows runs this
//! process started and runs another process did.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use serde_json::Value;

use super::{accent, dim, muted};
use crate::theme::{self, Token};
use crate::workflow::board::{self, AgentRow, AgentState, Board};
use crate::workflow::store::{self, RunRecord};
use crate::workflow::{live, script};

/// Runs listed, newest first.
const SHOWN_RUNS: usize = 12;
/// How often the files are read again while the panel is open.
const REFRESH: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Runs,
    Phases,
    Agents,
    Detail,
}

#[derive(Debug, Clone)]
struct RunView {
    record: RunRecord,
    status: &'static str,
    board: Board,
}

#[derive(Debug)]
pub struct WorkflowPanel {
    runs: Vec<RunView>,
    level: Level,
    run: usize,
    phase: usize,
    agent: usize,
    scroll: usize,
    note: Option<String>,
    read_at: Instant,
}

fn load() -> Vec<RunView> {
    store::list()
        .into_iter()
        .take(SHOWN_RUNS)
        .map(|record| {
            let board = board::read(&record.run_id).unwrap_or_default();
            let status = store::effective_status(&record);
            RunView {
                record,
                status,
                board,
            }
        })
        .collect()
}

impl WorkflowPanel {
    /// Open on the only run there is (or the only one running), else on the list.
    pub fn open() -> Self {
        let runs = load();
        let running: Vec<usize> = runs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.status == "running")
            .map(|(i, _)| i)
            .collect();
        let (level, run) = match (runs.len(), running.as_slice()) {
            (1, _) => (Level::Phases, 0),
            (_, [one]) => (Level::Phases, *one),
            _ => (Level::Runs, 0),
        };
        Self {
            runs,
            level,
            run,
            phase: 0,
            agent: 0,
            scroll: 0,
            note: None,
            read_at: Instant::now(),
        }
    }

    /// Read the run files again, at most every [`REFRESH`].
    pub fn refresh(&mut self) {
        if self.read_at.elapsed() < REFRESH {
            return;
        }
        self.read_at = Instant::now();
        let id = self.current().map(|r| r.record.run_id.clone());
        self.runs = load();
        if let Some(id) = id
            && let Some(at) = self.runs.iter().position(|r| r.record.run_id == id)
        {
            self.run = at;
        }
        self.run = self.run.min(self.runs.len().saturating_sub(1));
    }

    fn current(&self) -> Option<&RunView> {
        self.runs.get(self.run)
    }

    fn phases(&self) -> Vec<String> {
        self.current()
            .map_or_else(Vec::new, |r| r.board.phase_list())
    }

    fn agents(&self) -> Vec<&AgentRow> {
        let phases = self.phases();
        match (self.current(), phases.get(self.phase)) {
            (Some(run), Some(phase)) => run.board.agents_of(phase),
            _ => Vec::new(),
        }
    }

    fn stop(&mut self) {
        let Some(run) = self.current() else {
            return;
        };
        if run.status != "running" {
            self.note = Some(format!("{} is not running", run.record.run_id));
            return;
        }
        let id = run.record.run_id.clone();
        self.note = Some(if live::stop(&id) {
            format!("stopping {id}")
        } else {
            match board::request_stop(&id) {
                Ok(()) => format!("asked process {} to stop {id}", run.record.pid),
                Err(err) => format!("could not stop {id}: {err:#}"),
            }
        });
    }

    fn step(&mut self, down: bool) {
        let len = match self.level {
            Level::Runs => self.runs.len(),
            Level::Phases => self.phases().len(),
            Level::Agents => self.agents().len(),
            Level::Detail => {
                self.scroll = if down {
                    self.scroll + 1
                } else {
                    self.scroll.saturating_sub(1)
                };
                return;
            }
        };
        let at = match self.level {
            Level::Runs => &mut self.run,
            Level::Phases => &mut self.phase,
            _ => &mut self.agent,
        };
        if len == 0 {
            *at = 0;
        } else if down {
            *at = (*at + 1).min(len - 1);
        } else {
            *at = at.saturating_sub(1);
        }
    }

    fn descend(&mut self) {
        match self.level {
            Level::Runs if !self.runs.is_empty() => {
                self.level = Level::Phases;
                self.phase = 0;
            }
            Level::Phases if !self.agents().is_empty() => {
                self.level = Level::Agents;
                self.agent = 0;
            }
            Level::Agents if !self.agents().is_empty() => {
                self.level = Level::Detail;
                self.scroll = 0;
            }
            _ => {}
        }
    }

    /// Step back one level. True when the panel should close.
    fn ascend(&mut self) -> bool {
        match self.level {
            Level::Detail => self.level = Level::Agents,
            Level::Agents => self.level = Level::Phases,
            Level::Phases if self.runs.len() > 1 => self.level = Level::Runs,
            _ => return true,
        }
        false
    }

    /// Take a key. True when the panel closes.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        self.note = None;
        match key.code {
            KeyCode::Esc | KeyCode::Left => return self.ascend(),
            KeyCode::Char('q') if plain => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::Enter | KeyCode::Right => self.descend(),
            KeyCode::Char('x') if plain => self.stop(),
            _ => {}
        }
        false
    }

    fn hint(&self) -> String {
        let running = self.current().is_some_and(|r| r.status == "running");
        let stop = if running { " · x stop workflow" } else { "" };
        match self.level {
            Level::Runs => format!("↑↓ select · enter open{stop} · esc close"),
            Level::Phases => format!("↑↓ select · enter agents{stop} · esc back"),
            Level::Agents => format!("↑↓ select · enter detail{stop} · esc back"),
            Level::Detail => format!("↑↓ scroll{stop} · esc back"),
        }
    }
}

fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn took(run: &RunView) -> String {
    let end = run.record.ended_ms.unwrap_or_else(store::now_ms);
    clock(end.saturating_sub(run.record.started_ms))
}

fn agent_took(agent: &AgentRow) -> String {
    match (agent.started_ms, agent.ended_ms) {
        (Some(start), Some(end)) => clock(end.saturating_sub(start)),
        (Some(start), None) => clock(store::now_ms().saturating_sub(start)),
        _ => String::new(),
    }
}

fn mark(state: AgentState) -> (&'static str, Token) {
    match state {
        AgentState::Queued => ("◌", Token::Faint),
        AgentState::Running => ("●", Token::ToolRunning),
        AgentState::Done | AgentState::Cached => ("✔", Token::ToolDone),
        AgentState::Failed => ("✘", Token::ToolFailed),
        AgentState::Stopped => ("■", Token::Warning),
    }
}

fn state_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Queued => "Waiting for an agent slot",
        AgentState::Running => "Running",
        AgentState::Done => "Completed",
        AgentState::Cached => "Completed (from the resume journal)",
        AgentState::Failed => "Failed",
        AgentState::Stopped => "Stopped before it finished",
    }
}

fn pad(text: &str, width: usize) -> String {
    let have = text.chars().count();
    if have >= width {
        text.chars().take(width).collect()
    } else {
        format!("{text}{}", " ".repeat(width - have))
    }
}

/// A row with `left` at the start and `right` against the far edge.
fn spread(left: Vec<Span<'static>>, right: String, width: usize) -> Line<'static> {
    let used: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let gap = width.saturating_sub(used + right.chars().count()).max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(right, dim()));
    Line::from(spans)
}

fn run_lines(panel: &WorkflowPanel, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if panel.runs.is_empty() {
        out.push(Line::from(Span::styled("No workflow runs yet.", dim())));
        return out;
    }
    for (at, run) in panel.runs.iter().enumerate() {
        let picked = at == panel.run;
        let (done, all) = run.board.progress();
        let name = format!(
            "{} {}  {}",
            if picked { "❯" } else { " " },
            run.record.run_id,
            run.record.name
        );
        let style = if picked { accent().bold() } else { muted() };
        out.push(spread(
            vec![Span::styled(name, style)],
            format!("{} · {done}/{all} agents · {}", run.status, took(run)),
            width,
        ));
    }
    out
}

fn phase_column(panel: &WorkflowPanel, run: &RunView) -> Vec<Line<'static>> {
    run.board
        .phase_list()
        .iter()
        .enumerate()
        .map(|(at, phase)| {
            let (done, all) = run.board.phase_progress(phase);
            let picked = at == panel.phase;
            let lead = if picked { "❯" } else { " " };
            let count = if all > 0 {
                format!("{done}/{all}")
            } else {
                String::new()
            };
            let text = format!("{lead} {} {phase}", at + 1);
            let style = if !picked {
                muted()
            } else if panel.level == Level::Phases {
                accent().bold()
            } else {
                accent()
            };
            let mut line = pad(&text, 11);
            line.push_str(&format!("{count:>4}"));
            Line::from(Span::styled(line, style))
        })
        .collect()
}

fn agent_column(panel: &WorkflowPanel, width: usize) -> Vec<Line<'static>> {
    let agents = panel.agents();
    if agents.is_empty() {
        let text = if panel.phases().is_empty() {
            "No agents yet."
        } else {
            "Not started yet"
        };
        return vec![Line::from(Span::styled(text, dim()))];
    }
    agents
        .iter()
        .enumerate()
        .map(|(at, agent)| {
            let picked = at == panel.agent && panel.level == Level::Agents;
            let (glyph, token) = mark(agent.state);
            let lead = if picked { "❯" } else { " " };
            spread(
                vec![
                    Span::styled(lead.to_string(), accent()),
                    Span::styled(format!("{glyph} "), theme::style(token)),
                    Span::styled(
                        pad(&agent.label, 14),
                        if picked { accent().bold() } else { muted() },
                    ),
                    Span::styled(format!(" {}", agent.agent), dim()),
                ],
                agent_took(agent),
                width,
            )
        })
        .collect()
}

fn detail_lines(agent: &AgentRow, width: usize) -> Vec<Line<'static>> {
    let (glyph, token) = mark(agent.state);
    let mut out = vec![
        Line::from(vec![
            Span::styled(format!("{glyph} "), theme::style(token)),
            Span::raw(state_word(agent.state)),
            Span::styled(format!(" · {}", agent.agent), dim()),
        ]),
        Line::from(Span::styled(agent_took(agent), dim())),
        Line::default(),
    ];
    let mut section = |title: &str, text: &str, style: ratatui::style::Style| {
        out.push(Line::from(Span::styled(title.to_string(), accent().bold())));
        for line in text.lines() {
            for chunk in line.chars().collect::<Vec<_>>().chunks(width.max(8)) {
                out.push(Line::from(Span::styled(
                    format!("  {}", chunk.iter().collect::<String>()),
                    style,
                )));
            }
        }
        out.push(Line::default());
    };
    section("Prompt", &agent.prompt, muted());
    if let Some(error) = &agent.error {
        section("Error", error, theme::style(Token::Error));
    }
    match &agent.outcome {
        Some(outcome) => section("Outcome", outcome, muted()),
        None if !agent.state.finished() => {
            section("Outcome", "Not available yet (agent still running).", dim());
        }
        None => {}
    }
    out
}

/// Draw the panel over `area`.
pub fn draw(frame: &mut Frame, panel: &WorkflowPanel, area: Rect) {
    if area.width < 30 || area.height < 8 {
        return;
    }
    frame.render_widget(Clear, area);
    let width = area.width as usize;
    let mut top: Vec<Line<'static>> = Vec::new();
    let run = panel.current();
    if panel.level == Level::Runs || run.is_none() {
        top.push(Line::from(Span::styled("Workflows", accent().bold())));
    } else if let Some(run) = run {
        let (done, all) = run.board.progress();
        top.push(Line::from(Span::styled(
            run.record.name.clone(),
            accent().bold(),
        )));
        top.push(spread(
            vec![Span::styled(
                crate::workflow::command::one_line(&run.record.description, width / 2),
                muted(),
            )],
            format!("{done}/{all} agents · {}", took(run)),
            width,
        ));
    }
    let top_rows = top.len() as u16 + 1;
    let hint_row = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..area
    };
    let body = Rect {
        y: area.y + top_rows,
        height: area.height.saturating_sub(top_rows + 1),
        ..area
    };
    frame.render_widget(
        Paragraph::new(top),
        Rect {
            height: top_rows,
            ..area
        },
    );

    let titled = |title: String| {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(dim())
            .title(Line::from(Span::styled(format!(" {title} "), accent())))
    };
    match (panel.level, run) {
        (Level::Runs, _) | (_, None) => {
            let block = titled("Runs".to_string());
            let inner = block.inner(body);
            frame.render_widget(block, body);
            frame.render_widget(
                Paragraph::new(run_lines(panel, inner.width as usize)),
                inner,
            );
        }
        (Level::Detail, Some(_)) => {
            let agents = panel.agents();
            if let Some(agent) = agents.get(panel.agent) {
                let block = titled(agent.label.clone());
                let inner = block.inner(body);
                frame.render_widget(block, body);
                let lines = detail_lines(agent, inner.width as usize);
                let skip = panel.scroll.min(lines.len().saturating_sub(1));
                frame.render_widget(
                    Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>()),
                    inner,
                );
            }
        }
        (_, Some(run)) => {
            let left_w = 18.min(body.width / 3);
            let left = Rect {
                width: left_w,
                ..body
            };
            let right = Rect {
                x: body.x + left_w,
                width: body.width - left_w,
                ..body
            };
            let phases = run.board.phase_list();
            let name = phases.get(panel.phase).cloned().unwrap_or_default();
            let count = panel.agents().len();
            let lblock = titled("Phases".to_string());
            let linner = lblock.inner(left);
            frame.render_widget(lblock, left);
            frame.render_widget(Paragraph::new(phase_column(panel, run)), linner);
            let rblock = titled(format!(
                "{name} · {count} agent{}",
                if count == 1 { "" } else { "s" }
            ));
            let rinner = rblock.inner(right);
            frame.render_widget(rblock, right);
            frame.render_widget(
                Paragraph::new(agent_column(panel, rinner.width as usize)),
                rinner,
            );
        }
    }
    let hint = panel.note.clone().unwrap_or_else(|| panel.hint());
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, dim()))),
        hint_row,
    );
}

/// What the approval prompt shows for a `Workflow` call: the heading and the
/// lines under it. The phases come from the script's own `meta`, read without
/// running anything.
pub fn approval(args: &Value) -> (&'static str, Vec<String>) {
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let cwd = std::env::current_dir().unwrap_or_default();
    let source = if let Some(path) = text("scriptPath") {
        let full = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            cwd.join(path)
        };
        std::fs::read_to_string(full).ok()
    } else if let Some(name) = text("name") {
        store::find_named(&cwd, dirs::home_dir().as_deref(), name).and_then(|n| n.read().ok())
    } else {
        text("script").map(str::to_string)
    };
    let mut out = Vec::new();
    match source
        .as_deref()
        .map(|s| (s.lines().count(), script::parse(s)))
    {
        Some((lines, Ok(parsed))) => {
            out.push(parsed.meta.description.clone());
            out.push(format!("(script of {lines} lines)"));
            if !parsed.meta.phases.is_empty() {
                out.push(String::new());
                out.push(
                    "This dynamic workflow will spin up multiple subagents across the following phases:"
                        .to_string(),
                );
                for (at, phase) in parsed.meta.phases.iter().enumerate() {
                    match &phase.detail {
                        Some(detail) => {
                            out.push(format!("  {}. {}: {detail}", at + 1, phase.title))
                        }
                        None => out.push(format!("  {}. {}", at + 1, phase.title)),
                    }
                }
            }
        }
        Some((_, Err(why))) => out.push(format!("The script does not parse: {why}")),
        None => out.push(
            text("name")
                .map(|n| format!("The saved workflow \"{n}\""))
                .unwrap_or_else(|| "A workflow script".to_string()),
        ),
    }
    out.push(String::new());
    out.push(
        "Dynamic workflows can use a lot of tokens quickly by running many subagents in parallel. Stop a running workflow at any time with /workflows."
            .to_string(),
    );
    ("Run a dynamic workflow?", out)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;
    use crate::workflow::store::Status;

    fn row(i: u32, label: &str, phase: &str, state: AgentState) -> AgentRow {
        AgentRow {
            i,
            label: label.into(),
            phase: Some(phase.into()),
            state,
            agent: "worker".into(),
            started_ms: Some(store::now_ms()),
            ended_ms: state.finished().then(store::now_ms),
            prompt: "quick scan task".into(),
            outcome: state.finished().then(|| "ok".to_string()),
            error: None,
        }
    }

    fn panel() -> WorkflowPanel {
        let record = RunRecord {
            run_id: "wf_00000001-aaa".into(),
            name: "demo-flow".into(),
            description: "Three agents in two phases".into(),
            status: Status::Running,
            started_ms: store::now_ms(),
            ended_ms: None,
            cwd: ".".into(),
            source: "inline".into(),
            args: Value::Null,
            agents: 2,
            cached: 0,
            phases: vec!["Scan".into(), "Fix".into()],
            result: None,
            error: None,
            failures: vec![],
            pid: std::process::id(),
            task_id: None,
        };
        let board = Board {
            phases: vec!["Scan".into(), "Fix".into()],
            agents: vec![
                row(1, "scan:quick", "Scan", AgentState::Done),
                row(2, "scan:slow", "Scan", AgentState::Running),
            ],
        };
        WorkflowPanel {
            runs: vec![RunView {
                record,
                status: "running",
                board,
            }],
            level: Level::Phases,
            run: 0,
            phase: 0,
            agent: 0,
            scroll: 0,
            note: None,
            read_at: Instant::now(),
        }
    }

    fn screen(panel: &WorkflowPanel) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal
            .draw(|frame| draw(frame, panel, frame.area()))
            .unwrap();
        let buf: &Buffer = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn phases_on_the_left_and_the_selected_phases_agents_on_the_right() {
        let shown = screen(&panel());
        assert!(shown.contains("demo-flow"), "{shown}");
        assert!(shown.contains("1/2 agents"), "{shown}");
        assert!(shown.contains("Phases"), "{shown}");
        assert!(shown.contains("❯ 1 Scan"), "{shown}");
        assert!(shown.contains("Scan · 2 agents"), "{shown}");
        assert!(shown.contains("✔ scan:quick"), "{shown}");
        assert!(shown.contains("● scan:slow"), "{shown}");
        assert!(shown.contains("x stop workflow"), "{shown}");
        assert!(shown.contains("2 Fix"), "{shown}");
    }

    #[test]
    fn enter_goes_down_a_level_and_escape_comes_back_then_closes() {
        let mut panel = panel();
        assert!(!panel.handle_key(key(KeyCode::Enter)));
        assert_eq!(panel.level, Level::Agents);
        panel.handle_key(key(KeyCode::Down));
        assert_eq!(panel.agent, 1);
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.level, Level::Detail);
        let shown = screen(&panel);
        assert!(shown.contains("scan:slow"), "{shown}");
        assert!(shown.contains("Prompt"), "{shown}");
        assert!(shown.contains("Not available yet"), "{shown}");
        assert!(!panel.handle_key(key(KeyCode::Esc)));
        assert!(!panel.handle_key(key(KeyCode::Esc)));
        assert_eq!(panel.level, Level::Phases);
        assert!(panel.handle_key(key(KeyCode::Esc)), "one run: Esc closes");
    }

    #[test]
    fn a_phase_with_no_agents_does_not_open() {
        let mut panel = panel();
        panel.handle_key(key(KeyCode::Down));
        let shown = screen(&panel);
        assert!(shown.contains("Not started yet"), "{shown}");
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.level, Level::Phases);
    }

    #[test]
    fn x_asks_a_run_of_another_process_to_stop_with_a_file() {
        let mut panel = panel();
        let id = panel.runs[0].record.run_id.clone();
        store::create(&panel.runs[0].record, "x").unwrap();
        panel.handle_key(key(KeyCode::Char('x')));
        assert!(
            panel
                .note
                .as_deref()
                .unwrap_or("")
                .contains("asked process")
        );
        assert!(board::stop_requested(&id));
    }

    #[test]
    fn the_approval_lists_the_phases_of_a_script() {
        let script = "export const meta = {name: 'w', description: 'Does things', phases: [{title: 'A', detail: 'first'}, {title: 'B'}]}\nreturn 1\n";
        let (heading, lines) = approval(&serde_json::json!({"script": script}));
        assert_eq!(heading, "Run a dynamic workflow?");
        assert_eq!(lines[0], "Does things");
        assert!(lines.iter().any(|l| l == "  1. A: first"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "  2. B"), "{lines:?}");
        let (_, named) = approval(&serde_json::json!({"name": "deep-research"}));
        assert!(named.iter().any(|l| l.contains("Scope")), "{named:?}");
    }
}
