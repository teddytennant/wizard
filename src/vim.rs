//! Modal (vim-style) editing for the prompt, the subset Claude Code ships.
//!
//! This file is a pure engine: it takes the text and a cursor (a character index) and edits
//! them in place, with no terminal, no app and no dependencies. The wizard TUI uses it
//! directly, and the `claude` look in `looks/` compiles the same file (`#[path]`), so the
//! two cannot drift.
//!
//! Keys, as Claude Code 2.1.314 has them:
//!
//! - motions `h j k l`, `w b e`, `W B E`, `0 ^ $`, `gg G`, `f F t T` with `;` and `,`, `%`
//! - operators `d c y` with a motion, a doubled key (`dd`), `iw aw iW aW`, quotes and brackets
//! - `x X r s S D C Y p P J ~ >> << u .`, `i a I A o O`, counts, and VISUAL / VISUAL LINE
//!
//! Enter is not handled here: in INSERT and NORMAL it submits, so it is passed back.

/// Which half of modal editing the prompt is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    Normal,
    #[default]
    Insert,
    Visual,
    VisualLine,
}

/// A key as the engine reads it. Control and alt chords never get here, except Ctrl+[ which
/// the caller maps to [`VimKey::Esc`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimKey {
    Char(char),
    Esc,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// Enter, Tab, page keys: nothing here, the caller's own meaning applies.
    Other,
}

/// What the caller does after [`VimState::key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Out {
    /// The engine took the key.
    Handled,
    /// Not the engine's: the caller treats the key as usual (typing in INSERT, Enter, Tab).
    Pass,
    /// `k` or Up on the first line.
    HistoryUp,
    /// `j` or Down on the last line.
    HistoryDown,
}

const MAX_COUNT: usize = 10_000;
const UNDO_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
}

/// A cursor motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mo {
    Left,
    Right,
    Down,
    Up,
    Word(bool),
    Back(bool),
    End(bool),
    LineStart,
    FirstNonBlank,
    LineEnd,
    Top,
    Bottom,
    Find(char, char),
    Repeat(bool),
    Percent,
}

/// What an operator acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tg {
    M(Mo),
    Line,
    Obj(bool, char),
}

/// A command that can be repeated with `.`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Op(Op, Tg),
    X,
    Replace(char),
    Toggle,
    Indent(bool),
    Join,
    Paste(bool),
    Open(bool),
    Insert(Ins),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ins {
    Here,
    After,
    End,
    Start,
}

/// The command being typed in NORMAL mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Cmd {
    #[default]
    Idle,
    Count(usize),
    Operator(Op, usize),
    OperatorCount(Op, usize, usize),
    OperatorFind(Op, usize, char),
    OperatorObj(Op, usize, bool),
    OperatorG(Op, usize),
    Find(char, usize),
    G(usize),
    Replace(usize),
    Indent(bool, usize),
    // VISUAL mode
    VisualFind(char, usize),
    VisualG(usize),
    VisualObj(bool),
    VisualReplace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Recorded {
    kind: Kind,
    count: usize,
    typed: String,
}

/// An INSERT session in progress: what opened it, and what was typed since.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Session {
    kind: Option<(Kind, usize)>,
    typed: String,
    /// The text before a session that no command opened (typing from the start).
    snap: Option<(String, usize)>,
}

/// All modal-editing state for the prompt.
#[derive(Debug, Clone, Default)]
pub struct VimState {
    /// Whether modal editing is on at all (`editorMode: "vim"`). Off, the prompt is always in
    /// INSERT and the engine is never asked.
    pub enabled: bool,
    pub mode: VimMode,
    cmd: Cmd,
    last_find: Option<(char, char)>,
    last_change: Option<Recorded>,
    register: String,
    linewise: bool,
    undo: Vec<(String, usize)>,
    session: Option<Session>,
    anchor: usize,
    want_col: Option<usize>,
    past_end: bool,
    /// A count was typed for the operator, so `G` after it means that line.
    op_counted: bool,
    /// A change took its own undo step before the insert; it goes again if nothing is typed.
    extra_undo: bool,
}

/// The text as characters, with the cursor, while one key is applied.
struct Doc {
    c: Vec<char>,
    cur: usize,
}

impl Doc {
    fn len(&self) -> usize {
        self.c.len()
    }
    fn at(&self, i: usize) -> char {
        self.c.get(i).copied().unwrap_or('\0')
    }
    fn line_start(&self, i: usize) -> usize {
        let mut i = i.min(self.len());
        while i > 0 && self.c[i - 1] != '\n' {
            i -= 1;
        }
        i
    }
    /// Index of the newline that ends the line, or the length.
    fn line_end(&self, i: usize) -> usize {
        let mut i = i.min(self.len());
        while i < self.len() && self.c[i] != '\n' {
            i += 1;
        }
        i
    }
    fn line_no(&self, i: usize) -> usize {
        self.c[..i.min(self.len())]
            .iter()
            .filter(|c| **c == '\n')
            .count()
    }
    fn lines(&self) -> usize {
        self.line_no(self.len()) + 1
    }
    fn nth_line_start(&self, n: usize) -> usize {
        let mut seen = 0;
        for (i, c) in self.c.iter().enumerate() {
            if seen == n {
                return i;
            }
            if *c == '\n' {
                seen += 1;
            }
        }
        if seen == n {
            self.len()
        } else {
            self.line_start(self.len())
        }
    }
    fn col(&self, i: usize) -> usize {
        i - self.line_start(i)
    }
    fn first_non_blank(&self, i: usize) -> usize {
        let mut j = self.line_start(i);
        let end = self.line_end(i);
        while j < end && matches!(self.c[j], ' ' | '\t') {
            j += 1;
        }
        if j >= end {
            end.saturating_sub(1).max(self.line_start(i))
        } else {
            j
        }
    }
    /// Where NORMAL mode may rest: on a character, never on the newline after the last one.
    fn clamp_normal(&mut self) {
        if self.cur >= self.len() {
            let after_newline = self.len() > 0 && self.c[self.len() - 1] == '\n';
            self.cur = if after_newline {
                self.len()
            } else {
                self.len().saturating_sub(1)
            };
        }
        if self.at(self.cur) == '\n' && self.cur > 0 && self.c[self.cur - 1] != '\n' {
            self.cur -= 1;
        }
    }
    fn slice(&self, a: usize, b: usize) -> String {
        self.c[a.min(self.len())..b.min(self.len())]
            .iter()
            .collect()
    }
    fn splice(&mut self, a: usize, b: usize, with: &str) {
        let (a, b) = (a.min(self.len()), b.min(self.len()));
        self.c.splice(a..b, with.chars());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

fn class(c: char, big: bool) -> Class {
    if c.is_whitespace() {
        Class::Blank
    } else if big || c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

fn is_ws(c: char) -> bool {
    c.is_whitespace()
}

/// An empty line: a newline with nothing before it on its line.
fn empty_line_at(d: &Doc, j: usize) -> bool {
    d.at(j) == '\n' && (j == 0 || d.at(j - 1) == '\n')
}

fn word_forward(d: &Doc, i: usize, big: bool) -> usize {
    let n = d.len();
    let mut j = i;
    if j >= n {
        return n;
    }
    let cl = class(d.c[j], big);
    if cl != Class::Blank {
        while j < n && class(d.c[j], big) == cl {
            j += 1;
        }
    }
    while j < n && is_ws(d.c[j]) {
        if j > i && empty_line_at(d, j) {
            break;
        }
        j += 1;
    }
    j
}

fn word_back(d: &Doc, i: usize, big: bool) -> usize {
    if i == 0 {
        return 0;
    }
    let mut j = i - 1;
    while is_ws(d.c[j]) {
        if j == 0 {
            return 0;
        }
        j -= 1;
    }
    let cl = class(d.c[j], big);
    while j > 0 && class(d.c[j - 1], big) == cl {
        j -= 1;
    }
    j
}

fn word_end(d: &Doc, i: usize, big: bool) -> usize {
    let n = d.len();
    if n == 0 {
        return 0;
    }
    let mut j = i + 1;
    while j < n && is_ws(d.c[j]) {
        j += 1;
    }
    if j >= n {
        return n - 1;
    }
    let cl = class(d.c[j], big);
    while j + 1 < n && class(d.c[j + 1], big) == cl {
        j += 1;
    }
    j
}

fn matching(d: &Doc, i: usize) -> Option<usize> {
    let end = d.line_end(i);
    let mut j = i;
    while j < end && !"()[]{}".contains(d.c[j]) {
        j += 1;
    }
    if j >= end {
        return None;
    }
    let open = d.c[j];
    let (other, forward) = match open {
        '(' => (')', true),
        '[' => (']', true),
        '{' => ('}', true),
        ')' => ('(', false),
        ']' => ('[', false),
        _ => ('{', false),
    };
    let mut depth = 0usize;
    if forward {
        for k in j + 1..d.len() {
            if d.c[k] == open {
                depth += 1;
            } else if d.c[k] == other {
                if depth == 0 {
                    return Some(k);
                }
                depth -= 1;
            }
        }
    } else {
        for k in (0..j).rev() {
            if d.c[k] == open {
                depth += 1;
            } else if d.c[k] == other {
                if depth == 0 {
                    return Some(k);
                }
                depth -= 1;
            }
        }
    }
    None
}

/// The `count`th `kind` (`f F t T`) of `ch` on the cursor's line.
fn find_char(d: &Doc, from: usize, kind: char, ch: char, count: usize) -> Option<usize> {
    let (start, end) = (d.line_start(from), d.line_end(from));
    let mut found = 0;
    match kind {
        'f' | 't' => {
            for j in from + 1..end {
                if d.c[j] == ch {
                    found += 1;
                    if found == count {
                        return Some(if kind == 't' { j - 1 } else { j });
                    }
                }
            }
        }
        _ => {
            let mut j = from;
            while j > start {
                j -= 1;
                if d.c[j] == ch {
                    found += 1;
                    if found == count {
                        return Some(if kind == 'T' { j + 1 } else { j });
                    }
                }
            }
        }
    }
    None
}

/// A text object as `[start, end)`.
fn text_object(d: &Doc, at: usize, inner: bool, ch: char) -> Option<(usize, usize)> {
    match ch {
        'w' | 'W' => {
            let big = ch == 'W';
            let n = d.len();
            if n == 0 {
                return None;
            }
            let i = at.min(n - 1);
            let cl = class(d.c[i], big);
            let (mut s, mut e) = (i, i + 1);
            while s > 0 && class(d.c[s - 1], big) == cl && d.c[s - 1] != '\n' {
                s -= 1;
            }
            while e < n && class(d.c[e], big) == cl && d.c[e] != '\n' {
                e += 1;
            }
            if cl == Class::Blank || inner {
                return Some((s, e));
            }
            let blank = |k: usize| matches!(d.c[k], ' ' | '\t');
            if e < n && blank(e) {
                while e < n && blank(e) {
                    e += 1;
                }
            } else if s > 0 && blank(s - 1) {
                while s > 0 && blank(s - 1) {
                    s -= 1;
                }
            }
            Some((s, e))
        }
        '"' | '\'' | '`' => {
            let (ls, le) = (d.line_start(at), d.line_end(at));
            let marks: Vec<usize> = (ls..le).filter(|&k| d.c[k] == ch).collect();
            for pair in marks.chunks(2) {
                if let [a, b] = pair {
                    if *a <= at && at <= *b {
                        return Some(if inner { (a + 1, *b) } else { (*a, b + 1) });
                    }
                }
            }
            None
        }
        '(' | ')' | 'b' | '[' | ']' | '{' | '}' | 'B' | '<' | '>' => {
            let (open, close) = match ch {
                '(' | ')' | 'b' => ('(', ')'),
                '[' | ']' => ('[', ']'),
                '{' | '}' | 'B' => ('{', '}'),
                _ => ('<', '>'),
            };
            let mut depth = 0usize;
            let mut start = None;
            let mut i = at.min(d.len().saturating_sub(1)) as isize;
            if d.len() == 0 {
                return None;
            }
            while i >= 0 {
                let c = d.c[i as usize];
                if c == close && i as usize != at {
                    depth += 1;
                } else if c == open {
                    if depth == 0 {
                        start = Some(i as usize);
                        break;
                    }
                    depth -= 1;
                }
                i -= 1;
            }
            let start = start?;
            let mut depth = 0usize;
            let mut end = None;
            for k in start + 1..d.len() {
                if d.c[k] == open {
                    depth += 1;
                } else if d.c[k] == close {
                    if depth == 0 {
                        end = Some(k);
                        break;
                    }
                    depth -= 1;
                }
            }
            let end = end?;
            Some(if inner {
                (start + 1, end)
            } else {
                (start, end + 1)
            })
        }
        _ => None,
    }
}

/// Where a motion lands, and how an operator reads it.
struct Landing {
    to: usize,
    inclusive: bool,
    linewise: bool,
}

impl VimState {
    /// True when keys are read as motions and operators.
    pub fn is_normal(&self) -> bool {
        self.enabled && self.mode == VimMode::Normal
    }

    /// True in VISUAL or VISUAL LINE.
    pub fn is_visual(&self) -> bool {
        self.enabled && matches!(self.mode, VimMode::Visual | VimMode::VisualLine)
    }

    /// The mode as the footer prints it (`NORMAL`, `INSERT`, `VISUAL`, `VISUAL LINE`).
    pub fn label(&self) -> Option<&'static str> {
        if !self.enabled {
            return None;
        }
        Some(match self.mode {
            VimMode::Normal => "NORMAL",
            VimMode::Insert => "INSERT",
            VimMode::Visual => "VISUAL",
            VimMode::VisualLine => "VISUAL LINE",
        })
    }

    /// The command being typed so far, for a pending-keys display. Empty when idle.
    pub fn is_pending(&self) -> bool {
        self.cmd != Cmd::Idle
    }

    /// Turn modal editing on or off. On, the prompt starts in INSERT.
    pub fn set_enabled(&mut self, on: bool) {
        *self = VimState {
            enabled: on,
            ..VimState::default()
        };
    }

    /// The selection in VISUAL modes as `[start, end)` character indexes.
    pub fn selection(&self, text: &str, cursor: usize) -> Option<(usize, usize)> {
        if !self.is_visual() {
            return None;
        }
        let d = Doc {
            c: text.chars().collect(),
            cur: cursor,
        };
        let (a, b) = (self.anchor.min(cursor), self.anchor.max(cursor));
        Some(if self.mode == VimMode::VisualLine {
            (d.line_start(a), (d.line_end(b) + 1).min(d.len()))
        } else {
            (a, (b + 1).min(d.len()))
        })
    }

    /// Put a cursor that landed off the text (a recalled prompt) back on a character, in the
    /// modes where the cursor sits on one.
    pub fn clamp(&self, text: &str, cursor: &mut usize) {
        if matches!(self.mode, VimMode::Insert) || !self.enabled {
            return;
        }
        let mut d = Doc {
            c: text.chars().collect(),
            cur: *cursor,
        };
        d.clamp_normal();
        *cursor = d.cur;
    }

    /// Drop the half-typed command (Esc, a dialog taking focus).
    pub fn clear_pending(&mut self) {
        self.cmd = Cmd::Idle;
    }

    /// Go to INSERT with nothing pending (a completion was accepted from NORMAL).
    pub fn insert_mode(&mut self) {
        self.mode = VimMode::Insert;
        self.cmd = Cmd::Idle;
        self.session = Some(Session {
            kind: None,
            typed: String::new(),
            snap: None,
        });
    }

    /// Back to INSERT with nothing pending, e.g. after the prompt is submitted.
    pub fn reset_to_insert(&mut self) {
        self.mode = VimMode::Insert;
        self.cmd = Cmd::Idle;
        self.session = None;
    }

    fn push_undo(&mut self, d: &Doc) {
        self.undo.push((d.c.iter().collect(), d.cur));
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }

    /// Apply one key to `text`, with the cursor a character index. The cursor is moved to
    /// where it belongs, including off the newline in NORMAL.
    pub fn key(&mut self, text: &mut String, cursor: &mut usize, key: VimKey) -> Out {
        let mut d = Doc {
            c: text.chars().collect(),
            cur: (*cursor).min(text.chars().count()),
        };
        let before_len = d.len();
        let out = match self.mode {
            VimMode::Insert => self.insert_key(&mut d, key),
            VimMode::Normal => self.normal_key(&mut d, key),
            VimMode::Visual | VimMode::VisualLine => self.visual_key(&mut d, key),
        };
        if out != Out::Pass || d.len() != before_len {
            let new: String = d.c.iter().collect();
            if new != *text {
                *text = new;
            }
            *cursor = d.cur;
        }
        out
    }

    /// The caller typed or deleted in INSERT itself; keep what a `.` will replay.
    pub fn typed(&mut self, c: char) {
        if let Some(s) = self.session.as_mut() {
            s.typed.push(c);
        }
    }

    /// A backspace the caller did in INSERT.
    pub fn typed_backspace(&mut self) {
        if let Some(s) = self.session.as_mut() {
            s.typed.pop();
        }
    }

    // --- INSERT ---

    fn insert_key(&mut self, d: &mut Doc, key: VimKey) -> Out {
        match key {
            VimKey::Esc => {
                self.leave_insert(d);
                Out::Handled
            }
            VimKey::Char(c) => {
                self.note_insert(d);
                self.typed(c);
                Out::Pass
            }
            VimKey::Backspace => {
                self.note_insert(d);
                self.typed_backspace();
                Out::Pass
            }
            _ => Out::Pass,
        }
    }

    /// Typing with no command behind it starts a session of its own, one undo step.
    fn note_insert(&mut self, d: &Doc) {
        let s = self.session.get_or_insert_with(|| Session {
            kind: None,
            typed: String::new(),
            snap: None,
        });
        if s.kind.is_none() && s.snap.is_none() {
            s.snap = Some((d.c.iter().collect(), d.cur));
        }
    }

    fn leave_insert(&mut self, d: &mut Doc) {
        if std::mem::take(&mut self.extra_undo)
            && self.session.as_ref().is_none_or(|s| s.typed.is_empty())
        {
            self.undo.pop();
        }
        if let Some(s) = self.session.take() {
            if let Some(snap) = s.snap {
                self.undo.push(snap);
            }
            if let Some((kind, count)) = s.kind {
                self.last_change = Some(Recorded {
                    kind,
                    count,
                    typed: s.typed,
                });
            }
        }
        self.mode = VimMode::Normal;
        self.cmd = Cmd::Idle;
        if d.cur > d.line_start(d.cur) {
            d.cur -= 1;
        }
        d.clamp_normal();
        self.want_col = None;
    }

    fn enter_insert(&mut self, kind: Option<(Kind, usize)>) {
        self.mode = VimMode::Insert;
        self.cmd = Cmd::Idle;
        self.session = Some(Session {
            kind,
            typed: String::new(),
            snap: None,
        });
    }

    // --- NORMAL ---

    fn normal_key(&mut self, d: &mut Doc, key: VimKey) -> Out {
        let c = match key {
            VimKey::Char(c) => c,
            VimKey::Esc => {
                self.cmd = Cmd::Idle;
                return Out::Handled;
            }
            VimKey::Left | VimKey::Backspace => 'h',
            VimKey::Right => 'l',
            VimKey::Up => 'k',
            VimKey::Down => 'j',
            VimKey::Home => '0',
            VimKey::End => '$',
            VimKey::Delete => 'x',
            VimKey::Other => {
                self.cmd = Cmd::Idle;
                return Out::Pass;
            }
        };
        self.past_end = false;
        let out = self.normal_char(d, c);
        if self.mode == VimMode::Normal && !self.past_end {
            d.clamp_normal();
        }
        out
    }

    fn normal_char(&mut self, d: &mut Doc, c: char) -> Out {
        match self.cmd {
            Cmd::Idle => self.idle(d, c, 1, false),
            Cmd::Count(n) => {
                if let Some(v) = c.to_digit(10) {
                    self.cmd = Cmd::Count((n * 10 + v as usize).min(MAX_COUNT));
                    Out::Handled
                } else {
                    self.cmd = Cmd::Idle;
                    self.idle(d, c, n, true)
                }
            }
            Cmd::Operator(op, n) => self.operator(d, op, n, c),
            Cmd::OperatorCount(op, n, m) => {
                if let Some(v) = c.to_digit(10) {
                    self.cmd = Cmd::OperatorCount(op, n, (m * 10 + v as usize).min(MAX_COUNT));
                    Out::Handled
                } else {
                    self.cmd = Cmd::Operator(op, (n * m).min(MAX_COUNT));
                    self.operator_after_count(d, op, (n * m).min(MAX_COUNT), c)
                }
            }
            Cmd::OperatorFind(op, n, kind) => {
                self.cmd = Cmd::Idle;
                self.last_find = Some((kind, c));
                self.run(d, Kind::Op(op, Tg::M(Mo::Find(kind, c))), n, false);
                Out::Handled
            }
            Cmd::OperatorObj(op, n, inner) => {
                self.cmd = Cmd::Idle;
                if "wW\"'`()b[]{}B<>".contains(c) {
                    self.run(d, Kind::Op(op, Tg::Obj(inner, c)), n, false);
                }
                Out::Handled
            }
            Cmd::OperatorG(op, n) => {
                self.cmd = Cmd::Idle;
                match c {
                    'g' => {
                        self.run(d, Kind::Op(op, Tg::M(Mo::Top)), n, false);
                    }
                    'j' => {
                        self.run(d, Kind::Op(op, Tg::M(Mo::Down)), n, false);
                    }
                    'k' => {
                        self.run(d, Kind::Op(op, Tg::M(Mo::Up)), n, false);
                    }
                    _ => {}
                }
                Out::Handled
            }
            Cmd::Find(kind, n) => {
                self.cmd = Cmd::Idle;
                self.last_find = Some((kind, c));
                self.run(d, Kind::Op(Op::Yank, Tg::M(Mo::Find(kind, c))), n, true);
                Out::Handled
            }
            Cmd::G(n) => {
                self.cmd = Cmd::Idle;
                match c {
                    'g' => {
                        if n > 1 {
                            d.cur = d.first_non_blank(d.nth_line_start((n - 1).min(d.lines() - 1)));
                        } else {
                            d.cur = d.first_non_blank(0);
                        }
                        self.want_col = None;
                    }
                    'j' => return self.vertical(d, true, n),
                    'k' => return self.vertical(d, false, n),
                    _ => {}
                }
                Out::Handled
            }
            Cmd::Replace(n) => {
                self.cmd = Cmd::Idle;
                self.run(d, Kind::Replace(c), n, false);
                Out::Handled
            }
            Cmd::Indent(right, n) => {
                self.cmd = Cmd::Idle;
                if c == if right { '>' } else { '<' } {
                    self.run(d, Kind::Indent(right), n, false);
                }
                Out::Handled
            }
            Cmd::VisualFind(..) | Cmd::VisualG(_) | Cmd::VisualObj(_) | Cmd::VisualReplace => {
                self.cmd = Cmd::Idle;
                Out::Handled
            }
        }
    }

    /// `counted` is true when the count was typed, so `G` and `gg` mean a line number.
    fn idle(&mut self, d: &mut Doc, c: char, n: usize, counted: bool) -> Out {
        match c {
            '1'..='9' if !counted => {
                self.cmd = Cmd::Count(c.to_digit(10).unwrap_or(1) as usize);
            }
            'd' | 'c' | 'y' => {
                let op = match c {
                    'd' => Op::Delete,
                    'c' => Op::Change,
                    _ => Op::Yank,
                };
                self.op_counted = counted;
                self.cmd = Cmd::Operator(op, n);
            }
            'f' | 'F' | 't' | 'T' => self.cmd = Cmd::Find(c, n),
            'g' => self.cmd = Cmd::G(n),
            'r' => self.cmd = Cmd::Replace(n),
            '>' => self.cmd = Cmd::Indent(true, n),
            '<' => self.cmd = Cmd::Indent(false, n),
            'h' => self.motion(d, Mo::Left, n),
            'l' | ' ' => self.motion(d, Mo::Right, n),
            'w' => self.motion(d, Mo::Word(false), n),
            'W' => self.motion(d, Mo::Word(true), n),
            'b' => self.motion(d, Mo::Back(false), n),
            'B' => self.motion(d, Mo::Back(true), n),
            'e' => self.motion(d, Mo::End(false), n),
            'E' => self.motion(d, Mo::End(true), n),
            '0' => self.motion(d, Mo::LineStart, n),
            '^' => self.motion(d, Mo::FirstNonBlank, n),
            '$' => self.motion(d, Mo::LineEnd, n),
            '%' => self.motion(d, Mo::Percent, n),
            ';' => self.motion(d, Mo::Repeat(false), n),
            ',' => self.motion(d, Mo::Repeat(true), n),
            'G' => {
                d.cur = if counted {
                    d.first_non_blank(d.nth_line_start((n - 1).min(d.lines() - 1)))
                } else {
                    d.first_non_blank(d.nth_line_start(d.lines() - 1))
                };
                self.want_col = None;
            }
            'j' => return self.vertical(d, true, n),
            'k' => return self.vertical(d, false, n),
            'x' => self.run(d, Kind::X, n, false),
            '~' => self.run(d, Kind::Toggle, n, false),
            'J' => self.run(d, Kind::Join, n, false),
            'p' => self.run(d, Kind::Paste(true), n, false),
            'P' => self.run(d, Kind::Paste(false), n, false),
            'D' => self.run(d, Kind::Op(Op::Delete, Tg::M(Mo::LineEnd)), 1, false),
            'C' => self.run(d, Kind::Op(Op::Change, Tg::M(Mo::LineEnd)), 1, false),
            'Y' => self.run(d, Kind::Op(Op::Yank, Tg::Line), n, false),
            's' => self.run(d, Kind::Op(Op::Change, Tg::M(Mo::Right)), n, false),
            'S' => self.run(d, Kind::Op(Op::Change, Tg::Line), n, false),
            'o' => self.run(d, Kind::Open(false), 1, false),
            'O' => self.run(d, Kind::Open(true), 1, false),
            'i' => self.run(d, Kind::Insert(Ins::Here), n, false),
            'I' => self.run(d, Kind::Insert(Ins::Start), n, false),
            'a' => self.run(d, Kind::Insert(Ins::After), n, false),
            'A' => self.run(d, Kind::Insert(Ins::End), n, false),
            'u' => self.undo(d),
            '.' => self.dot(d, counted.then_some(n)),
            'v' => self.enter_visual(d, VimMode::Visual),
            'V' => self.enter_visual(d, VimMode::VisualLine),
            _ => {
                self.cmd = Cmd::Idle;
                return Out::Pass;
            }
        }
        Out::Handled
    }

    fn operator(&mut self, d: &mut Doc, op: Op, n: usize, c: char) -> Out {
        if let Some(v) = c.to_digit(10).filter(|v| *v > 0) {
            self.op_counted = true;
            self.cmd = Cmd::OperatorCount(op, n, v as usize);
            return Out::Handled;
        }
        self.operator_after_count(d, op, n, c)
    }

    fn operator_after_count(&mut self, d: &mut Doc, op: Op, n: usize, c: char) -> Out {
        let same = match op {
            Op::Delete => 'd',
            Op::Change => 'c',
            Op::Yank => 'y',
        };
        self.cmd = Cmd::Idle;
        if c == same {
            self.run(d, Kind::Op(op, Tg::Line), n, false);
            return Out::Handled;
        }
        let mo = match c {
            'h' => Mo::Left,
            'l' | ' ' => Mo::Right,
            'j' => Mo::Down,
            'k' => Mo::Up,
            'w' => Mo::Word(false),
            'W' => Mo::Word(true),
            'b' => Mo::Back(false),
            'B' => Mo::Back(true),
            'e' => Mo::End(false),
            'E' => Mo::End(true),
            '0' => Mo::LineStart,
            '^' => Mo::FirstNonBlank,
            '$' => Mo::LineEnd,
            '%' => Mo::Percent,
            'G' => {
                // a bare operator count of 1 means "no count": the last line
                let target = if n == 1 && !self.op_counted {
                    usize::MAX
                } else {
                    n
                };
                self.op_to_line(d, op, target);
                return Out::Handled;
            }
            'g' => {
                self.cmd = Cmd::OperatorG(op, n);
                return Out::Handled;
            }
            'i' => {
                self.cmd = Cmd::OperatorObj(op, n, true);
                return Out::Handled;
            }
            'a' => {
                self.cmd = Cmd::OperatorObj(op, n, false);
                return Out::Handled;
            }
            'f' | 'F' | 't' | 'T' => {
                self.cmd = Cmd::OperatorFind(op, n, c);
                return Out::Handled;
            }
            _ => return Out::Handled,
        };
        self.run(d, Kind::Op(op, Tg::M(mo)), n, false);
        Out::Handled
    }

    /// `dG` and `d{n}G`: linewise from here to a line.
    fn op_to_line(&mut self, d: &mut Doc, op: Op, line: usize) {
        let last = d.lines() - 1;
        let target = if line == usize::MAX {
            last
        } else {
            (line - 1).min(last)
        };
        let here = d.line_no(d.cur);
        let (a, b) = (here.min(target), here.max(target));
        let start = d.nth_line_start(a);
        let end = (d.line_end(d.nth_line_start(b)) + 1).min(d.len());
        self.apply_range(d, Kind::Op(op, Tg::M(Mo::Bottom)), 1, start, end, true);
    }

    // --- motions ---

    fn motion(&mut self, d: &mut Doc, mo: Mo, n: usize) {
        if !matches!(mo, Mo::Down | Mo::Up) {
            self.want_col = None;
        }
        if let Some(l) = self.land(d, mo, n, false) {
            d.cur = l.to;
        }
    }

    fn vertical(&mut self, d: &mut Doc, down: bool, n: usize) -> Out {
        let line = d.line_no(d.cur);
        if down && line + 1 >= d.lines() {
            return Out::HistoryDown;
        }
        if !down && line == 0 {
            return Out::HistoryUp;
        }
        self.motion(d, if down { Mo::Down } else { Mo::Up }, n);
        Out::Handled
    }

    /// Resolve a motion from the cursor. `op` is true for an operator's target.
    fn land(&mut self, d: &Doc, mo: Mo, n: usize, op: bool) -> Option<Landing> {
        let cur = d.cur;
        let here = |to| Landing {
            to,
            inclusive: false,
            linewise: false,
        };
        Some(match mo {
            Mo::Left => {
                let s = d.line_start(cur);
                here(cur.saturating_sub(n).max(s))
            }
            Mo::Right => {
                let e = d.line_end(cur);
                let cap = if op {
                    e
                } else {
                    e.saturating_sub(1).max(d.line_start(cur))
                };
                here((cur + n).min(cap))
            }
            Mo::Down | Mo::Up => {
                let line = d.line_no(cur);
                let last = d.lines() - 1;
                let target = if mo == Mo::Down {
                    (line + n).min(last)
                } else {
                    line.saturating_sub(n)
                };
                if target == line {
                    return if op { None } else { Some(here(cur)) };
                }
                let col = self.want_col.unwrap_or_else(|| d.col(cur));
                self.want_col = Some(col);
                let s = d.nth_line_start(target);
                let e = d.line_end(s);
                let max = if op { e } else { e.saturating_sub(1).max(s) };
                Landing {
                    to: (s + col).min(max),
                    inclusive: false,
                    linewise: true,
                }
            }
            Mo::Word(big) => {
                let mut j = cur;
                for _ in 0..n {
                    let next = word_forward(d, j, big);
                    if next == j {
                        break;
                    }
                    j = next;
                }
                if op {
                    // the last word on a line ends the range at that word, not on the next line
                    let mut to = j;
                    if d.line_no(to) > d.line_no(cur) {
                        let mut k = to;
                        while k > cur && is_ws(d.at(k - 1)) {
                            k -= 1;
                        }
                        to = if k > cur { k } else { d.line_end(cur) };
                    }
                    here(to)
                } else {
                    // Claude lets `w` on the last word rest just past it
                    self.past_end = j >= d.len();
                    here(j.min(d.len()))
                }
            }
            Mo::Back(big) => {
                let mut j = cur;
                for _ in 0..n {
                    let prev = word_back(d, j, big);
                    if prev == j {
                        break;
                    }
                    j = prev;
                }
                here(j)
            }
            Mo::End(big) => {
                let mut j = cur;
                for _ in 0..n {
                    let next = word_end(d, j, big);
                    if next == j {
                        break;
                    }
                    j = next;
                }
                Landing {
                    to: j,
                    inclusive: true,
                    linewise: false,
                }
            }
            Mo::LineStart => here(d.line_start(cur)),
            Mo::FirstNonBlank => here(d.first_non_blank(cur)),
            Mo::LineEnd => {
                let target = (d.line_no(cur) + n - 1).min(d.lines() - 1);
                let e = d.line_end(d.nth_line_start(target));
                Landing {
                    to: e.saturating_sub(1).max(d.nth_line_start(target)),
                    inclusive: true,
                    linewise: false,
                }
            }
            Mo::Top => {
                let to = if n > 1 {
                    d.nth_line_start((n - 1).min(d.lines() - 1))
                } else {
                    0
                };
                Landing {
                    to: d.first_non_blank(to),
                    inclusive: false,
                    linewise: true,
                }
            }
            Mo::Bottom => Landing {
                to: d.first_non_blank(d.nth_line_start(d.lines() - 1)),
                inclusive: false,
                linewise: true,
            },
            Mo::Find(kind, ch) => {
                let to = find_char(d, cur, kind, ch, n)?;
                Landing {
                    to,
                    inclusive: true,
                    linewise: false,
                }
            }
            Mo::Repeat(rev) => {
                let (kind, ch) = self.last_find?;
                let kind = if rev {
                    match kind {
                        'f' => 'F',
                        'F' => 'f',
                        't' => 'T',
                        _ => 't',
                    }
                } else {
                    kind
                };
                let to = find_char(d, cur, kind, ch, n)?;
                Landing {
                    to,
                    inclusive: true,
                    linewise: false,
                }
            }
            Mo::Percent => {
                let to = matching(d, cur)?;
                Landing {
                    to,
                    inclusive: true,
                    linewise: false,
                }
            }
        })
    }

    // --- commands ---

    /// Run a command. `plain_find` is a bare `f`/`t`, which only moves.
    fn run(&mut self, d: &mut Doc, kind: Kind, n: usize, plain_find: bool) {
        if plain_find {
            if let Kind::Op(_, Tg::M(mo)) = kind {
                self.want_col = None;
                if let Some(l) = self.land(d, mo, n, false) {
                    d.cur = l.to;
                }
            }
            return;
        }
        self.want_col = None;
        match kind {
            Kind::Op(op, Tg::M(mo)) => self.op_motion(d, kind, op, mo, n),
            Kind::Op(_, Tg::Line) => self.op_line(d, kind, n),
            Kind::Op(_, Tg::Obj(inner, ch)) => match text_object(d, d.cur, inner, ch) {
                Some((a, b)) => self.apply_range(d, kind, n, a, b, false),
                None => {}
            },
            Kind::X => {
                if d.len() == 0 || d.at(d.cur) == '\n' {
                    return;
                }
                let end = (d.cur + n).min(d.line_end(d.cur));
                let start = d.cur;
                self.apply_range(d, kind, n, start, end, false);
            }
            Kind::Replace(ch) => {
                let end = d.line_end(d.cur);
                if d.cur + n > end {
                    return;
                }
                self.push_undo(d);
                for k in 0..n {
                    d.c[d.cur + k] = ch;
                }
                d.cur += n - 1;
                self.finish_change(kind, n);
            }
            Kind::Toggle => {
                if d.at(d.cur) == '\0' || d.at(d.cur) == '\n' {
                    return;
                }
                self.push_undo(d);
                let end = (d.cur + n).min(d.line_end(d.cur));
                for k in d.cur..end {
                    let ch = d.c[k];
                    d.c[k] = if ch.is_uppercase() {
                        ch.to_lowercase().next().unwrap_or(ch)
                    } else {
                        ch.to_uppercase().next().unwrap_or(ch)
                    };
                }
                d.cur = end;
                self.finish_change(kind, n);
            }
            Kind::Indent(right) => {
                self.push_undo(d);
                let line = d.line_no(d.cur);
                let last = d.lines() - 1;
                for l in line..=(line + n - 1).min(last) {
                    let s = d.nth_line_start(l);
                    if right {
                        d.splice(s, s, "  ");
                    } else {
                        let mut k = 0;
                        while k < 2 && matches!(d.at(s), ' ' | '\t') {
                            d.splice(s, s + 1, "");
                            k += 1;
                        }
                    }
                }
                d.cur = d.first_non_blank(d.nth_line_start(line));
                self.finish_change(kind, n);
            }
            Kind::Join => {
                let line = d.line_no(d.cur);
                if line + 1 >= d.lines() {
                    return;
                }
                self.push_undo(d);
                let joins = n.max(2) - 1;
                let mut at = d.cur;
                for _ in 0..joins.min(d.lines() - 1 - line) {
                    let e = d.line_end(d.cur);
                    if e >= d.len() {
                        break;
                    }
                    let mut next = e + 1;
                    while matches!(d.at(next), ' ' | '\t') {
                        next += 1;
                    }
                    let blank_next = d.at(next) == '\n' || next >= d.len();
                    let sep =
                        if blank_next || e == d.line_start(e) || matches!(d.c[e - 1], ' ' | '\t') {
                            ""
                        } else {
                            " "
                        };
                    at = e;
                    d.splice(e, next, sep);
                }
                d.cur = at;
                self.finish_change(kind, n);
            }
            Kind::Paste(after) => self.paste(d, kind, after, n),
            Kind::Open(above) => {
                self.push_undo(d);
                let at = if above {
                    d.line_start(d.cur)
                } else {
                    d.line_end(d.cur)
                };
                if above {
                    d.splice(at, at, "\n");
                    d.cur = at;
                } else {
                    d.splice(at, at, "\n");
                    d.cur = at + 1;
                }
                self.enter_insert(Some((kind, n)));
            }
            Kind::Insert(ins) => {
                self.push_undo(d);
                d.cur = match ins {
                    Ins::Here => d.cur,
                    Ins::After => {
                        if d.at(d.cur) == '\n' || d.cur >= d.len() {
                            d.cur
                        } else {
                            d.cur + 1
                        }
                    }
                    Ins::End => d.line_end(d.cur),
                    Ins::Start => {
                        let mut j = d.line_start(d.cur);
                        while matches!(d.at(j), ' ' | '\t') {
                            j += 1;
                        }
                        j
                    }
                };
                self.enter_insert(Some((kind, n)));
            }
        }
    }

    fn finish_change(&mut self, kind: Kind, n: usize) {
        self.last_change = Some(Recorded {
            kind,
            count: n,
            typed: String::new(),
        });
    }

    fn op_motion(&mut self, d: &mut Doc, kind: Kind, op: Op, mo: Mo, n: usize) {
        let cur = d.cur;
        let empty =
            empty_line_at(d, cur) || (cur == d.len() && (cur == 0 || d.at(cur - 1) == '\n'));
        if empty && matches!(mo, Mo::Word(_)) {
            match op {
                Op::Delete if cur < d.len() => self.apply_range(d, kind, n, cur, cur + 1, false),
                Op::Change => {
                    self.push_undo(d);
                    self.enter_insert(Some((kind, n)));
                }
                _ => {}
            }
            return;
        }
        // `cw` on a word changes to its end, not to the start of the next word
        if let (Op::Change, Mo::Word(big)) = (op, mo) {
            if !is_ws(d.at(cur)) && d.at(cur) != '\0' {
                let cl = class(d.at(cur), big);
                let mut end = cur;
                while end + 1 < d.len() && class(d.at(end + 1), big) == cl {
                    end += 1;
                }
                for _ in 1..n {
                    end = word_end(d, end, big);
                }
                self.apply_range(d, kind, n, cur, end + 1, false);
                return;
            }
        }
        let (mo, count) = (mo, n);
        let Some(l) = self.land(d, mo, count, true) else {
            return;
        };
        if l.to == cur && !l.inclusive && !l.linewise {
            if op == Op::Change && mo == Mo::Right {
                self.push_undo(d);
                self.enter_insert(Some((kind, n)));
            }
            return;
        }
        if l.linewise {
            let (a, b) = (
                d.line_no(cur).min(d.line_no(l.to)),
                d.line_no(cur).max(d.line_no(l.to)),
            );
            let start = d.nth_line_start(a);
            let end = (d.line_end(d.nth_line_start(b)) + 1).min(d.len());
            self.apply_range(d, kind, n, start, end, true);
            return;
        }
        let (a, mut b) = (cur.min(l.to), cur.max(l.to));
        if l.inclusive {
            b = (b + 1).min(d.len());
            if mo == Mo::LineEnd && b > 0 && d.at(b - 1) == '\n' {
                b -= 1;
            }
        }
        self.apply_range(d, kind, n, a, b, false);
    }

    fn op_line(&mut self, d: &mut Doc, kind: Kind, n: usize) {
        let line = d.line_no(d.cur);
        let last = d.lines() - 1;
        let to = (line + n - 1).min(last);
        let start = d.nth_line_start(line);
        let end = (d.line_end(d.nth_line_start(to)) + 1).min(d.len());
        self.apply_range(d, kind, n, start, end, true);
    }

    /// Apply the operator in `kind` to `[a, b)`.
    #[allow(clippy::too_many_arguments)]
    fn apply_range(
        &mut self,
        d: &mut Doc,
        kind: Kind,
        n: usize,
        a: usize,
        b: usize,
        linewise: bool,
    ) {
        let op = match kind {
            Kind::Op(op, _) => op,
            _ => Op::Delete,
        };
        let mut text = d.slice(a, b);
        if linewise && !text.ends_with('\n') {
            text.push('\n');
        }
        self.register = text;
        self.linewise = linewise;
        match op {
            Op::Yank => {
                d.cur = a;
                self.want_col = None;
            }
            Op::Delete => {
                self.push_undo(d);
                let (mut a, b) = (a, b);
                if linewise && b >= d.len() && a > 0 && d.at(a - 1) == '\n' {
                    a -= 1;
                }
                d.splice(a, b, "");
                d.cur = a;
                if linewise {
                    d.cur = d.first_non_blank(d.line_start(a.min(d.len())));
                    if d.len() > 0 && d.cur >= d.len() {
                        d.cur = d.len() - 1;
                    }
                }
                d.clamp_normal();
                self.finish_change(kind, n);
            }
            Op::Change => {
                self.push_undo(d);
                if linewise {
                    let keep_nl = b > a && d.at(b - 1) == '\n';
                    d.splice(a, if keep_nl { b - 1 } else { b }, "");
                    d.cur = a;
                } else {
                    d.splice(a, b, "");
                    d.cur = a;
                }
                self.push_undo(d);
                self.extra_undo = true;
                self.enter_insert(Some((kind, n)));
            }
        }
    }

    fn paste(&mut self, d: &mut Doc, kind: Kind, after: bool, n: usize) {
        if self.register.is_empty() {
            return;
        }
        self.push_undo(d);
        if self.linewise {
            let body = self.register.trim_end_matches('\n').to_string();
            let mut block = String::new();
            for k in 0..n {
                if k > 0 {
                    block.push('\n');
                }
                block.push_str(&body);
            }
            if after {
                let e = d.line_end(d.cur);
                d.splice(e, e, &format!("\n{block}"));
                d.cur = e + 1;
            } else {
                let s = d.line_start(d.cur);
                d.splice(s, s, &format!("{block}\n"));
                d.cur = s;
            }
            d.cur = d.first_non_blank(d.cur);
        } else {
            let text = self.register.repeat(n);
            let at = if after && d.cur < d.len() && d.at(d.cur) != '\n' {
                d.cur + 1
            } else {
                d.cur
            };
            d.splice(at, at, &text);
            // a paste of several lines leaves the cursor where it starts
            d.cur = if text.contains('\n') {
                at
            } else {
                (at + text.chars().count()).saturating_sub(1).max(at)
            };
        }
        self.finish_change(kind, n);
    }

    fn undo(&mut self, d: &mut Doc) {
        if let Some((text, cur)) = self.undo.pop() {
            d.c = text.chars().collect();
            d.cur = cur;
            d.clamp_normal();
        }
    }

    fn dot(&mut self, d: &mut Doc, count: Option<usize>) {
        let Some(rec) = self.last_change.clone() else {
            return;
        };
        let n = count.unwrap_or(rec.count);
        self.run(d, rec.kind, n, false);
        if self.mode == VimMode::Insert {
            if !rec.typed.is_empty() {
                let at = d.cur;
                d.splice(at, at, &rec.typed);
                d.cur = at + rec.typed.chars().count();
            }
            self.session = Some(Session {
                kind: Some((rec.kind, n)),
                typed: rec.typed,
                snap: None,
            });
            self.leave_insert(d);
        }
    }

    // --- VISUAL ---

    fn enter_visual(&mut self, d: &mut Doc, mode: VimMode) {
        self.mode = mode;
        self.anchor = d.cur;
        self.cmd = Cmd::Idle;
    }

    fn leave_visual(&mut self) {
        self.mode = VimMode::Normal;
        self.cmd = Cmd::Idle;
    }

    fn visual_key(&mut self, d: &mut Doc, key: VimKey) -> Out {
        let c = match key {
            VimKey::Char(c) => c,
            VimKey::Esc => {
                self.leave_visual();
                return Out::Handled;
            }
            VimKey::Left | VimKey::Backspace => 'h',
            VimKey::Right => 'l',
            VimKey::Up => 'k',
            VimKey::Down => 'j',
            VimKey::Home => '0',
            VimKey::End => '$',
            VimKey::Delete => 'x',
            VimKey::Other => return Out::Pass,
        };
        let out = self.visual_char(d, c);
        if self.is_visual_mode() {
            if d.cur >= d.len() {
                d.cur = d.len().saturating_sub(1);
            }
        } else {
            d.clamp_normal();
        }
        out
    }

    fn is_visual_mode(&self) -> bool {
        matches!(self.mode, VimMode::Visual | VimMode::VisualLine)
    }

    fn visual_char(&mut self, d: &mut Doc, c: char) -> Out {
        match self.cmd {
            Cmd::VisualFind(kind, n) => {
                self.cmd = Cmd::Idle;
                self.last_find = Some((kind, c));
                if let Some(to) = find_char(d, d.cur, kind, c, n) {
                    d.cur = to;
                }
                return Out::Handled;
            }
            Cmd::VisualG(n) => {
                self.cmd = Cmd::Idle;
                match c {
                    'g' => {
                        d.cur = if n > 1 {
                            d.first_non_blank(d.nth_line_start((n - 1).min(d.lines() - 1)))
                        } else {
                            0
                        }
                    }
                    'j' => self.motion(d, Mo::Down, n),
                    'k' => self.motion(d, Mo::Up, n),
                    _ => {}
                }
                return Out::Handled;
            }
            Cmd::VisualObj(inner) => {
                self.cmd = Cmd::Idle;
                if let Some((a, b)) = text_object(d, d.cur, inner, c) {
                    self.anchor = a;
                    d.cur = b.saturating_sub(1).max(a);
                }
                return Out::Handled;
            }
            Cmd::VisualReplace => {
                self.cmd = Cmd::Idle;
                let (a, b) = self.span(d);
                self.push_undo(d);
                for k in a..b {
                    if d.c[k] != '\n' {
                        d.c[k] = c;
                    }
                }
                d.cur = a;
                self.leave_visual();
                return Out::Handled;
            }
            Cmd::Count(n) => {
                if let Some(v) = c.to_digit(10) {
                    self.cmd = Cmd::Count((n * 10 + v as usize).min(MAX_COUNT));
                    return Out::Handled;
                }
            }
            _ => {}
        }
        let n = match self.cmd {
            Cmd::Count(n) => n,
            _ => 1,
        };
        let counted = matches!(self.cmd, Cmd::Count(_));
        self.cmd = Cmd::Idle;
        match c {
            '1'..='9' if !counted => self.cmd = Cmd::Count(c.to_digit(10).unwrap_or(1) as usize),
            'h' => self.motion(d, Mo::Left, n),
            'l' | ' ' => self.motion(d, Mo::Right, n),
            'w' => self.motion(d, Mo::Word(false), n),
            'W' => self.motion(d, Mo::Word(true), n),
            'b' => self.motion(d, Mo::Back(false), n),
            'B' => self.motion(d, Mo::Back(true), n),
            'e' => self.motion(d, Mo::End(false), n),
            'E' => self.motion(d, Mo::End(true), n),
            '0' => self.motion(d, Mo::LineStart, n),
            '^' => self.motion(d, Mo::FirstNonBlank, n),
            '$' => self.motion(d, Mo::LineEnd, n),
            '%' => self.motion(d, Mo::Percent, n),
            ';' => self.motion(d, Mo::Repeat(false), n),
            ',' => self.motion(d, Mo::Repeat(true), n),
            'j' => self.motion(d, Mo::Down, n),
            'k' => self.motion(d, Mo::Up, n),
            'G' => {
                d.cur = if counted {
                    d.first_non_blank(d.nth_line_start((n - 1).min(d.lines() - 1)))
                } else {
                    d.first_non_blank(d.nth_line_start(d.lines() - 1))
                }
            }
            'f' | 'F' | 't' | 'T' => self.cmd = Cmd::VisualFind(c, n),
            'g' => self.cmd = Cmd::VisualG(n),
            'i' => self.cmd = Cmd::VisualObj(true),
            'a' => self.cmd = Cmd::VisualObj(false),
            'r' => self.cmd = Cmd::VisualReplace,
            'o' => std::mem::swap(&mut self.anchor, &mut d.cur),
            'v' => {
                if self.mode == VimMode::Visual {
                    self.leave_visual();
                } else {
                    self.mode = VimMode::Visual;
                }
            }
            'V' => {
                if self.mode == VimMode::VisualLine {
                    self.leave_visual();
                } else {
                    self.mode = VimMode::VisualLine;
                }
            }
            'd' | 'x' | 'y' | 'c' | 's' | 'D' | 'X' | 'Y' | 'C' | 'S' => {
                let (a, b) = self.span(d);
                let linewise =
                    self.mode == VimMode::VisualLine || matches!(c, 'D' | 'X' | 'Y' | 'C' | 'S');
                let (a, b) = if linewise {
                    (
                        d.line_start(a),
                        (d.line_end(b.saturating_sub(1).max(a)) + 1).min(d.len()),
                    )
                } else {
                    (a, b)
                };
                let op = match c {
                    'y' | 'Y' => Op::Yank,
                    'c' | 's' | 'C' | 'S' => Op::Change,
                    _ => Op::Delete,
                };
                self.leave_visual();
                self.apply_range(d, Kind::Op(op, Tg::Line), 1, a, b, linewise);
                self.last_change = None;
            }
            'p' | 'P' => {
                let (a, b) = self.span(d);
                let was_linewise = self.linewise;
                let reg = self.register.clone();
                let sel_linewise = self.mode == VimMode::VisualLine;
                self.leave_visual();
                if reg.is_empty() {
                    return Out::Handled;
                }
                self.push_undo(d);
                let (a, b) = if sel_linewise {
                    (
                        d.line_start(a),
                        (d.line_end(b.saturating_sub(1).max(a)) + 1).min(d.len()),
                    )
                } else {
                    (a, b)
                };
                let replaced = d.slice(a, b);
                let body = if was_linewise && !sel_linewise {
                    format!("\n{}\n", reg.trim_end_matches('\n'))
                } else if sel_linewise && !was_linewise {
                    format!("{reg}\n")
                } else {
                    reg
                };
                d.splice(a, b, &body);
                d.cur = (a + body.chars().count()).saturating_sub(1).max(a);
                self.register = replaced;
                self.linewise = sel_linewise;
                d.clamp_normal();
            }
            '~' | 'u' | 'U' => {
                let (a, b) = self.span(d);
                self.push_undo(d);
                for k in a..b {
                    let ch = d.c[k];
                    d.c[k] = match c {
                        'u' => ch.to_lowercase().next().unwrap_or(ch),
                        'U' => ch.to_uppercase().next().unwrap_or(ch),
                        _ => {
                            if ch.is_uppercase() {
                                ch.to_lowercase().next().unwrap_or(ch)
                            } else {
                                ch.to_uppercase().next().unwrap_or(ch)
                            }
                        }
                    };
                }
                d.cur = a;
                self.leave_visual();
            }
            '>' | '<' => {
                let (a, b) = self.span(d);
                let first = d.line_no(a);
                let last = d.line_no(b.saturating_sub(1).max(a));
                self.leave_visual();
                d.cur = d.nth_line_start(first);
                self.run(d, Kind::Indent(c == '>'), last - first + 1, false);
            }
            'J' => {
                let (a, b) = self.span(d);
                let first = d.line_no(a);
                let last = d.line_no(b.saturating_sub(1).max(a));
                self.leave_visual();
                d.cur = d.nth_line_start(first);
                self.run(d, Kind::Join, (last - first + 1).max(2), false);
            }
            _ => return Out::Pass,
        }
        Out::Handled
    }

    /// The selection as `[a, b)` in VISUAL, widened to whole lines in VISUAL LINE.
    fn span(&self, d: &Doc) -> (usize, usize) {
        let (a, b) = (self.anchor.min(d.cur), self.anchor.max(d.cur));
        if self.mode == VimMode::VisualLine {
            (d.line_start(a), (d.line_end(b) + 1).min(d.len()))
        } else {
            (a, (b + 1).min(d.len()))
        }
    }
}

#[cfg(test)]
#[path = "vim_tests.rs"]
mod tests;
