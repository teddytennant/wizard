// OWNER: session (scrollable transcript, user/assistant blocks, markdown, thought lines, completion line)
//! The session screen: transcript scrollbox, prompt box and hint row, optional sidebar.
//!
//! The transcript is rendered into [`Block`]s per message (cached while the message is
//! unchanged), laid out with opencode's margin rules, and only the rows in the viewport are
//! painted. Rows are expressed relative to the main column, whose left edge is column 2.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Duration;

use agent_core::transcript::{Message, Part, Role};
use agent_core::{NoticeLevel, StopReason, ToolCall, ToolStatus};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::markdown::{MarkdownOptions, StreamingMarkdown};
use tuikit::paint::{fill, put_line};
use tuikit::scroll::Scrollbar;
use tuikit::spinner::braille_frame;
use tuikit::width::{wrap_mode, WrapMode};
use tuikit::Theme;

use super::{footer, prompt, sidebar, tools};
use crate::app::{AgentKind, App, MsgMeta, ViewFlags};

/// Vertical space a block asks for above itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gap {
    /// No margin (the first user message).
    Zero,
    One,
    /// Inline tool rows: one blank row only after something taller than a row, or when
    /// `separate`.
    Inline {
        separate: bool,
    },
}

/// The rows of a block. A long streaming answer keeps its finished rows in shared chunks that
/// are never rewritten, so a frame copies pointers to them and re-renders only the live tail;
/// copying every row of a megabyte of text on every frame was what made streaming cost grow
/// with the size of the message.
#[derive(Clone, Debug, Default)]
pub struct Lines {
    chunks: Vec<Rc<Vec<Line<'static>>>>,
    head_len: usize,
    tail: Vec<Line<'static>>,
}

impl Lines {
    pub fn len(&self) -> usize {
        self.head_len + self.tail.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &Line<'static>> {
        self.iter_from(0)
    }

    /// Rows from `start` on. Whole chunks before it are skipped without being walked, so
    /// painting the end of a very long answer does not visit every row above it.
    pub fn iter_from(&self, start: usize) -> impl Iterator<Item = &Line<'static>> {
        let mut skip = start;
        let mut first = 0;
        while first < self.chunks.len() && skip >= self.chunks[first].len() {
            skip -= self.chunks[first].len();
            first += 1;
        }
        // past every chunk, what is left to skip comes off the tail
        let (head_skip, tail_skip) = if first == self.chunks.len() {
            (0, skip)
        } else {
            (skip, 0)
        };
        self.chunks[first..]
            .iter()
            .flat_map(|c| c.iter())
            .skip(head_skip)
            .chain(self.tail.iter().skip(tail_skip))
    }
}

impl From<Vec<Line<'static>>> for Lines {
    fn from(tail: Vec<Line<'static>>) -> Lines {
        Lines {
            chunks: Vec::new(),
            head_len: 0,
            tail,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub lines: Lines,
    pub gap: Gap,
    /// opencode's `alwaysSeparate`: the next inline row gets a blank above it.
    pub always_sep: bool,
    /// Id handed to the click handler (tool id, or `think:<msg>:<part>`).
    pub click: Option<String>,
}

impl Block {
    pub fn sep(lines: Vec<Line<'static>>) -> Block {
        Block {
            lines: lines.into(),
            gap: Gap::One,
            always_sep: true,
            click: None,
        }
    }
    pub fn inline(lines: Vec<Line<'static>>, separate: bool) -> Block {
        Block {
            lines: lines.into(),
            gap: Gap::Inline { separate },
            always_sep: separate,
            click: None,
        }
    }
}

/// Everything a renderer needs that is not the message itself.
pub struct RenderCx<'a> {
    pub theme: &'a Theme,
    /// Main column width: screen width minus sidebar minus the 2 + 2 padding.
    pub width: u16,
    pub flags: &'a ViewFlags,
    pub anim: Duration,
    pub cwd: &'a str,
    pub expanded: &'a HashSet<String>,
}

pub fn panel_lines(
    theme: &Theme,
    bar: Color,
    bar_bg: Option<Color>,
    body: Vec<Vec<Span<'static>>>,
) -> Vec<Line<'static>> {
    let panel = Style::new().bg(theme.background_panel);
    let mut bar_style = Style::new().fg(bar);
    if let Some(b) = bar_bg {
        bar_style = bar_style.bg(b);
    }
    let mk = |content: Vec<Span<'static>>| {
        let mut spans = vec![Span::styled("┃", bar_style), Span::raw("  ")];
        spans.extend(content);
        Line::from(spans).style(panel)
    };
    let mut lines = vec![mk(vec![])];
    lines.extend(body.into_iter().map(mk));
    lines.push(mk(vec![]));
    lines
}

// ---- per-message cache -----------------------------------------------------------------

struct MsgCache {
    sig: u64,
    key: u64,
    blocks: Rc<Vec<Block>>,
    md: Vec<MdSlot>,
}

/// A mouse selection in screen cells: where the drag started and where it is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (u16, u16),
    pub head: (u16, u16),
}

impl Selection {
    /// `(start, end)` in reading order; `end` is exclusive in its row.
    fn ordered(self) -> ((u16, u16), (u16, u16)) {
        let a = (self.anchor.1, self.anchor.0);
        let h = (self.head.1, self.head.0);
        let (s, e) = if a <= h { (a, h) } else { (h, a) };
        ((s.1, s.0), (e.1, e.0))
    }
}

/// Transcript render state owned by the app.
#[derive(Default)]
pub struct SessionView {
    cache: Vec<MsgCache>,
    /// Tool ids and `think:` ids the user expanded by click.
    pub expanded: HashSet<String>,
    /// `(first row, last row, id)` of clickable blocks as last drawn, in screen rows.
    pub hits: Vec<(u16, u16, String)>,
    /// Rect of the transcript as last drawn.
    pub area: Rect,
    /// Screen row of each message's first row as last drawn, for the timeline.
    pub row_of_msg: HashMap<usize, usize>,
    /// Drag selection over the transcript text; copied on release.
    pub sel: Option<Selection>,
    /// Where the left button went down, until it comes up.
    pub press: Option<(u16, u16)>,
    /// The press has moved: a selection, not a click.
    pub dragged: bool,
    /// Rows from the top of the scrollbar thumb to where it was grabbed, in half rows.
    pub sb_grab: Option<usize>,
    /// The next draw copies the selection (it needs the painted cells).
    pub copy_pending: bool,
    /// How many messages have been laid out since the app started; for tests of the cache.
    pub builds: usize,
}

impl SessionView {
    pub fn toggle(&mut self, id: &str) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_string());
        }
    }
    pub fn click(&mut self, y: u16) -> bool {
        let hit = self
            .hits
            .iter()
            .find(|(a, b, _)| y >= *a && y <= *b)
            .map(|h| h.2.clone());
        match hit {
            Some(id) => {
                self.toggle(&id);
                true
            }
            None => false,
        }
    }
}

fn hash<T: Hash>(v: &T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn msg_sig(m: &Message) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        matches!(m.role, Role::User),
        matches!(m.role, Role::Assistant),
        m.parts.len(),
        m.took.is_some(),
        m.stop.map(|s| s as u8),
    )
        .hash(&mut h);
    if let Role::Notice(l) = m.role {
        (l as u8).hash(&mut h);
    }
    m.model.hash(&mut h);
    for p in &m.parts {
        match p {
            Part::Text(s) => (1u8, s.len()).hash(&mut h),
            Part::Thought { text, took, .. } => {
                (2u8, text.len(), took.map(|d| d.as_millis() / 100)).hash(&mut h)
            }
            Part::Tool(c) => (
                3u8,
                &c.id,
                c.status as u8,
                c.output.as_ref().map(String::len),
                c.diff.is_some(),
                c.title.len(),
            )
                .hash(&mut h),
        }
    }
    h.finish()
}

fn volatile(m: &Message, is_last: bool, busy: bool) -> bool {
    let live_tool = m.parts.iter().any(|p| matches!(p, Part::Tool(c) if matches!(c.status, ToolStatus::Running | ToolStatus::Pending)));
    let open_thought = m
        .parts
        .iter()
        .any(|p| matches!(p, Part::Thought { took: None, .. }));
    (is_last && busy && m.role == Role::Assistant) || live_tool || open_thought
}

// ---- formatting helpers ----------------------------------------------------------------

/// opencode's `Locale.duration`.
pub fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis() as u64;
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    } else if ms < 86_400_000 {
        format!("{}h {}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    } else {
        format!("{}d {}h", ms / 86_400_000, (ms % 86_400_000) / 3_600_000)
    }
}

/// `**Title**` first line of a reasoning block, and the rest.
fn reasoning_summary(content: &str) -> (Option<String>, String) {
    let first = content.lines().next().unwrap_or("").trim();
    if first.len() > 4
        && first.starts_with("**")
        && first.ends_with("**")
        && !first[2..first.len() - 2].contains("**")
    {
        let title = first[2..first.len() - 2].trim().to_string();
        let rest = content
            .split_once('\n')
            .map_or("", |(_, r)| r)
            .trim()
            .to_string();
        return (Some(title), rest);
    }
    (None, content.trim().to_string())
}

/// The binary differs from opencode's source in a markdown color (spec 0.1): block quote text
/// is plain `text` with only the bar muted. (Ordered list numbers in the bullet color are
/// handled by the renderer's `opencode_blocks`.) Body text is the syntax default (`text`) and a
/// rule is `textMuted`, whatever `markdownText` and `markdownHorizontalRule` say: only the
/// default theme has them equal (fidelity round 1, finding 10).
fn md_theme(t: &Theme) -> Theme {
    let mut m = t.clone();
    m.markdown_block_quote = t.text;
    m.markdown_text = t.text;
    m.markdown_horizontal_rule = t.text_muted;
    m
}

fn mute_quote_bars(lines: &mut [Line<'static>], t: &Theme) {
    for l in lines {
        for sp in l.spans.iter_mut() {
            if sp.content.starts_with('│') {
                sp.style.fg = Some(t.text_muted);
            } else if !sp.content.trim().is_empty() {
                break;
            }
        }
    }
}

/// One markdown part of a message: the streaming renderer, plus the finished rows after they
/// were indented and had their quote bars muted, so a frame does that work for new rows only.
#[derive(Default)]
struct MdSlot {
    md: StreamingMarkdown,
    chunks: Vec<Rc<Vec<Line<'static>>>>,
    /// Finished rows already in `chunks`.
    done: usize,
    epoch: u64,
}

fn md_options(conceal: bool, t: &Theme) -> MarkdownOptions {
    MarkdownOptions {
        conceal,
        grid_tables: true,
        soft_break_newline: true,
        opencode_inline: true,
        punct_wrap: true,
        opencode_blocks: true,
        // a quote inside a quote is literal text: `>` like an operator, the words in the quote
        // colour that `md_theme` takes away from the outer one
        code_word_wrap: true,
        literal_quote: Some((t.syntax_operator, t.markdown_block_quote)),
        ..Default::default()
    }
}

/// An answer's rows, each indented by `pad` columns. Work done per frame is the rows that
/// became final since the last one plus the live tail, not the whole message.
fn md_lines_indented(
    slot: &mut MdSlot,
    src: &str,
    width: u16,
    t: &Theme,
    conceal: bool,
    pad: usize,
) -> Lines {
    let mt = md_theme(t);
    let split = slot
        .md
        .update_split(src, width, &mt, &md_options(conceal, t));
    if split.epoch != slot.epoch {
        slot.chunks.clear();
        slot.done = 0;
        slot.epoch = split.epoch;
    }
    let style = |l: &Line<'static>| {
        let mut one = [l.clone()];
        mute_quote_bars(&mut one, t);
        let [l] = one;
        indent(pad, l)
    };
    if split.stable.len() > slot.done {
        let fresh: Vec<Line<'static>> = split.stable[slot.done..].iter().map(style).collect();
        slot.done = split.stable.len();
        slot.chunks.push(Rc::new(fresh));
    }
    Lines {
        chunks: slot.chunks.clone(),
        head_len: slot.done,
        tail: split.tail.iter().map(style).collect(),
    }
}

fn md_lines(
    md: &mut StreamingMarkdown,
    src: &str,
    width: u16,
    t: &Theme,
    conceal: bool,
) -> Vec<Line<'static>> {
    let opts = md_options(conceal, t);
    let mt = md_theme(t);
    let mut lines = md.update(src, width, &mt, &opts).to_vec();
    mute_quote_bars(&mut lines, t);
    lines
}

fn indent(n: usize, mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(" ".repeat(n)));
    line
}

fn fade(line: &mut Line<'static>, theme: &Theme, alpha: f32) {
    for sp in &mut line.spans {
        let fg = sp.style.fg.unwrap_or(theme.text);
        sp.style.fg = Some(Theme::alpha_over(fg, theme.background, alpha));
    }
}

// ---- message rendering -----------------------------------------------------------------

struct Env<'a> {
    cx: &'a RenderCx<'a>,
    agent: AgentKind,
    model: String,
    is_last: bool,
    first: bool,
    queued: bool,
    idx: usize,
    meta: MsgMeta,
    /// The assistant message answers a `/compact`, so its completion line says Compaction.
    compaction_reply: bool,
    /// An error box follows this assistant message; opencode draws the completion line after
    /// it, so the message leaves its own to the last error notice of the run.
    defer_completion: bool,
    /// On an error notice that ends a run after an assistant message: that message's line.
    trailing: Option<Finish>,
}

/// What the completion line needs from the assistant message it describes.
#[derive(Clone, Debug, PartialEq)]
struct Finish {
    took: Option<Duration>,
    stop: Option<StopReason>,
    agent: AgentKind,
    model: String,
    compaction_reply: bool,
}

/// `──── Compaction ────` across the column.
fn compaction_divider(env: &Env) -> Block {
    let t = env.cx.theme;
    let title = " Compaction ";
    let w = env.cx.width as usize;
    let rest = w.saturating_sub(title.len());
    let (l, r) = (rest / 2, rest - rest / 2);
    let line = Line::from(Span::styled(
        format!("{}{title}{}", "─".repeat(l), "─".repeat(r)),
        Style::new().fg(t.border_active),
    ));
    Block::sep(vec![line])
}

/// `File` / `Directory` chips, wrapped to `width`, one `Vec` of spans per row.
fn chip_rows(env: &Env, width: usize) -> Vec<Vec<Span<'static>>> {
    let t = env.cx.theme;
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0usize;
    for c in &env.meta.files {
        let kind = if c.directory { " Directory " } else { " File " };
        let name = format!(" {} ", c.name);
        let w = kind.len() + tuikit::width::display_width(&name);
        if used > 0 && used + 1 + w > width {
            rows.push(Vec::new());
            used = 0;
        }
        let row = rows.last_mut().expect("one row exists");
        if used > 0 {
            row.push(Span::raw(" "));
            used += 1;
        }
        row.push(Span::styled(
            kind,
            Style::new().fg(t.background).bg(t.secondary),
        ));
        row.push(Span::styled(
            name,
            Style::new().fg(t.text_muted).bg(t.background_element),
        ));
        used += w;
    }
    rows
}

fn user_block(env: &Env, m: &Message) -> Vec<Block> {
    let t = env.cx.theme;
    if env.meta.compaction {
        return vec![compaction_divider(env)];
    }
    let text: String = m
        .parts
        .iter()
        .filter_map(|p| {
            if let Part::Text(s) = p {
                Some(s.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.trim().is_empty() {
        return Vec::new();
    }
    // the panel ends 3 columns inside the main column and the text starts 2 inside the panel,
    // with no padding on the right: a 113 character line fits at 120 columns
    let w = (env.cx.width as usize).saturating_sub(3);
    let mut body: Vec<Vec<Span<'static>>> = wrap_mode(&text, w, WrapMode::WordPunct)
        .into_iter()
        .map(|l| vec![Span::styled(l, Style::new().fg(t.text))])
        .collect();
    let at = env
        .meta
        .at
        .filter(|_| env.cx.flags.timestamps && !env.queued);
    let metadata = env.queued || at.is_some();
    if !env.meta.files.is_empty() {
        body.push(Vec::new());
        body.extend(chip_rows(env, w));
        if metadata {
            body.push(Vec::new());
        }
    }
    if env.queued {
        let c = env.agent.color(t);
        body.push(vec![Span::styled(
            " QUEUED ",
            Style::new()
                .fg(t.selected_foreground(Some(c)))
                .bg(c)
                .add_modifier(Modifier::BOLD),
        )]);
    } else if let Some(at) = at {
        let s = crate::timefmt::today_time_or_date_time(at, std::time::SystemTime::now());
        body.push(vec![Span::styled(s, Style::new().fg(t.text_muted))]);
    }
    let bar = env.agent.color(t);
    let lines = panel_lines(t, bar, Some(t.background), body);
    let mut b = Block::sep(lines);
    if env.first {
        b.gap = Gap::Zero;
    }
    vec![b]
}

fn text_block(env: &Env, slot: &mut MdSlot, text: &str) -> Option<Block> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let w = env.cx.width.saturating_sub(3);
    let lines = md_lines_indented(slot, t, w, env.cx.theme, env.cx.flags.conceal, 3);
    let mut b = Block::sep(Vec::new());
    b.lines = lines;
    Some(b)
}

fn thought_block(
    env: &Env,
    md: &mut StreamingMarkdown,
    part: usize,
    text: &str,
    took: Option<Duration>,
) -> Option<Block> {
    let t = env.cx.theme;
    let content = text.replace("[REDACTED]", "");
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    let id = format!("think:{}:{part}", env.idx);
    let minimal = !env.cx.flags.thinking_shown;
    let expanded = env.cx.expanded.contains(&id);
    let open = !minimal || expanded;
    let (title, body) = reasoning_summary(content);
    let warn = Theme::alpha_over(t.warning, t.background, t.thinking_opacity);
    let header_fg = if open { warn } else { t.warning };
    let mut lines: Vec<Line<'static>> = Vec::new();
    match took {
        None => {
            let label = title
                .as_ref()
                .map_or("Thinking".to_string(), |s| format!("Thinking: {s}"));
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    braille_frame(env.cx.anim).to_string(),
                    Style::new().fg(header_fg),
                ),
                Span::raw(" "),
                Span::styled(label, Style::new().fg(header_fg)),
            ]));
        }
        Some(d) => {
            let dur = (!d.is_zero()).then(|| fmt_duration(d));
            let detail: Vec<String> = title.iter().cloned().chain(dur).collect();
            let prefix = if minimal {
                if open {
                    "- "
                } else {
                    "+ "
                }
            } else {
                ""
            };
            let tail = if detail.is_empty() {
                String::new()
            } else {
                format!(": {}", detail.join(" · "))
            };
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(format!("{prefix}Thought{tail}"), Style::new().fg(header_fg)),
            ]));
        }
    }
    if open && !body.is_empty() {
        lines.push(Line::default());
        let pad = if minimal { 5 } else { 3 };
        let w = env.cx.width.saturating_sub(pad as u16);
        for mut l in md_lines(md, &body, w, t, env.cx.flags.conceal) {
            fade(&mut l, t, t.thinking_opacity);
            lines.push(indent(pad, l));
        }
    }
    let mut b = Block::sep(lines);
    if minimal {
        b.click = Some(id);
    }
    Some(b)
}

fn completion_block(theme: &Theme, f: &Finish) -> Block {
    let t = theme;
    let interrupted = f.stop == Some(StopReason::Cancelled);
    let c = if interrupted {
        t.text_muted
    } else {
        f.agent.color(t)
    };
    let mut spans = vec![
        Span::raw("   "),
        Span::styled("▣ ", Style::new().fg(c)),
        Span::raw(" "),
        Span::styled(
            if f.compaction_reply {
                "Compaction"
            } else {
                f.agent.title()
            }
            .to_string(),
            Style::new().fg(t.text),
        ),
        Span::styled(format!(" · {}", f.model), Style::new().fg(t.text_muted)),
    ];
    let final_turn = matches!(
        f.stop,
        Some(StopReason::EndTurn) | Some(StopReason::MaxTurns)
    );
    if let (Some(d), true) = (f.took, final_turn) {
        if !d.is_zero() {
            spans.push(Span::styled(
                format!(" · {}", fmt_duration(d)),
                Style::new().fg(t.text_muted),
            ));
        }
    }
    if interrupted {
        spans.push(Span::styled(
            " · interrupted",
            Style::new().fg(t.text_muted),
        ));
    }
    Block::sep(vec![Line::from(spans)])
}

fn notice_blocks(env: &Env, md: &mut MdSlot, level: NoticeLevel, text: &str) -> Vec<Block> {
    let t = env.cx.theme;
    match level {
        NoticeLevel::Error => {
            let w = (env.cx.width as usize).saturating_sub(5);
            let body = wrap_mode(text.trim(), w, WrapMode::WordPunct)
                .into_iter()
                .map(|l| vec![Span::styled(l, Style::new().fg(t.text_muted))])
                .collect();
            let lines = panel_lines(t, t.error, Some(t.background), body);
            vec![Block::sep(lines)]
        }
        _ => text_block(env, md, text).into_iter().collect(),
    }
}

fn message_blocks(
    env: &Env,
    m: &Message,
    md: &mut Vec<MdSlot>,
    all_calls: &[&ToolCall],
) -> Vec<Block> {
    let mut out = Vec::new();
    if md.len() < m.parts.len() + 1 {
        md.resize_with(m.parts.len() + 1, MdSlot::default);
    }
    match m.role {
        Role::User => out.extend(user_block(env, m)),
        Role::Notice(level) => {
            if let Some(Part::Text(s)) = m.parts.first() {
                out.extend(notice_blocks(env, &mut md[0], level, s));
            }
            if let Some(f) = &env.trailing {
                out.push(completion_block(env.cx.theme, f));
            }
        }
        Role::Assistant => {
            // opencode adds `ctrl+x down view subagents` after a step that called `task`; a run
            // of consecutive tool parts is the closest thing to a step here.
            let mut subagent_step = false;
            for (i, p) in m.parts.iter().enumerate() {
                if subagent_step && !matches!(p, Part::Tool(_)) {
                    out.push(tools::subagent_hint(env.cx));
                    subagent_step = false;
                }
                match p {
                    Part::Text(s) => out.extend(text_block(env, &mut md[i], s)),
                    Part::Thought { text, took, .. } => {
                        out.extend(thought_block(env, &mut md[i].md, i, text, *took))
                    }
                    Part::Tool(c) => {
                        if c.parent_id.is_some() {
                            continue;
                        }
                        let kids: Vec<&ToolCall> = all_calls
                            .iter()
                            .copied()
                            .filter(|k| k.parent_id.as_deref() == Some(c.id.as_str()))
                            .collect();
                        subagent_step |= tools::classify(c) == tools::Tool::Task;
                        out.extend(tools::render(c, &kids, env.cx));
                    }
                }
            }
            if subagent_step {
                out.push(tools::subagent_hint(env.cx));
            }
            let has_content = !m.parts.is_empty();
            let interrupted = m.stop == Some(StopReason::Cancelled);
            if !env.defer_completion
                && (env.is_last || m.took.is_some() || interrupted)
                && (has_content || interrupted)
            {
                out.push(completion_block(
                    env.cx.theme,
                    &Finish {
                        took: m.took,
                        stop: m.stop,
                        agent: env.agent,
                        model: env.model.clone(),
                        compaction_reply: env.compaction_reply,
                    },
                ));
            }
        }
    }
    out
}

// ---- layout ----------------------------------------------------------------------------

struct Item {
    msg: usize,
    blk: usize,
    gap: u16,
    /// Rows, not a `u16`: an expanded output can pass 65,535 and would wrap into the blocks below.
    height: usize,
}

fn layout(cache: &[MsgCache]) -> (Vec<Item>, usize) {
    let mut items = Vec::new();
    let mut total = 0usize;
    let mut prev: Option<(usize, bool)> = None; // (height, always_sep)
    for (mi, mc) in cache.iter().enumerate() {
        for (bi, b) in mc.blocks.iter().enumerate() {
            let gap = match (b.gap, prev) {
                (_, None) | (Gap::Zero, _) => 0,
                (Gap::One, _) => 1,
                (Gap::Inline { separate }, Some((h, sep))) => u16::from(separate || h > 1 || sep),
            };
            let height = b.lines.len();
            total += gap as usize + height;
            items.push(Item {
                msg: mi,
                blk: bi,
                gap,
                height,
            });
            prev = Some((height, b.always_sep));
        }
    }
    (items, total)
}

/// Rebuild stale caches. Returns nothing; the layout is computed from `view.cache` after.
fn refresh(app: &mut App, width: u16) {
    let theme = app.theme.clone();
    let flags = app.flags.clone();
    let anim = app.anim_elapsed();
    let cwd = app.cwd.clone();
    let busy = app.transcript.busy;
    let n = app.transcript.messages.len();
    let key = hash(&(
        width,
        theme.name.clone(),
        theme.mode as u8,
        flags.conceal,
        flags.timestamps,
        flags.thinking_shown,
        flags.tool_details,
        flags.generic_output,
    ));
    app.view.cache.truncate(n);
    let model_default = app.model_display().0;
    // Index of the first user message and of any open assistant turn, for QUEUED.
    let first_user = app
        .transcript
        .messages
        .iter()
        .position(|m| m.role == Role::User);
    let mut open_before = false;
    for i in 0..n {
        let m = &app.transcript.messages[i];
        let is_last = i + 1 == n;
        let sig = msg_sig(m);
        let vol = volatile(m, is_last, busy);
        let queued = m.role == Role::User && open_before && busy;
        if m.role == Role::Assistant && m.took.is_none() && !is_last {
            open_before = true;
        }
        if m.role == Role::Assistant && m.took.is_none() && is_last && busy {
            open_before = true;
        }
        let is_error = |j: usize| {
            matches!(
                app.transcript.messages.get(j).map(|x| x.role),
                Some(Role::Notice(NoticeLevel::Error))
            )
        };
        let defer_completion = m.role == Role::Assistant && is_error(i + 1);
        // an error notice that ends a run of them, right after a finished assistant message
        let trailing_from = (is_error(i) && !is_error(i + 1))
            .then(|| (0..i).rev().find(|j| !is_error(*j)))
            .flatten()
            .filter(|j| app.transcript.messages[*j].role == Role::Assistant);
        let trailing = trailing_from.and_then(|k| {
            let a = &app.transcript.messages[k];
            let interrupted = a.stop == Some(StopReason::Cancelled);
            let due = interrupted || a.took.is_some() && !a.parts.is_empty();
            due.then(|| Finish {
                took: a.took,
                stop: a.stop,
                agent: app.agent_for(k),
                model: if a.model.is_empty() {
                    model_default.clone()
                } else {
                    app.display_name(&a.model)
                },
                compaction_reply: (0..k)
                    .rev()
                    .find(|j| app.transcript.messages[*j].role == Role::User)
                    .is_some_and(|j| app.msg_meta.get(&j).is_some_and(|x| x.compaction)),
            })
        });
        // What the user expanded matters to the message that owns it, not to the rest: clicking
        // a tool row rebuilt every message of the session before.
        // (the positions of the parts that are open, so a message with none hashes the same
        // whether or not anything else in the session is)
        let opened: Vec<usize> = if app.view.expanded.is_empty() {
            Vec::new()
        } else {
            m.parts
                .iter()
                .enumerate()
                .filter(|(pi, p)| match p {
                    Part::Tool(c) => app.view.expanded.contains(&c.id),
                    Part::Thought { .. } => app.view.expanded.contains(&format!("think:{i}:{pi}")),
                    Part::Text(_) => false,
                })
                .map(|(pi, _)| pi)
                .collect()
        };
        let mkey = hash(&(
            key,
            opened,
            queued,
            defer_completion,
            trailing
                .as_ref()
                .map(|f| (f.took.is_some(), f.stop.map(|s| s as u8), f.model.clone())),
        ));
        let stale = match app.view.cache.get(i) {
            Some(c) => c.sig != sig || c.key != mkey || vol,
            None => true,
        };
        if !stale {
            continue;
        }
        let agent = app.agent_for(i);
        let model = if m.model.is_empty() {
            model_default.clone()
        } else {
            app.display_name(&m.model)
        };
        let all_calls: Vec<&ToolCall> = m
            .parts
            .iter()
            .filter_map(|p| if let Part::Tool(c) = p { Some(c) } else { None })
            .collect();
        let cx = RenderCx {
            theme: &theme,
            width,
            flags: &flags,
            anim,
            cwd: &cwd,
            expanded: &app.view.expanded,
        };
        let meta = app.msg_meta.get(&i).cloned().unwrap_or_default();
        let compaction_reply = m.role == Role::Assistant
            && (0..i)
                .rev()
                .find(|j| app.transcript.messages[*j].role == Role::User)
                .is_some_and(|j| app.msg_meta.get(&j).is_some_and(|x| x.compaction));
        let env = Env {
            cx: &cx,
            agent,
            model,
            is_last,
            first: first_user == Some(i),
            queued,
            idx: i,
            meta,
            compaction_reply,
            defer_completion,
            trailing,
        };
        let mut md = app
            .view
            .cache
            .get_mut(i)
            .map(|c| std::mem::take(&mut c.md))
            .unwrap_or_default();
        let blocks = message_blocks(&env, m, &mut md, &all_calls);
        app.view.builds += 1;
        let entry = MsgCache {
            sig,
            key: mkey,
            blocks: Rc::new(blocks),
            md,
        };
        if i < app.view.cache.len() {
            app.view.cache[i] = entry;
        } else {
            app.view.cache.push(entry);
        }
    }
}

// ---- drawing ---------------------------------------------------------------------------

/// Geometry shared by drawing and mouse handling.
pub struct Geometry {
    pub main_w: u16,
    pub sidebar: Option<Rect>,
    pub overlay: bool,
    pub col_x: u16,
    pub col_w: u16,
    pub transcript: Rect,
    pub prompt: Rect,
    pub hint: Rect,
}

pub fn geometry(app: &App) -> Geometry {
    let (w, h) = app.size;
    let sb = app.sidebar_state();
    let main_w = if sb.shown && !sb.overlay {
        w.saturating_sub(sidebar::WIDTH)
    } else {
        w
    };
    let col_x = 2u16.min(main_w);
    let col_w = main_w.saturating_sub(4);
    // A permission or question replaces the prompt and its hint row, and is wider than the
    // column: it runs from column 2 to the right edge of the main area.
    let ask = app.ask_height();
    let ph = match ask {
        Some(a) => a.min(h.saturating_sub(3)),
        None => prompt::box_height(app, col_w).min(h.saturating_sub(3)),
    };
    // bottom block = prompt + hint row, then 1 padding row; 1 gap row above the block
    let hint_h = if ask.is_some() {
        0
    } else {
        footer::hint_height(app, col_w)
            .min(h.saturating_sub(ph + 2))
            .max(1)
    };
    let prompt_y = h.saturating_sub(1 + hint_h + ph);
    let tr_h = prompt_y.saturating_sub(1);
    Geometry {
        main_w,
        sidebar: sb.shown.then(|| {
            if sb.overlay {
                Rect::new(
                    w.saturating_sub(sidebar::WIDTH),
                    0,
                    sidebar::WIDTH.min(w),
                    h,
                )
            } else {
                Rect::new(main_w, 0, w - main_w, h)
            }
        }),
        overlay: sb.overlay,
        col_x,
        col_w,
        transcript: Rect::new(col_x, 0, col_w, tr_h),
        prompt: Rect::new(
            col_x,
            prompt_y,
            if ask.is_some() {
                main_w.saturating_sub(col_x)
            } else {
                col_w
            },
            ph,
        ),
        hint: Rect::new(col_x, prompt_y + ph, col_w, hint_h),
    }
}

pub fn draw(buf: &mut Buffer, app: &mut App) -> Option<(u16, u16)> {
    let g = geometry(app);
    // a visible scrollbar takes its column and one of padding from the viewport
    let content_w = g
        .col_w
        .saturating_sub(if app.flags.scrollbar { 2 } else { 0 });
    refresh(app, content_w);
    let (items, blocks_total) = layout(&app.view.cache);
    let vp = g.transcript.height as usize;
    // One blank row of padding above the first block is part of the scrollable content, so
    // scrolling to the top shows it, as opencode's scrollbox padding does.
    let spacer = 1;
    app.scroll.set_extent(blocks_total + spacer, vp);
    app.view.area = g.transcript;
    app.view.hits.clear();
    app.view.row_of_msg.clear();

    let off = app.scroll.offset();
    let mut row = spacer; // content row of the next block's top, spacer included
    let tr = g.transcript;
    let body = Rect::new(tr.x, tr.y, content_w, tr.height);
    for it in &items {
        row += it.gap as usize;
        let top = row;
        row += it.height;
        app.view.row_of_msg.entry(it.msg).or_insert(top);
        if row <= off || top >= off + vp {
            continue;
        }
        let blk = &app.view.cache[it.msg].blocks[it.blk];
        let first_row = off.saturating_sub(top);
        for (li, line) in blk.lines.iter_from(first_row).enumerate() {
            let cr = top + first_row + li;
            if cr >= off + vp {
                break;
            }
            let y = tr.y + (cr - off) as u16;
            if let Some(bg) = line.style.bg {
                fill(buf, Rect::new(tr.x, y, body.width, 1), Style::new().bg(bg));
            }
            put_line(buf, tr.x, y, line, body);
        }
        if let (Some(id), true) = (&blk.click, vp > 0) {
            let a = tr.y + top.saturating_sub(off) as u16;
            let b = tr.y + ((row - 1).min(off + vp - 1) - off) as u16;
            app.view.hits.push((a, b, id.clone()));
        }
    }
    if app.flags.scrollbar {
        draw_scrollbar(buf, app, g.main_w, tr);
    }
    if let Some(sel) = app.view.sel {
        let spans = selected_spans(buf, tr, sel);
        if app.view.copy_pending {
            app.view.copy_pending = false;
            app.view.sel = None;
            let text = selected_text(buf, &spans);
            if !text.is_empty() {
                app.pending.push(crate::app::Deferred::Copy(text));
                app.toast(tuikit::theme::Variant::Info, "Copied to clipboard");
            }
        } else {
            let style = Style::new().fg(app.theme.background).bg(app.theme.text);
            for (y, x0, x1) in spans {
                tuikit::paint::set_style(buf, Rect::new(x0, y, x1 - x0, 1), style);
            }
        }
    }

    // Prompt, popup, hints; or the permission or question that is waiting for an answer.
    let cursor = if app.asks.is_empty() {
        let cursor = prompt::draw_box(buf, g.prompt, app);
        prompt::draw_popup(buf, g.prompt, app);
        footer::session_hints(buf, g.hint, app);
        cursor
    } else {
        app.draw_ask(buf, g.prompt)
    };
    if let Some(sb) = g.sidebar {
        sidebar::draw(buf, app, sb, g.overlay);
    }
    cursor
}

/// Cells of text inside the selection, one `(row, from, to)` per row. Padding on either side
/// of the text (the user bar, the indent) is not selectable, as in opencode.
fn selected_spans(buf: &Buffer, tr: Rect, sel: Selection) -> Vec<(u16, u16, u16)> {
    let ((sx, sy), (ex, ey)) = sel.ordered();
    let mut out = Vec::new();
    for y in sy.max(tr.y)..=ey.min(tr.bottom().saturating_sub(1)) {
        // extents of the text on this row
        let cell = |x: u16| buf[(x, y)].symbol();
        let mut ts = tr.x;
        while ts < tr.right() && matches!(cell(ts), " " | "┃" | "") {
            ts += 1;
        }
        let mut te = tr.right();
        while te > ts && cell(te - 1) == " " {
            te -= 1;
        }
        if ts >= te {
            // a blank row between two selected rows is a blank line in the copy
            if y > sy && y < ey {
                out.push((y, tr.x, tr.x));
            }
            continue;
        }
        let from = if y == sy { sx.max(ts) } else { ts };
        let to = if y == ey { ex.min(te) } else { te };
        if from < to {
            out.push((y, from, to));
        }
    }
    // blank rows only count between text
    while out.last().is_some_and(|s| s.1 == s.2) {
        out.pop();
    }
    while out.first().is_some_and(|s| s.1 == s.2) {
        out.remove(0);
    }
    out
}

fn selected_text(buf: &Buffer, spans: &[(u16, u16, u16)]) -> String {
    spans
        .iter()
        .map(|(y, x0, x1)| {
            (*x0..*x1)
                .map(|x| buf[(x, *y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn draw_scrollbar(buf: &mut Buffer, app: &App, main_w: u16, tr: Rect) {
    let t = &app.theme;
    let area = Rect::new(main_w.saturating_sub(3), tr.y, 1, tr.height);
    ratatui::widgets::Widget::render(
        Scrollbar {
            state: &app.scroll,
            track: t.background_element,
            thumb: t.border,
        },
        area,
        buf,
    );
}

/// Screen column of the scrollbar for a column layout `main_w` wide.
pub fn scrollbar_col(main_w: u16) -> u16 {
    main_w.saturating_sub(3)
}

impl SessionView {
    /// Mouse went down on the scrollbar at screen row `row`: grab the thumb, or page toward the
    /// click when it landed on the track.
    pub fn scrollbar_press(&mut self, scroll: &mut tuikit::scroll::ScrollState, row: u16) {
        let top = self.area.y;
        let r = row.saturating_sub(top) as usize;
        let rows = self.area.height as usize;
        match scroll.thumb_halves(rows) {
            Some((start, len)) if (r * 2) >= start && r * 2 < start + len => {
                self.sb_grab = Some(r * 2 - start);
            }
            Some((start, _)) => {
                let page = scroll.viewport().max(1) as isize;
                scroll.scroll_by(if r * 2 < start { -page } else { page });
            }
            None => {}
        }
    }

    /// Dragging the thumb: put its top where the pointer is, minus the grab offset.
    pub fn scrollbar_drag(&mut self, scroll: &mut tuikit::scroll::ScrollState, row: u16) {
        let Some(grab) = self.sb_grab else { return };
        let rows = self.area.height as usize;
        let Some((_, len)) = scroll.thumb_halves(rows) else {
            return;
        };
        let units = rows * 2;
        let span = units.saturating_sub(len).max(1);
        let r = row.saturating_sub(self.area.y) as usize;
        let start = (r * 2).saturating_sub(grab).min(span);
        let target = (start as f64 / span as f64 * scroll.max_offset() as f64).round() as isize;
        scroll.scroll_by(target - scroll.offset() as isize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_iterate_from_any_row_across_chunks_and_tail() {
        let row = |i: usize| Line::from(i.to_string());
        let chunk = |a: usize, b: usize| Rc::new((a..b).map(row).collect::<Vec<_>>());
        let lines = Lines {
            chunks: vec![chunk(0, 3), chunk(3, 4), chunk(4, 9)],
            head_len: 9,
            tail: (9..12).map(row).collect(),
        };
        assert_eq!(lines.len(), 12);
        let all: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(all.len(), 12);
        for n in 0..=14 {
            let got: Vec<String> = lines.iter_from(n).map(|l| l.to_string()).collect();
            assert_eq!(
                got,
                all.iter().skip(n).cloned().collect::<Vec<_>>(),
                "from {n}"
            );
        }
        let plain = Lines::from((0..4).map(row).collect::<Vec<_>>());
        assert_eq!(plain.iter_from(2).count(), 2);
    }

    #[test]
    fn durations_follow_locale() {
        assert_eq!(fmt_duration(Duration::from_millis(237)), "237ms");
        assert_eq!(fmt_duration(Duration::from_millis(6500)), "6.5s");
        assert_eq!(fmt_duration(Duration::from_secs(194)), "3m 14s");
        assert_eq!(fmt_duration(Duration::from_secs(7500)), "2h 5m");
    }

    #[test]
    fn reasoning_title_comes_from_a_bold_first_line() {
        assert_eq!(
            reasoning_summary("**Plan**\nbody here"),
            (Some("Plan".into()), "body here".into())
        );
        assert_eq!(
            reasoning_summary("just thinking"),
            (None, "just thinking".into())
        );
    }
}
