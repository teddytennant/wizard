// OWNER: renderer (test support; others add helpers, do not reshape)
//! Headless testing: a writer the test can read, a small terminal emulator that understands the
//! escape sequences codexw writes (cursor moves, SGR, erase, scroll regions, reverse index, the
//! alternate screen), and a `Harness` that runs an `App` on it. The emulator keeps a scrollback
//! like a real terminal, so a test sees what a user would after history has scrolled.

use std::io::Write;
use std::sync::{Arc, Mutex};

use agent_core::Request;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier, Style};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::app::{App, AppOpts};
use crate::style::{ColorLevel, Palette, set_palette};

#[derive(Clone, Default)]
pub struct SharedBuf(pub Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    /// Take everything written so far.
    pub fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

impl Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The dark capture terminal: default fg #e6e6e6 on bg #000000, truecolor.
pub fn dark_palette() -> Palette {
    Palette::new(
        Some((230, 230, 230)),
        Some((0, 0, 0)),
        ColorLevel::TrueColor,
    )
}

/// The light capture terminal: fg #1a1a1a on bg #ffffff.
pub fn light_palette() -> Palette {
    Palette::new(
        Some((26, 26, 26)),
        Some((255, 255, 255)),
        ColorLevel::TrueColor,
    )
}

/// Remove OSC sequences (terminal title writes), which byte comparisons ignore.
pub fn strip_osc(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b && b.get(i + 1) == Some(&b']') {
            while i < b.len() && b[i] != 0x07 {
                i += 1;
            }
            i += 1;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Printable rendering of an escape stream for assertion messages.
pub fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .replace('\x1b', "\\e")
        .replace('\r', "\\r")
        .replace('\n', "\\n\n")
}

// ---- the emulator -----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    /// 1-based index into `Screen::link_urls`, 0 for none (OSC 8 state when the cell was written).
    pub link: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            style: Style::default(),
            link: 0,
        }
    }
}

#[derive(Clone)]
struct Grid {
    rows: Vec<Vec<Cell>>,
}

impl Grid {
    fn new(w: usize, h: usize) -> Self {
        Grid {
            rows: vec![vec![Cell::default(); w]; h],
        }
    }
}

/// A terminal screen with scrollback. Feed it the bytes the app wrote.
pub struct Screen {
    pub width: usize,
    pub height: usize,
    main: Grid,
    alt: Grid,
    in_alt: bool,
    pub scrollback: Vec<Vec<Cell>>,
    x: usize,
    y: usize,
    saved: (usize, usize),
    top: usize,
    bottom: usize,
    style: Style,
    wrap_pending: bool,
    pub cursor_visible: bool,
    pub title: String,
    /// Destinations seen in OSC 8, in first-seen order; `Cell::link` indexes it from 1.
    pub link_urls: Vec<String>,
    link: u16,
    pub alt_screen_entered: usize,
    pending: Vec<u8>,
    /// How many times a screen or scrollback clear was seen.
    pub scrollback_clears: usize,
}

impl Screen {
    pub fn new(width: usize, height: usize) -> Self {
        Screen {
            width,
            height,
            main: Grid::new(width, height),
            alt: Grid::new(width, height),
            in_alt: false,
            scrollback: Vec::new(),
            x: 0,
            y: 0,
            saved: (0, 0),
            top: 0,
            bottom: height - 1,
            style: Style::default(),
            wrap_pending: false,
            cursor_visible: true,
            title: String::new(),
            link_urls: Vec::new(),
            link: 0,
            alt_screen_entered: 0,
            pending: Vec::new(),
            scrollback_clears: 0,
        }
    }

    /// Put the cursor somewhere (a shell prompt above the app).
    pub fn set_cursor(&mut self, x: usize, y: usize) {
        self.x = x;
        self.y = y;
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.x, self.y)
    }

    fn grid(&mut self) -> &mut Grid {
        if self.in_alt {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        for g in [&mut self.main, &mut self.alt] {
            for r in g.rows.iter_mut() {
                r.resize(width, Cell::default());
            }
            g.rows.resize(height, vec![Cell::default(); width]);
        }
        self.top = 0;
        self.bottom = height - 1;
        self.x = self.x.min(width - 1);
        self.y = self.y.min(height - 1);
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            match b {
                0x1b => match self.escape(&data[i..]) {
                    Some(n) => i += n,
                    None => {
                        self.pending = data[i..].to_vec();
                        return;
                    }
                },
                b'\r' => {
                    self.x = 0;
                    self.wrap_pending = false;
                    i += 1;
                }
                b'\n' => {
                    self.line_feed();
                    i += 1;
                }
                0x07 | 0x08 => i += 1,
                b if b < 0x20 => i += 1,
                _ => {
                    let len = utf8_len(b);
                    if i + len > data.len() {
                        self.pending = data[i..].to_vec();
                        return;
                    }
                    if let Ok(s) = std::str::from_utf8(&data[i..i + len]) {
                        if let Some(c) = s.chars().next() {
                            self.put(c);
                        }
                    }
                    i += len;
                }
            }
        }
    }

    fn put(&mut self, c: char) {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        if w == 0 {
            return;
        }
        if self.wrap_pending || self.x + w > self.width {
            self.x = 0;
            self.line_feed();
        }
        let (x, y, style, link) = (self.x, self.y, self.style, self.link);
        self.grid().rows[y][x] = Cell { ch: c, style, link };
        for k in 1..w {
            if x + k < self.width {
                self.grid().rows[y][x + k] = Cell {
                    ch: '\0',
                    style,
                    link,
                };
            }
        }
        self.x += w;
        self.wrap_pending = self.x >= self.width;
        if self.wrap_pending {
            self.x = self.width - 1;
        }
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.y == self.bottom {
            self.scroll_up(1);
        } else if self.y + 1 < self.height {
            self.y += 1;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n {
            let (top, bottom) = (self.top, self.bottom);
            let full = top == 0 && !self.in_alt;
            let w = self.width;
            let g = self.grid();
            let row = g.rows.remove(top);
            g.rows.insert(bottom, vec![Cell::default(); w]);
            if full {
                self.scrollback.push(row);
            }
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n {
            let (top, bottom, w) = (self.top, self.bottom, self.width);
            let g = self.grid();
            g.rows.remove(bottom);
            g.rows.insert(top, vec![Cell::default(); w]);
        }
    }

    fn erase_cell(&self) -> Cell {
        Cell {
            ch: ' ',
            style: Style {
                bg: self.style.bg,
                ..Style::default()
            },
            link: 0,
        }
    }

    /// Parse one escape sequence at the start of `d`; `None` when it is incomplete.
    fn escape(&mut self, d: &[u8]) -> Option<usize> {
        match d.get(1)? {
            b'[' => {
                let mut j = 2;
                while j < d.len() && !(0x40..=0x7e).contains(&d[j]) {
                    j += 1;
                }
                if j >= d.len() {
                    return None;
                }
                let params = String::from_utf8_lossy(&d[2..j]).to_string();
                self.csi(&params, d[j]);
                Some(j + 1)
            }
            b']' => {
                let mut j = 2;
                while j < d.len() && d[j] != 0x07 && !(d[j] == 0x1b && d.get(j + 1) == Some(&b'\\'))
                {
                    j += 1;
                }
                if j >= d.len() {
                    return None;
                }
                let body = String::from_utf8_lossy(&d[2..j]).to_string();
                if let Some(t) = body.strip_prefix("0;") {
                    self.title = t.to_string();
                } else if let Some(rest) = body.strip_prefix("8;") {
                    // `8;params;URL`; an empty URL closes the link.
                    let url = rest.split_once(';').map_or("", |(_, u)| u);
                    self.link = if url.is_empty() {
                        0
                    } else {
                        let at = self.link_urls.iter().position(|u| u == url);
                        at.unwrap_or_else(|| {
                            self.link_urls.push(url.to_string());
                            self.link_urls.len() - 1
                        }) as u16
                            + 1
                    };
                }
                Some(j + if d[j] == 0x1b { 2 } else { 1 })
            }
            b'M' => {
                if self.y == self.top {
                    self.scroll_down(1);
                } else if self.y > 0 {
                    self.y -= 1;
                }
                Some(2)
            }
            b'7' => {
                self.saved = (self.x, self.y);
                Some(2)
            }
            b'8' => {
                (self.x, self.y) = self.saved;
                Some(2)
            }
            _ => Some(2),
        }
    }

    fn csi(&mut self, params: &str, fin: u8) {
        let private = params.starts_with('?');
        let nums: Vec<usize> = params
            .trim_start_matches(['?', '>', '<', '='])
            .split(';')
            .map(|s| s.trim_end_matches(' ').parse().unwrap_or(0))
            .collect();
        let n1 = |d: usize| nums.first().copied().filter(|v| *v > 0).unwrap_or(d);
        match fin {
            b'H' | b'f' => {
                self.y = (n1(1) - 1).min(self.height - 1);
                self.x =
                    (nums.get(1).copied().filter(|v| *v > 0).unwrap_or(1) - 1).min(self.width - 1);
                self.wrap_pending = false;
            }
            b'A' => self.y = self.y.saturating_sub(n1(1)),
            b'B' => self.y = (self.y + n1(1)).min(self.height - 1),
            b'C' => self.x = (self.x + n1(1)).min(self.width - 1),
            b'D' => self.x = self.x.saturating_sub(n1(1)),
            b'G' => {
                self.x = (n1(1) - 1).min(self.width - 1);
                self.wrap_pending = false;
            }
            b'J' => {
                let mode = nums.first().copied().unwrap_or(0);
                let blank = self.erase_cell();
                let (x, y, w, h) = (self.x, self.y, self.width, self.height);
                match mode {
                    0 => {
                        let g = self.grid();
                        for c in g.rows[y].iter_mut().skip(x) {
                            *c = blank.clone();
                        }
                        for r in g.rows.iter_mut().skip(y + 1) {
                            *r = vec![blank.clone(); w];
                        }
                    }
                    2 => {
                        let g = self.grid();
                        g.rows = vec![vec![blank.clone(); w]; h];
                        self.scrollback_clears += 1;
                    }
                    3 => {
                        self.scrollback.clear();
                    }
                    _ => {}
                }
            }
            b'K' => {
                let blank = self.erase_cell();
                let (x, y) = (self.x, self.y);
                let g = self.grid();
                for c in g.rows[y].iter_mut().skip(x) {
                    *c = blank.clone();
                }
            }
            b'S' => self.scroll_up(n1(1)),
            b'r' => {
                if nums.is_empty() || nums[0] == 0 {
                    self.top = 0;
                    self.bottom = self.height - 1;
                } else {
                    self.top = (n1(1) - 1).min(self.height - 1);
                    self.bottom = (nums
                        .get(1)
                        .copied()
                        .filter(|v| *v > 0)
                        .unwrap_or(self.height)
                        - 1)
                    .min(self.height - 1);
                    if self.top >= self.bottom {
                        self.top = 0;
                        self.bottom = self.height - 1;
                    }
                }
                self.x = 0;
                self.y = 0;
            }
            b'm' if !private => self.sgr(&nums),
            b'h' | b'l' if private => {
                let on = fin == b'h';
                match nums.first().copied().unwrap_or(0) {
                    1049 => {
                        self.in_alt = on;
                        if on {
                            self.alt_screen_entered += 1;
                            self.alt = Grid::new(self.width, self.height);
                        }
                    }
                    25 => self.cursor_visible = on,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn sgr(&mut self, p: &[usize]) {
        let mut i = 0;
        let p = if p.is_empty() { &[0][..] } else { p };
        while i < p.len() {
            let c = p[i];
            let s = &mut self.style;
            match c {
                0 => *s = Style::default(),
                1 => *s = s.add_modifier(Modifier::BOLD),
                2 => *s = s.add_modifier(Modifier::DIM),
                3 => *s = s.add_modifier(Modifier::ITALIC),
                4 => *s = s.add_modifier(Modifier::UNDERLINED),
                7 => *s = s.add_modifier(Modifier::REVERSED),
                9 => *s = s.add_modifier(Modifier::CROSSED_OUT),
                22 => *s = s.remove_modifier(Modifier::BOLD | Modifier::DIM),
                23 => *s = s.remove_modifier(Modifier::ITALIC),
                24 => *s = s.remove_modifier(Modifier::UNDERLINED),
                27 => *s = s.remove_modifier(Modifier::REVERSED),
                29 => *s = s.remove_modifier(Modifier::CROSSED_OUT),
                30..=37 => s.fg = Some(Color::Indexed((c - 30) as u8)),
                39 => s.fg = None,
                40..=47 => s.bg = Some(Color::Indexed((c - 40) as u8)),
                49 => s.bg = None,
                90..=97 => s.fg = Some(Color::Indexed((c - 90 + 8) as u8)),
                100..=107 => s.bg = Some(Color::Indexed((c - 100 + 8) as u8)),
                38 | 48 => {
                    let color = match p.get(i + 1) {
                        Some(5) => {
                            let v = p.get(i + 2).copied().unwrap_or(0) as u8;
                            i += 2;
                            Some(Color::Indexed(v))
                        }
                        Some(2) => {
                            let g = |k: usize| p.get(i + k).copied().unwrap_or(0) as u8;
                            let col = Color::Rgb(g(2), g(3), g(4));
                            i += 4;
                            Some(col)
                        }
                        _ => None,
                    };
                    if c == 38 {
                        s.fg = color;
                    } else {
                        s.bg = color;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    // ---- reading --------------------------------------------------------------------------

    fn visible(&self) -> &Grid {
        if self.in_alt { &self.alt } else { &self.main }
    }

    pub fn in_alt_screen(&self) -> bool {
        self.in_alt
    }

    pub fn cell(&self, x: usize, y: usize) -> &Cell {
        &self.visible().rows[y][x]
    }

    fn row_text(row: &[Cell]) -> String {
        let s: String = row.iter().filter(|c| c.ch != '\0').map(|c| c.ch).collect();
        s.trim_end().to_string()
    }

    /// One string per screen row, trailing spaces trimmed (like `tmux capture-pane -p`).
    pub fn rows(&self) -> Vec<String> {
        self.visible()
            .rows
            .iter()
            .map(|r| Self::row_text(r))
            .collect()
    }

    pub fn text(&self) -> String {
        self.rows().join("\n")
    }

    /// Scrollback and screen together, trailing blank rows dropped (`capture-pane -S -`).
    pub fn full_text(&self) -> String {
        let mut rows: Vec<String> = self.scrollback.iter().map(|r| Self::row_text(r)).collect();
        rows.extend(self.main.rows.iter().map(|r| Self::row_text(r)));
        while rows.last().is_some_and(|r| r.is_empty()) {
            rows.pop();
        }
        rows.join("\n")
    }

    /// The text of every link in scrollback and on screen with its destination, in reading order.
    /// A link that wraps over rows comes back as one entry per row.
    pub fn links(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for r in self.scrollback.iter().chain(self.main.rows.iter()) {
            let mut cur: Option<(u16, String)> = None;
            for c in r.iter().chain(std::iter::once(&Cell::default())) {
                match (&mut cur, c.link) {
                    (Some((id, t)), l) if *id == l => {
                        if c.ch != '\0' {
                            t.push(c.ch)
                        }
                    }
                    _ => {
                        if let Some((id, t)) = cur.take() {
                            out.push((t, self.link_urls[id as usize - 1].clone()));
                        }
                        if c.link != 0 {
                            cur = Some((c.link, c.ch.to_string()));
                        }
                    }
                }
            }
        }
        out
    }

    /// Style of the first cell on a row whose text contains `needle`, at the needle's column.
    pub fn style_of(&self, needle: &str) -> Option<Style> {
        for (y, r) in self.visible().rows.iter().enumerate() {
            let t: String = r
                .iter()
                .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                .collect();
            if let Some(col) = t.find(needle) {
                let x = t[..col].chars().count();
                return Some(self.cell(x, y).style);
            }
        }
        None
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

// ---- harness ----------------------------------------------------------------------------------

/// An `App` on a headless terminal. `pump` moves what the app wrote into the emulator.
pub struct Harness {
    pub app: App<SharedBuf>,
    pub out: SharedBuf,
    pub requests: UnboundedReceiver<Request>,
    pub screen: Screen,
    /// Everything written so far, for byte-level checks.
    pub bytes: Vec<u8>,
}

/// A headless app on a `cols` x `rows` screen with the cursor at the top, as in a fresh tmux
/// pane, home `/tmp/cxw-home` and cwd `/tmp/cxw-home/proj` (shown as `~/proj`), model
/// `gpt-5.5`, the dark capture palette. `placeholder` picks the composer placeholder.
pub fn harness(cols: u16, rows: u16, placeholder: usize) -> Harness {
    harness_with(
        cols,
        rows,
        AppOpts {
            placeholder: Some(placeholder),
            ..Default::default()
        },
    )
}

pub fn harness_with(cols: u16, rows: u16, mut opts: AppOpts) -> Harness {
    set_palette(dark_palette());
    let (tx, requests) = unbounded_channel();
    let out = SharedBuf::default();
    if opts.cwd.as_os_str().is_empty() {
        opts.cwd = "/tmp/cxw-home/proj".into();
    }
    opts.home.get_or_insert_with(|| "/tmp/cxw-home".into());
    opts.model.get_or_insert_with(|| "gpt-5.5".into());
    let app = App::new(
        opts,
        tx,
        out.clone(),
        Size::new(cols, rows),
        Position::new(0, 0),
    );
    Harness {
        app,
        out,
        requests,
        screen: Screen::new(cols as usize, rows as usize),
        bytes: Vec::new(),
    }
}

impl Harness {
    /// Feed the emulator what the app has written since the last call; returns those bytes.
    pub fn pump(&mut self) -> Vec<u8> {
        let b = self.out.take();
        self.screen.write(&b);
        self.bytes.extend_from_slice(&b);
        b
    }

    /// Draw one frame and pump it.
    pub fn draw(&mut self) -> Vec<u8> {
        self.app.draw().unwrap();
        self.pump()
    }

    /// Resize the emulated terminal and the app, then let the debounce elapse and redraw.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.screen.resize(cols as usize, rows as usize);
        self.app.on_resize(cols, rows);
        self.draw();
        self.app.reflow_now();
        self.draw();
    }

    pub fn key(&mut self, code: crossterm::event::KeyCode) {
        self.app.on_key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ));
    }

    pub fn ctrl(&mut self, c: char) {
        self.app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::CONTROL,
        ));
    }

    pub fn type_str(&mut self, s: &str) {
        for ch in s.chars() {
            self.key(crossterm::event::KeyCode::Char(ch));
        }
    }

    /// Everything the app asked the backend for since the last call.
    pub fn sent(&mut self) -> Vec<Request> {
        let mut v = Vec::new();
        while let Ok(r) = self.requests.try_recv() {
            v.push(r);
        }
        v
    }
}
