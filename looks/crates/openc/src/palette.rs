// OWNER: visual
//! `hearth` (dark) and `parchment` (light), plus the glyph tables.
//!
//! Every widget draws with the truecolor tokens below. Lower colour depths are not a second
//! set of styles: [`Palette::downgrade`] rewrites the finished frame buffer, so a 16-colour
//! terminal gets exactly the same layout, cell for cell, and only the colours differ
//! (design checklist 20). Token colours are looked up exactly; anything else (a colour that
//! came from a syntax theme, say) falls back to the nearest xterm index or to a hue class.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use tuikit::theme::{Mode, Theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    True,
    Ansi256,
    Ansi16,
    /// `NO_COLOR` or `TERM=dumb`: attributes and glyphs only.
    Mono,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hearth,
    Parchment,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Hearth => "hearth",
            Kind::Parchment => "parchment",
        }
    }
}

const fn rgb(h: u32) -> Color {
    Color::Rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

/// One row of the design's token table: hex and 256-colour index for dark and light.
struct Tok {
    dark: (u32, u8),
    light: (u32, u8),
}

const fn tok(dark: u32, d256: u8, light: u32, l256: u8) -> Tok {
    Tok {
        dark: (dark, d256),
        light: (light, l256),
    }
}

const BG: Tok = tok(0x14110f, 233, 0xfaf6ee, 255);
const SURFACE: Tok = tok(0x221d18, 235, 0xeee6d8, 254);
const RAISED: Tok = tok(0x2d271f, 236, 0xe5dccb, 253);
// The dock (todos, queued messages) sits at the end of the range opposite to the code and tool
// surfaces: they step toward the text colour, the dock steps away from it (a recessed tray on
// dark, a lighter card on light). Going the other way would put it on `raised`, the composer's
// own step, and `faint` text only clears 4.5:1 up to about that lightness in the dark theme.
/// Alpha of the black scrim behind a dialog on the light theme.
pub const LIGHT_SCRIM: f32 = 0.15;

const DOCK: Tok = tok(0x0c0a08, 232, 0xfffcf6, 231);
const LINE: Tok = tok(0x3a332b, 238, 0xcfc4b2, 250);
const TEXT: Tok = tok(0xece4d6, 254, 0x2a251f, 235);
const DIM: Tok = tok(0xb5aa9a, 250, 0x5a5146, 239);
const FAINT: Tok = tok(0x9c9282, 247, 0x5f564a, 240);
const ACCENT: Tok = tok(0x6fc2b0, 79, 0x11695a, 23);
const ACCENT_BG: Tok = tok(0x1f3a35, 237, 0xcfe8df, 152);
const USER: Tok = tok(0xb9a6ee, 147, 0x5b46b0, 56);
const OK: Tok = tok(0x93c47d, 114, 0x2f7430, 22);
const WARN: Tok = tok(0xe5ad4a, 179, 0x8f5d00, 94);
const ERR: Tok = tok(0xec7a6a, 209, 0xb0352a, 124);
const INFO: Tok = tok(0x7fb2e0, 110, 0x1f5f9e, 25);
const ADD_BG: Tok = tok(0x1d2f1f, 237, 0xd9eed0, 194);
const DEL_BG: Tok = tok(0x3a1f1c, 237, 0xf6d8d2, 224);
// 256 colours, dark theme. The nearest cube entries to the word tints are #005f00 and #5f0000
// (dE2000 8.8 and 9.2), full-saturation blocks, which round 2 finding 3 asked to be rid of and
// checklist 15 bans; the nearest greys sit 14 to 16 away. The row tints are the greys 237
// (dE 14.2 and 15.4, against 13.4 and 13.9 for the very nearest) because the nearest, 235, is
// the context row's own `surface`; the changed words are the grey 240, 13 dE units above the
// row, with bold. Hue stays in the foreground of the sign and the numbers. (A cube 22 or 52
// tab behind the sign was tried: three cells of #005f00 on every changed row is the neon
// again.)
const ADD_WORD: Tok = tok(0x2f5a2f, 240, 0xb4dca3, 151);
const DEL_WORD: Tok = tok(0x6a2e29, 240, 0xeab0a6, 181);
const ADD_FG: Tok = tok(0xa6d68f, 150, 0x1f5f22, 22);
const DEL_FG: Tok = tok(0xf0908a, 210, 0x9c2a20, 124);
const KW: Tok = tok(0xd89ad6, 176, 0x8d3d8a, 90);
const STR: Tok = tok(0xa9cf8a, 150, 0x3d6b1f, 22);
const NUM: Tok = tok(0xe5ad4a, 179, 0x7d5100, 94);
const FUNC: Tok = tok(0x7fb2e0, 110, 0x1f5f9e, 25);
const TYPE: Tok = tok(0x6fc2b0, 79, 0x0f6454, 23);
const COMMENT: Tok = tok(0x9c9282, 247, 0x5f564a, 240);
// Inline code chip, mouse selection and the current search match. Not in the design's table.
// The chip is `text` on the `raised` step with no hue of its own (finding 24: the yellow-brown
// one was the only fifth colour in prose); the others were picked so `text` clears 7:1.
// One digit off `raised` so the 16 colour downgrade can tell the two apart.
const CHIP_BG: Tok = tok(0x2e2820, 236, 0xe6dcc6, 253);
const CHIP_FG: Tok = tok(0xece4d6, 254, 0x2a251f, 235);
const SEL_BG: Tok = tok(0x2c4a66, 24, 0xb9d3ea, 153);
const CUR_BG: Tok = tok(0x6b5320, 94, 0xf1d58a, 222);

#[derive(Clone, Debug)]
pub struct Palette {
    pub kind: Kind,
    pub depth: Depth,
    pub bg: Color,
    pub surface: Color,
    pub raised: Color,
    /// The dock tray: its own surface, apart from `surface` (code, tool bodies) and `raised`.
    pub dock: Color,
    pub line: Color,
    pub text: Color,
    pub dim: Color,
    pub faint: Color,
    pub accent: Color,
    pub accent_bg: Color,
    pub user: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub info: Color,
    pub add_bg: Color,
    pub del_bg: Color,
    pub add_word: Color,
    pub del_word: Color,
    pub add_fg: Color,
    pub del_fg: Color,
    pub kw: Color,
    pub str_: Color,
    pub num: Color,
    pub func: Color,
    pub ty: Color,
    pub comment: Color,
    pub chip_bg: Color,
    pub chip_fg: Color,
    /// Mouse selection.
    pub sel_bg: Color,
    /// The search match the cursor is on; the other matches use `accent_bg`.
    pub cur_bg: Color,
    /// `(truecolor, xterm index)` for every token, for the 256-colour downgrade.
    idx: Vec<(Color, u8)>,
    fg16: Vec<(Color, Color, Modifier)>,
    bg16: Vec<(Color, Modifier)>,
}

impl Palette {
    pub fn new(kind: Kind, depth: Depth) -> Self {
        let pick = |t: &Tok| match kind {
            Kind::Hearth => (rgb(t.dark.0), t.dark.1),
            Kind::Parchment => (rgb(t.light.0), t.light.1),
        };
        let c = |t: &Tok| pick(t).0;
        let toks = [
            &BG, &SURFACE, &RAISED, &DOCK, &LINE, &TEXT, &DIM, &FAINT, &ACCENT, &ACCENT_BG, &USER,
            &OK, &WARN, &ERR, &INFO, &ADD_BG, &DEL_BG, &ADD_WORD, &DEL_WORD, &ADD_FG, &DEL_FG, &KW,
            &STR, &NUM, &FUNC, &TYPE, &COMMENT, &CHIP_BG, &CHIP_FG, &SEL_BG, &CUR_BG,
        ];
        let idx = toks.iter().map(|t| pick(t)).collect();
        let none = Modifier::empty();
        let dim = Modifier::DIM;
        // 16-colour: default foreground for body text, SGR 2 for dim and faint (never
        // bright black), the terminal's own hues for the rest.
        let fg16 = vec![
            (c(&TEXT), Color::Reset, none),
            (c(&DIM), Color::Reset, dim),
            (c(&FAINT), Color::Reset, dim),
            (c(&ACCENT), Color::Cyan, none),
            (c(&USER), Color::Magenta, none),
            (c(&OK), Color::Green, none),
            (c(&WARN), Color::Yellow, none),
            (c(&ERR), Color::Red, none),
            (c(&INFO), Color::Blue, none),
            (c(&ADD_FG), Color::Green, none),
            (c(&DEL_FG), Color::Red, none),
            (c(&KW), Color::Magenta, none),
            (c(&STR), Color::Green, none),
            (c(&NUM), Color::Yellow, none),
            (c(&FUNC), Color::Blue, none),
            (c(&TYPE), Color::Cyan, none),
            (c(&COMMENT), Color::Reset, dim),
        ];
        // No background tints in 16 colours; the changed words of a diff get bold+underline.
        // The `line` tint marks the selected row of a list and the paste chip: reverse video.
        let bg16 = vec![
            (c(&ADD_WORD), Modifier::BOLD | Modifier::UNDERLINED),
            (c(&DEL_WORD), Modifier::BOLD | Modifier::UNDERLINED),
            (c(&LINE), Modifier::REVERSED),
            // Search hits and the selection have to show without a tint: underline marks the
            // hits, reverse video marks the one you are on and the selection.
            (c(&ACCENT_BG), Modifier::UNDERLINED),
            (c(&CUR_BG), Modifier::REVERSED | Modifier::BOLD),
            (c(&SEL_BG), Modifier::REVERSED),
        ];
        Palette {
            kind,
            depth,
            bg: c(&BG),
            surface: c(&SURFACE),
            raised: c(&RAISED),
            dock: c(&DOCK),
            line: c(&LINE),
            text: c(&TEXT),
            dim: c(&DIM),
            faint: c(&FAINT),
            accent: c(&ACCENT),
            accent_bg: c(&ACCENT_BG),
            user: c(&USER),
            ok: c(&OK),
            warn: c(&WARN),
            err: c(&ERR),
            info: c(&INFO),
            add_bg: c(&ADD_BG),
            del_bg: c(&DEL_BG),
            add_word: c(&ADD_WORD),
            del_word: c(&DEL_WORD),
            add_fg: c(&ADD_FG),
            del_fg: c(&DEL_FG),
            kw: c(&KW),
            str_: c(&STR),
            num: c(&NUM),
            func: c(&FUNC),
            ty: c(&TYPE),
            comment: c(&COMMENT),
            chip_bg: c(&CHIP_BG),
            chip_fg: c(&CHIP_FG),
            sel_bg: c(&SEL_BG),
            cur_bg: c(&CUR_BG),
            idx,
            fg16,
            bg16,
        }
    }

    /// The colour a terminal at this depth will really show for a token: itself in truecolor,
    /// the xterm value of its index in 256 colours. Contrast has to be judged on this.
    pub fn seen(&self, c: Color) -> Color {
        match (self.depth, c) {
            (Depth::Ansi256, Color::Rgb(r, g, b)) => {
                let (r, g, b) = index_rgb(self.index_of(c, r, g, b));
                Color::Rgb(r, g, b)
            }
            _ => c,
        }
    }

    pub fn is_dark(&self) -> bool {
        self.kind == Kind::Hearth
    }

    // Foreground styles. Backgrounds are applied by whoever knows the surface.
    pub fn s_text(&self) -> Style {
        Style::new().fg(self.text)
    }
    pub fn s_dim(&self) -> Style {
        Style::new().fg(self.dim)
    }
    pub fn s_faint(&self) -> Style {
        Style::new().fg(self.faint)
    }
    pub fn s_accent(&self) -> Style {
        Style::new().fg(self.accent)
    }
    pub fn s_user(&self) -> Style {
        Style::new().fg(self.user)
    }
    pub fn s_ok(&self) -> Style {
        Style::new().fg(self.ok)
    }
    pub fn s_warn(&self) -> Style {
        Style::new().fg(self.warn)
    }
    pub fn s_err(&self) -> Style {
        Style::new().fg(self.err)
    }
    pub fn s_line(&self) -> Style {
        Style::new().fg(self.line)
    }
    /// Inline code: its own foreground on its own tint.
    pub fn s_chip(&self) -> Style {
        Style::new().fg(self.chip_fg).bg(self.chip_bg)
    }

    /// The tuikit theme for the shared widgets (syntax colours, select lists, toasts).
    pub fn theme(&self) -> Theme {
        let mode = if self.is_dark() {
            Mode::Dark
        } else {
            Mode::Light
        };
        let hex = |c: Color| match c {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            _ => "#000000".into(),
        };
        let mut m = serde_json::Map::new();
        let mut put = |k: &str, c: Color| {
            m.insert(k.into(), serde_json::Value::String(hex(c)));
        };
        put("primary", self.line);
        put("secondary", self.info);
        put("accent", self.accent);
        put("error", self.err);
        put("warning", self.warn);
        put("success", self.ok);
        put("info", self.info);
        put("text", self.text);
        put("textMuted", self.dim);
        put("selectedListItemText", self.text);
        put("background", self.bg);
        put("backgroundPanel", self.raised);
        put("backgroundElement", self.surface);
        put("backgroundMenu", self.raised);
        put("border", self.line);
        put("borderActive", self.accent);
        put("borderSubtle", self.line);
        put("diffAdded", self.add_fg);
        put("diffRemoved", self.del_fg);
        put("diffContext", self.faint);
        put("diffHunkHeader", self.faint);
        put("diffHighlightAdded", self.add_fg);
        put("diffHighlightRemoved", self.del_fg);
        put("diffAddedBg", self.add_bg);
        put("diffRemovedBg", self.del_bg);
        put("diffContextBg", self.bg);
        put("diffLineNumber", self.faint);
        put("diffAddedLineNumberBg", self.add_bg);
        put("diffRemovedLineNumberBg", self.del_bg);
        put("markdownText", self.text);
        put("markdownHeading", self.accent);
        put("markdownLink", self.accent);
        put("markdownLinkText", self.accent);
        put("markdownCode", self.dim);
        put("markdownBlockQuote", self.dim);
        put("markdownEmph", self.text);
        put("markdownStrong", self.text);
        put("markdownHorizontalRule", self.line);
        put("markdownListItem", self.text);
        put("markdownListEnumeration", self.dim);
        put("markdownImage", self.accent);
        put("markdownImageText", self.dim);
        put("markdownCodeBlock", self.text);
        put("syntaxComment", self.comment);
        put("syntaxKeyword", self.kw);
        put("syntaxFunction", self.func);
        put("syntaxVariable", self.text);
        put("syntaxString", self.str_);
        put("syntaxNumber", self.num);
        put("syntaxType", self.ty);
        put("syntaxOperator", self.dim);
        put("syntaxPunctuation", self.dim);
        let json = serde_json::json!({ "theme": m }).to_string();
        let mut t = Theme::from_json_named(self.kind.name(), &json, mode)
            .unwrap_or_else(|_| Theme::default_theme(mode));
        if !self.is_dark() {
            // The default scrim is black at 59 percent, a good dark backdrop and on a light page
            // a dirty grey wash (#676562, text at 2.2 to 3.3:1, round 2 finding 4). A light
            // page only needs to step back from the dialog's `raised` panel: black at
            // `LIGHT_SCRIM`, which keeps the warm hue and the text above 4.5:1.
            t.overlay_alpha = LIGHT_SCRIM;
        }
        t
    }

    /// Rewrite a finished frame for the terminal's colour depth. A no-op in truecolor.
    pub fn downgrade(&self, buf: &mut Buffer) {
        if self.depth == Depth::True {
            return;
        }
        let area = buf.area;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let Some(cell) = buf.cell_mut((x, y)) else {
                    continue;
                };
                let (fg, fm) = self.map_fg(cell.fg);
                let (bg, bm) = self.map_bg(cell.bg);
                cell.fg = fg;
                cell.bg = bg;
                cell.modifier |= fm | bm;
            }
        }
    }

    fn map_fg(&self, c: Color) -> (Color, Modifier) {
        let Color::Rgb(r, g, b) = c else {
            return (c, Modifier::empty());
        };
        match self.depth {
            Depth::True => (c, Modifier::empty()),
            Depth::Ansi256 => (Color::Indexed(self.index_of(c, r, g, b)), Modifier::empty()),
            Depth::Ansi16 => match self.fg16.iter().find(|(t, _, _)| *t == c) {
                Some((_, col, m)) => (*col, *m),
                None => hue16(r, g, b),
            },
            Depth::Mono => {
                if c == self.accent || c == self.err || c == self.warn {
                    (Color::Reset, Modifier::BOLD)
                } else if c == self.dim || c == self.faint || c == self.comment {
                    (Color::Reset, Modifier::DIM)
                } else {
                    (Color::Reset, Modifier::empty())
                }
            }
        }
    }

    fn map_bg(&self, c: Color) -> (Color, Modifier) {
        let Color::Rgb(r, g, b) = c else {
            return (c, Modifier::empty());
        };
        match self.depth {
            Depth::True => (c, Modifier::empty()),
            Depth::Ansi256 => (Color::Indexed(self.index_of(c, r, g, b)), Modifier::empty()),
            Depth::Ansi16 | Depth::Mono => {
                // Without colour the chip is only told apart by reverse video; with 16 colours
                // its yellow foreground does the job.
                if self.depth == Depth::Mono {
                    // Attributes only, and the four marks have to differ from each other.
                    let m = if c == self.chip_bg {
                        Some(Modifier::BOLD)
                    } else if c == self.sel_bg {
                        Some(Modifier::REVERSED | Modifier::BOLD)
                    } else if c == self.cur_bg {
                        Some(Modifier::REVERSED | Modifier::BOLD | Modifier::UNDERLINED)
                    } else if c == self.accent_bg {
                        Some(Modifier::BOLD | Modifier::UNDERLINED)
                    } else {
                        None
                    };
                    if let Some(m) = m {
                        return (Color::Reset, m);
                    }
                }
                if c == self.chip_bg {
                    // No tint and no hue: bold is the whole mark (a reverse block outweighed
                    // the headings, finding 24).
                    return (Color::Reset, Modifier::BOLD);
                }
                let m = self
                    .bg16
                    .iter()
                    .find(|(t, _)| *t == c)
                    .map_or(Modifier::empty(), |(_, m)| *m);
                (Color::Reset, m)
            }
        }
    }

    #[cfg(test)]
    fn seen_index(&self, c: Color) -> Color {
        let Color::Rgb(r, g, b) = c else { return c };
        Color::Indexed(self.index_of(c, r, g, b))
    }

    fn index_of(&self, c: Color, r: u8, g: u8, b: u8) -> u8 {
        self.idx
            .iter()
            .find(|(t, _)| *t == c)
            .map_or_else(|| nearest_256(r, g, b), |(_, i)| *i)
    }
}

/// CIE L*a*b* of an sRGB colour (D65), for perceptual distances.
fn lab(r: u8, g: u8, b: u8) -> [f32; 3] {
    let lin = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
    let f = |t: f32| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// Perceptual distance (CIE76) between two sRGB colours. Good enough to rank palette entries;
/// the tests use the finer CIEDE2000 to check the token table.
fn delta_e(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let (la, lb) = (lab(a.0, a.1, a.2), lab(b.0, b.1, b.2));
    ((la[0] - lb[0]).powi(2) + (la[1] - lb[1]).powi(2) + (la[2] - lb[2]).powi(2)).sqrt()
}

/// The sRGB value xterm gives a 256-colour index from 16 up (cube and grey ramp).
pub fn index_rgb(i: u8) -> (u8, u8, u8) {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match i {
        16..=231 => {
            let n = i - 16;
            (
                STEPS[(n / 36) as usize],
                STEPS[((n / 6) % 6) as usize],
                STEPS[(n % 6) as usize],
            )
        }
        232..=255 => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
        _ => (0, 0, 0),
    }
}

/// The entry of the xterm cube or grey ramp nearest to a colour by perceptual distance. Plain
/// RGB distance picks greys for dark saturated colours and olives for greens; Lab does not.
/// Only the cube cells around the colour and the nearest greys are scored.
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let around = |v: u8| {
        let hi = STEPS.iter().position(|s| *s >= v).unwrap_or(5);
        let lo = hi.saturating_sub(1);
        [lo, hi]
    };
    let mut best = (f32::MAX, 16u8);
    let mut consider = |i: u8| {
        let d = delta_e((r, g, b), index_rgb(i));
        if d < best.0 {
            best = (d, i);
        }
    };
    for ri in around(r) {
        for gi in around(g) {
            for bi in around(b) {
                consider((16 + 36 * ri + 6 * gi + bi) as u8);
            }
        }
    }
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let grey = (((avg - 8).max(0) + 5) / 10).min(23);
    for k in [grey - 1, grey, grey + 1] {
        if (0..24).contains(&k) {
            consider((232 + k) as u8);
        }
    }
    best.1
}

/// Classify an unknown colour into one of the six hues, or the default foreground when it is
/// nearly grey. Dark neutrals get SGR 2 so they never read brighter than body text.
fn hue16(r: u8, g: u8, b: u8) -> (Color, Modifier) {
    let (rf, gf, bf) = (r as f32, g as f32, b as f32);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    if max - min < 28.0 {
        return (
            Color::Reset,
            if max < 150.0 {
                Modifier::DIM
            } else {
                Modifier::empty()
            },
        );
    }
    let d = max - min;
    let h = if max == rf {
        ((gf - bf) / d).rem_euclid(6.0)
    } else if max == gf {
        (bf - rf) / d + 2.0
    } else {
        (rf - gf) / d + 4.0
    } * 60.0;
    let c = match h as u32 {
        0..=20 | 340..=360 => Color::Red,
        21..=70 => Color::Yellow,
        71..=160 => Color::Green,
        161..=200 => Color::Cyan,
        201..=265 => Color::Blue,
        _ => Color::Magenta,
    };
    (c, Modifier::empty())
}

/// WCAG contrast ratio between two palette colours.
pub fn contrast(a: Color, b: Color) -> f64 {
    fn lum(c: Color) -> f64 {
        let Color::Rgb(r, g, b) = c else { return 0.0 };
        let f = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
    }
    let (la, lb) = (lum(a), lum(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Glyphs, with the ASCII column of the design's table behind `OPENC_ASCII`, a non-UTF-8
/// `LANG` or `TERM=linux`.
#[derive(Clone, Copy, Debug)]
pub struct Glyphs {
    pub bar: &'static str,
    pub folded: &'static str,
    pub unfolded: &'static str,
    pub spinner: &'static [&'static str],
    pub ok: &'static str,
    pub fail: &'static str,
    pub stop: &'static str,
    pub pending: &'static str,
    pub active: &'static str,
    /// A call that runs but is not the one animated spinner on screen (finding 27: `◐` also
    /// means the active todo, so two parallel calls looked paused).
    pub busy: &'static str,
    pub think: &'static str,
    pub sep: &'static str,
    pub ellipsis: &'static str,
    pub wrap: &'static str,
    pub tee: &'static str,
    pub elbow: &'static str,
    pub gauge_on: &'static str,
    pub gauge_off: &'static str,
    pub rule: &'static str,
    pub dashed: &'static str,
    pub dots: &'static str,
    /// Top edge of the dock: a lower half block, so the tray starts half a row down. Empty in
    /// the ASCII table, where the edge is a rule.
    pub edge: &'static str,
    pub down: &'static str,
    pub up: &'static str,
    pub bullet: &'static str,
    /// Bullets by list depth, cycling: the first is `bullet`.
    pub bullets: &'static [&'static str],
    pub vline: &'static str,
    pub select: &'static str,
}

pub const UNICODE: Glyphs = Glyphs {
    bar: "▎",
    folded: "▸",
    unfolded: "▾",
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    ok: "✓",
    fail: "✗",
    stop: "■",
    pending: "○",
    active: "◐",
    busy: "⠿",
    think: "◇",
    sep: "·",
    ellipsis: "…",
    wrap: "↳",
    tee: "├─",
    elbow: "└─",
    gauge_on: "━",
    gauge_off: "─",
    rule: "─",
    dashed: "╌",
    dots: "┈",
    edge: "▄",
    down: "↓",
    up: "↑",
    bullet: "•",
    bullets: &["•", "◦", "▪"],
    vline: "│",
    select: "▸",
};

pub const ASCII: Glyphs = Glyphs {
    bar: "|",
    folded: ">",
    unfolded: "v",
    spinner: &["|", "/", "-", "\\"],
    ok: "+",
    fail: "x",
    stop: "#",
    pending: "o",
    active: "*",
    busy: "*",
    think: "~",
    sep: "-",
    ellipsis: "...",
    wrap: "\\",
    tee: "+-",
    elbow: "+-",
    gauge_on: "#",
    gauge_off: "-",
    rule: "-",
    dashed: ".",
    dots: ".",
    edge: "",
    down: "v",
    up: "^",
    bullet: "*",
    bullets: &["*", "-", "+"],
    vline: "|",
    select: ">",
};

pub fn glyphs_from_env() -> Glyphs {
    glyphs_with(|k| std::env::var(k).ok())
}

/// [`glyphs_from_env`] over any variable lookup, so the rules are tested without touching the
/// process environment.
pub fn glyphs_with(get: impl Fn(&str) -> Option<String>) -> Glyphs {
    // `OPENC_AMBIGUOUS=wide` is for a terminal that draws `·` `─` `▸` two cells wide (a CJK
    // locale does): every chrome glyph that is ambiguous in width has an ASCII twin, so the
    // one switch that exists for that is the ASCII table (finding 25).
    let wide = get("OPENC_AMBIGUOUS").is_some_and(|v| v.eq_ignore_ascii_case("wide"));
    let ascii = wide
        || get("OPENC_ASCII").is_some_and(|v| !v.is_empty() && v != "0")
        || get("TERM").is_some_and(|t| t == "linux")
        || {
            let loc = ["LC_ALL", "LC_CTYPE", "LANG"]
                .iter()
                .find_map(|k| get(k).filter(|v| !v.is_empty()));
            loc.is_some_and(|l| {
                let l = l.to_ascii_lowercase();
                !(l.contains("utf-8") || l.contains("utf8"))
            })
        };
    if ascii {
        ASCII
    } else {
        UNICODE
    }
}

/// Colour depth from the environment. `tmux_rgb` is the answer to "does the tmux client
/// advertise RGB", asked by the caller only when `$TMUX` is set and `COLORTERM` is not.
pub fn depth_from_env(tmux_rgb: impl FnOnce() -> bool) -> Depth {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if var("NO_COLOR").is_some() || var("TERM").as_deref() == Some("dumb") {
        return Depth::Mono;
    }
    if matches!(var("COLORTERM").as_deref(), Some("truecolor" | "24bit")) {
        return Depth::True;
    }
    if var("TMUX").is_some() && tmux_rgb() {
        return Depth::True;
    }
    if var("TERM").is_some_and(|t| t.contains("256color")) {
        return Depth::Ansi256;
    }
    Depth::Ansi16
}

/// A terminal that echoes back a truecolor SGR (see `term::probe`) gets truecolor whatever
/// `COLORTERM` says; tmux, ssh without `SendEnv` and most remote shells drop that variable.
/// Only ever raises 256 or 16 colours to truecolor, never lowers, and leaves `NO_COLOR` alone.
pub fn upgrade_depth(from_env: Depth, caps: &crate::term::Caps) -> Depth {
    match from_env {
        Depth::Ansi256 | Depth::Ansi16 if caps.truecolor => Depth::True,
        d => d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn both() -> [Palette; 2] {
        [
            Palette::new(Kind::Hearth, Depth::True),
            Palette::new(Kind::Parchment, Depth::True),
        ]
    }

    #[test]
    fn text_tokens_clear_4_5_on_the_surfaces_they_sit_on() {
        for p in both() {
            // Statuses are only ever drawn on `bg` and `surface`; the rest also on `raised`.
            for (name, fg, on_raised) in [
                ("text", p.text, true),
                ("dim", p.dim, true),
                ("faint", p.faint, true),
                ("accent", p.accent, true),
                ("user", p.user, true),
                ("ok", p.ok, false),
                ("warn", p.warn, false),
                ("err", p.err, false),
                ("info", p.info, false),
            ] {
                for (bn, bg, needed) in [
                    ("bg", p.bg, true),
                    ("surface", p.surface, true),
                    ("raised", p.raised, on_raised),
                    // The dock shows todo text, glyphs in accent and ok, and faint hints.
                    (
                        "dock",
                        p.dock,
                        matches!(name, "text" | "dim" | "faint" | "accent" | "ok"),
                    ),
                ] {
                    let c = contrast(fg, bg);
                    assert!(!needed || c >= 4.5, "{:?} {name} on {bn}: {c:.2}", p.kind);
                }
            }
            for (name, fg, bg) in [
                ("add_fg", p.add_fg, p.add_bg),
                ("del_fg", p.del_fg, p.del_bg),
                ("text on add", p.text, p.add_bg),
                ("text on del", p.text, p.del_bg),
                ("text on add_word", p.text, p.add_word),
                ("text on del_word", p.text, p.del_word),
                ("text on accent_bg", p.text, p.accent_bg),
            ] {
                let c = contrast(fg, bg);
                assert!(c >= 4.5, "{:?} {name}: {c:.2}", p.kind);
            }
        }
    }

    #[test]
    fn the_light_scrim_keeps_the_page_readable_and_the_dialog_standing_out() {
        // Round 2 finding 4: at the default 59 percent the light page went to #676562 and its
        // text to 2.2:1. Everything behind a dialog is still real text someone may read.
        let p = Palette::new(Kind::Parchment, Depth::True);
        let t = p.theme();
        assert!((t.overlay_alpha - LIGHT_SCRIM).abs() < 1e-6);
        let scrim_bg = t.dim(p.bg);
        for (n, fg) in [("text", p.text), ("dim", p.dim), ("faint", p.faint)] {
            for (bn, bg) in [("bg", p.bg), ("surface", p.surface), ("raised", p.raised)] {
                let c = contrast(t.dim(fg), t.dim(bg));
                assert!(c >= 4.5, "{n} on {bn} under the scrim: {c:.2}");
            }
        }
        // The accent only ever sits on `bg` and `surface` as text; on `raised` it is the focus bar.
        for (bn, bg) in [("bg", p.bg), ("surface", p.surface)] {
            let c = contrast(t.dim(p.accent), t.dim(bg));
            assert!(c >= 4.5, "accent on {bn} under the scrim: {c:.2}");
        }
        // The dialog's own panel is the lightest thing on screen and clearly apart from it.
        assert!(
            contrast(p.raised, scrim_bg) >= 1.1,
            "{:.2}",
            contrast(p.raised, scrim_bg)
        );
        // The dark theme keeps the default, which round 2 called good.
        let d = Palette::new(Kind::Hearth, Depth::True).theme();
        assert!((d.overlay_alpha - 150.0 / 255.0).abs() < 1e-3);
    }

    #[test]
    fn the_dock_is_its_own_surface_at_every_depth() {
        // Round 2 finding 2: the tray had the code block's colour (1.0:1 against it). It has to
        // be told apart from `surface` and from `raised` (the composer's) in truecolor, and
        // from every other background in 256 colours.
        for p in both() {
            let vs_surface = contrast(p.dock, p.surface);
            let vs_raised = contrast(p.dock, p.raised);
            assert!(
                vs_surface >= 1.15,
                "{:?} dock/surface {vs_surface:.2}",
                p.kind
            );
            assert!(vs_raised >= 1.25, "{:?} dock/raised {vs_raised:.2}", p.kind);
            assert_ne!(p.dock, p.bg);
        }
        for kind in [Kind::Hearth, Kind::Parchment] {
            let p = Palette::new(kind, Depth::Ansi256);
            let at = |c: Color| match c {
                Color::Rgb(r, g, b) => p.index_of(c, r, g, b),
                _ => unreachable!(),
            };
            let dock = at(p.dock);
            for (n, other) in [
                ("bg", p.bg),
                ("surface", p.surface),
                ("raised", p.raised),
                ("chip", p.chip_bg),
                ("accent_bg", p.accent_bg),
            ] {
                assert_ne!(
                    dock,
                    at(other),
                    "{kind:?} 256: dock shares an index with {n}"
                );
            }
            // And by luminance, in the colours the terminal will really draw.
            let ratio = |a: u8, b: u8| contrast(xterm(a), xterm(b));
            assert!(
                ratio(dock, at(p.surface)) >= 1.15,
                "{kind:?} 256 dock/surface"
            );
            assert!(
                ratio(dock, at(p.raised)) >= 1.25,
                "{kind:?} 256 dock/raised"
            );
        }
    }

    #[test]
    fn diff_rows_keep_text_at_ten_to_one() {
        for p in both() {
            for bg in [p.add_bg, p.del_bg] {
                assert!(contrast(p.text, bg) >= 10.0, "{:?}", p.kind);
            }
        }
    }

    #[test]
    fn dark_background_is_not_black_and_text_is_not_white() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        assert_eq!(p.bg, Color::Rgb(0x14, 0x11, 0x0f));
        assert_ne!(p.text, Color::Rgb(255, 255, 255));
    }

    #[test]
    fn the_ambiguous_width_switch_selects_the_ascii_table() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        let g = glyphs_with(env(&[("LANG", "en_US.UTF-8")]));
        assert_eq!(g.sep, "·");
        let g = glyphs_with(env(&[("LANG", "ja_JP.UTF-8"), ("OPENC_AMBIGUOUS", "wide")]));
        assert_eq!((g.sep, g.rule, g.select), ("-", "-", ">"));
        // Other values leave the Unicode table alone.
        let g = glyphs_with(env(&[
            ("LANG", "en_US.UTF-8"),
            ("OPENC_AMBIGUOUS", "narrow"),
        ]));
        assert_eq!(g.sep, "·");
    }

    #[test]
    fn every_glyph_is_one_cell_wide() {
        for g in [UNICODE, ASCII] {
            let mut all = vec![
                g.bar,
                g.folded,
                g.unfolded,
                g.ok,
                g.fail,
                g.stop,
                g.pending,
                g.active,
                g.busy,
                g.think,
                g.sep,
                g.wrap,
                g.gauge_on,
                g.gauge_off,
                g.rule,
                g.dashed,
                g.dots,
                g.down,
                g.up,
                g.bullet,
                g.vline,
                g.select,
            ];
            all.extend(g.spinner.iter());
            all.extend(g.bullets.iter());
            all.extend(Some(g.edge).filter(|e| !e.is_empty()));
            for s in all {
                assert_eq!(s.width(), 1, "{s:?}");
            }
        }
    }

    fn frame(p: &Palette) -> Buffer {
        let mut b = Buffer::empty(ratatui::layout::Rect::new(0, 0, 4, 1));
        b[(0, 0)].set_style(Style::new().fg(p.dim).bg(p.raised));
        b[(1, 0)].set_style(Style::new().fg(p.accent).bg(p.add_word));
        b[(2, 0)].set_style(Style::new().fg(Color::Rgb(10, 200, 20)));
        b
    }

    #[test]
    fn ansi256_uses_the_design_indices() {
        let p = Palette::new(Kind::Hearth, Depth::Ansi256);
        let mut b = frame(&p);
        p.downgrade(&mut b);
        assert_eq!(b[(0, 0)].fg, Color::Indexed(250));
        assert_eq!(b[(0, 0)].bg, Color::Indexed(236));
        assert_eq!(b[(1, 0)].fg, Color::Indexed(79));
        assert_eq!(b[(1, 0)].bg, Color::Indexed(240));
    }

    #[test]
    fn no_256_colour_diff_background_is_a_saturated_primary() {
        // Finding 1: 22 (#005f00) and 52 (#5f0000) as full-width rows were the neon blocks the
        // design calls Claude Code's worst habit. Rows go grey, only the changed words keep a hue.
        let p = Palette::new(Kind::Hearth, Depth::Ansi256);
        let mut b = Buffer::empty(ratatui::layout::Rect::new(0, 0, 4, 1));
        b[(0, 0)].set_style(Style::new().bg(p.add_bg));
        b[(1, 0)].set_style(Style::new().bg(p.del_bg));
        p.downgrade(&mut b);
        for x in 0..2 {
            let Color::Indexed(i) = b[(x, 0)].bg else {
                panic!("not indexed");
            };
            assert!((232..=239).contains(&i), "row background {i} is not a grey");
        }
        // The words are no longer a hue at all in 256 colours (see the table note); they are
        // a grey lift, so none of the four is a saturated primary.
        for c in [p.add_word, p.del_word] {
            let Color::Indexed(i) = p.seen_index(c) else {
                panic!("not indexed")
            };
            assert!(
                (232..=255).contains(&i),
                "word background {i} is a cube colour"
            );
        }
    }

    /// CIEDE2000 between two sRGB triples, the reference the 256-colour table is checked against.
    fn de2000(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
        let l = |c: (u8, u8, u8)| {
            let [l, a, b] = lab(c.0, c.1, c.2);
            (l as f64, a as f64, b as f64)
        };
        let ((l1, a1, b1), (l2, a2, b2)) = (l(a), l(b));
        let (c1, c2) = (a1.hypot(b1), a2.hypot(b2));
        let cm = (c1 + c2) / 2.0;
        let g = 0.5 * (1.0 - (cm.powi(7) / (cm.powi(7) + 25f64.powi(7))).sqrt());
        let (a1p, a2p) = ((1.0 + g) * a1, (1.0 + g) * a2);
        let (c1p, c2p) = (a1p.hypot(b1), a2p.hypot(b2));
        let hue = |b: f64, a: f64| b.atan2(a).to_degrees().rem_euclid(360.0);
        let (h1, h2) = (hue(b1, a1p), hue(b2, a2p));
        let (dl, dc) = (l2 - l1, c2p - c1p);
        let dh = if c1p * c2p == 0.0 {
            0.0
        } else if (h2 - h1).abs() <= 180.0 {
            h2 - h1
        } else if h2 - h1 > 180.0 {
            h2 - h1 - 360.0
        } else {
            h2 - h1 + 360.0
        };
        let dh_big = 2.0 * (c1p * c2p).sqrt() * (dh.to_radians() / 2.0).sin();
        let lm = (l1 + l2) / 2.0;
        let cmp = (c1p + c2p) / 2.0;
        let hm = if c1p * c2p == 0.0 {
            h1 + h2
        } else if (h1 - h2).abs() <= 180.0 {
            (h1 + h2) / 2.0
        } else if h1 + h2 < 360.0 {
            (h1 + h2 + 360.0) / 2.0
        } else {
            (h1 + h2 - 360.0) / 2.0
        };
        let t = 1.0 - 0.17 * (hm - 30.0).to_radians().cos()
            + 0.24 * (2.0 * hm).to_radians().cos()
            + 0.32 * (3.0 * hm + 6.0).to_radians().cos()
            - 0.20 * (4.0 * hm - 63.0).to_radians().cos();
        let sl = 1.0 + 0.015 * (lm - 50.0).powi(2) / (20.0 + (lm - 50.0).powi(2)).sqrt();
        let sc = 1.0 + 0.045 * cmp;
        let sh = 1.0 + 0.015 * cmp * t;
        let dth = 30.0 * (-((hm - 275.0) / 25.0).powi(2)).exp();
        let rc = 2.0 * (cmp.powi(7) / (cmp.powi(7) + 25f64.powi(7))).sqrt();
        let rt = -(2.0 * dth).to_radians().sin() * rc;
        ((dl / sl).powi(2)
            + (dc / sc).powi(2)
            + (dh_big / sh).powi(2)
            + rt * (dc / sc) * (dh_big / sh))
            .sqrt()
    }

    fn rgb_of(c: Color) -> (u8, u8, u8) {
        let Color::Rgb(r, g, b) = c else {
            panic!("{c:?} is not truecolor")
        };
        (r, g, b)
    }

    #[test]
    fn the_256_table_is_the_perceptually_nearest_index_unless_a_stated_constraint_says_no() {
        // Every token's index, scored against every entry of the cube and the grey ramp by
        // CIEDE2000. A token may sit up to 3 units behind the best entry. The ones that sit
        // further are the diff tints in the dark theme, and each has its reason in the table.
        let mut worst = Vec::new();
        for kind in [Kind::Hearth, Kind::Parchment] {
            let p = Palette::new(kind, Depth::Ansi256);
            let exempt = |c: Color| {
                kind == Kind::Hearth && [p.add_bg, p.del_bg, p.add_word, p.del_word].contains(&c)
            };
            for (c, i) in &p.idx {
                if exempt(*c) {
                    continue;
                }
                let t = rgb_of(*c);
                let got = de2000(t, index_rgb(*i));
                let best = (16..=255u8)
                    .map(|k| de2000(t, index_rgb(k)))
                    .fold(f64::MAX, f64::min);
                if got > best + 3.0 {
                    worst.push(format!(
                        "{kind:?} {c:?} index {i}: dE {got:.1}, best {best:.1}"
                    ));
                }
            }
        }
        assert!(worst.is_empty(), "{}", worst.join("\n"));
    }

    #[test]
    fn dark_256_diff_marks_are_told_apart_and_stay_readable_numerically() {
        let p = Palette::new(Kind::Hearth, Depth::Ansi256);
        let at = |c: Color| {
            let (r, g, b) = rgb_of(c);
            index_rgb(p.index_of(c, r, g, b))
        };
        let (surface, row_a, row_d) = (at(p.surface), at(p.add_bg), at(p.del_bg));
        let (word_a, word_d) = (at(p.add_word), at(p.del_word));
        // A changed row is visibly not a context row; a changed word is visibly not its row.
        assert!(de2000(row_a, surface) >= 5.0 && de2000(row_d, surface) >= 5.0);
        assert!(de2000(word_a, row_a) >= 9.0 && de2000(word_d, row_d) >= 9.0);
        // Text and the syntax colours that carry code stay legible on all of them. Comments
        // are left out: the 256 grey for `faint` is 4.2:1 on the row grey and the table has
        // no lighter entry that is still a comment colour.
        let rgb = |t: (u8, u8, u8)| Color::Rgb(t.0, t.1, t.2);
        for (n, on) in [
            ("row", row_a),
            ("row", row_d),
            ("word", word_a),
            ("word", word_d),
        ] {
            for (name, fg, floor) in [
                ("text", p.text, if n == "row" { 8.5 } else { 4.5 }),
                // Inside a changed word anything under 4.5 is swapped for `text` (styled_code).
                ("dim", p.dim, if n == "row" { 4.5 } else { 3.0 }),
            ] {
                let c = contrast(rgb(at(fg)), rgb(on));
                assert!(c >= floor, "{name} on {n} {on:?}: {c:.2}");
            }
            if n == "row" {
                for (name, fg) in [
                    ("kw", p.kw),
                    ("str", p.str_),
                    ("fn", p.func),
                    ("num", p.num),
                ] {
                    let c = contrast(rgb(at(fg)), rgb(on));
                    assert!(c >= 4.5, "{name} on {n} {on:?}: {c:.2}");
                }
            }
        }
        // The changed word's legible fallback, `text`, clears 4.5 on the word grey.
        assert!(contrast(rgb(at(p.text)), rgb(word_a)) >= 4.5);
    }

    #[test]
    fn text_drawn_on_the_selection_step_clears_4_5() {
        // Finding 23: `faint` on `line` (the selected row) is 4.06:1; the arrows, the meta line
        // and the `current` tag on a selected row use `dim`, which this pins.
        for p in both() {
            assert!(contrast(p.dim, p.line) >= 4.5, "{:?}", p.kind);
            assert!(contrast(p.text, p.line) >= 4.5, "{:?}", p.kind);
            assert!(contrast(p.faint, p.bg) >= 4.5 && contrast(p.faint, p.surface) >= 4.5);
        }
    }

    #[test]
    fn surfaces_step_far_enough_from_the_background_to_show() {
        // Finding 12: surface was 1.06:1 on bg, so tool bodies and the rail did not read as
        // regions. These are floors, and the text pairs are covered by the 4.5 test above.
        for p in both() {
            assert!(contrast(p.surface, p.bg) >= 1.12, "{:?} surface", p.kind);
            assert!(contrast(p.raised, p.bg) >= 1.25, "{:?} raised", p.kind);
            assert!(
                contrast(p.raised, p.surface) >= 1.08,
                "{:?} raised/surface",
                p.kind
            );
        }
    }

    #[test]
    fn ansi16_drops_tints_and_never_uses_bright_black() {
        let p = Palette::new(Kind::Hearth, Depth::Ansi16);
        let mut b = frame(&p);
        p.downgrade(&mut b);
        assert_eq!(b[(0, 0)].fg, Color::Reset);
        assert!(b[(0, 0)].modifier.contains(Modifier::DIM));
        assert_eq!(b[(0, 0)].bg, Color::Reset);
        assert_eq!(b[(1, 0)].fg, Color::Cyan);
        assert!(b[(1, 0)]
            .modifier
            .contains(Modifier::BOLD | Modifier::UNDERLINED));
        assert_eq!(b[(2, 0)].fg, Color::Green);
        for c in b.content() {
            assert!(!matches!(c.fg, Color::DarkGray | Color::Indexed(8)));
        }
    }

    #[test]
    fn mono_keeps_only_attributes() {
        let p = Palette::new(Kind::Hearth, Depth::Mono);
        let mut b = frame(&p);
        p.downgrade(&mut b);
        for c in b.content() {
            assert_eq!((c.fg, c.bg), (Color::Reset, Color::Reset));
        }
        assert!(b[(1, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn depth_detection() {
        // Only the pure parts are testable without touching the process environment.
        assert_eq!(nearest_256(0x14, 0x11, 0x0f), 233);
        assert!(matches!(hue16(200, 40, 30).0, Color::Red));
        assert!(matches!(hue16(40, 200, 60).0, Color::Green));
    }

    /// xterm's value for a 256-colour index (cube and grey ramp only).
    fn xterm(i: u8) -> Color {
        match i {
            16..=231 => {
                let n = i - 16;
                let s = [0u8, 95, 135, 175, 215, 255];
                Color::Rgb(
                    s[(n / 36) as usize],
                    s[((n / 6) % 6) as usize],
                    s[(n % 6) as usize],
                )
            }
            232..=255 => {
                let v = 8 + 10 * (i - 232);
                Color::Rgb(v, v, v)
            }
            _ => panic!("index {i} is a terminal-defined colour"),
        }
    }

    fn idx(p: &Palette, c: Color) -> Color {
        let Color::Rgb(r, g, b) = c else { return c };
        xterm(p.index_of(c, r, g, b))
    }

    #[test]
    fn light_theme_text_roles_clear_wcag_4_5_numerically() {
        // The same roles and surfaces as the design table, plus the roles added since: the
        // inline code chip, the selection, and the search marks. Run with --nocapture to see
        // the numbers.
        let p = Palette::new(Kind::Parchment, Depth::True);
        let mut rows = Vec::new();
        let mut check = |name: &str, fg: Color, bg: Color, min: f64| {
            let c = contrast(fg, bg);
            rows.push(format!("{name:<22} {c:5.2}"));
            assert!(c >= min, "parchment {name}: {c:.2} < {min}");
        };
        for (n, fg) in [
            ("text", p.text),
            ("dim", p.dim),
            ("faint", p.faint),
            ("accent", p.accent),
            ("user", p.user),
            ("ok", p.ok),
            ("warn", p.warn),
            ("err", p.err),
            ("info", p.info),
            ("kw", p.kw),
            ("str", p.str_),
            ("num", p.num),
            ("fn", p.func),
            ("type", p.ty),
            ("comment", p.comment),
        ] {
            check(&format!("{n} on bg"), fg, p.bg, 4.5);
            check(&format!("{n} on surface"), fg, p.surface, 4.5);
        }
        for (n, fg) in [
            ("text", p.text),
            ("dim", p.dim),
            ("faint", p.faint),
            ("accent", p.accent),
            ("user", p.user),
        ] {
            check(&format!("{n} on raised"), fg, p.raised, 4.5);
        }
        check("chip_fg on chip_bg", p.chip_fg, p.chip_bg, 7.0);
        check("chip_fg on bg", p.chip_fg, p.bg, 4.5);
        check("text on selection", p.text, p.sel_bg, 7.0);
        check("text on current match", p.text, p.cur_bg, 7.0);
        check("text on other matches", p.text, p.accent_bg, 7.0);
        check("add_fg on add_bg", p.add_fg, p.add_bg, 4.5);
        check("del_fg on del_bg", p.del_fg, p.del_bg, 4.5);
        println!("{}", rows.join("\n"));
    }

    #[test]
    fn syntax_colours_stay_readable_on_the_diff_tints_in_both_themes() {
        for p in both() {
            for (n, fg) in [
                ("kw", p.kw),
                ("str", p.str_),
                ("num", p.num),
                ("fn", p.func),
                ("type", p.ty),
                ("comment", p.comment),
                ("dim", p.dim),
            ] {
                for (bn, bg) in [("add_bg", p.add_bg), ("del_bg", p.del_bg)] {
                    let c = contrast(fg, bg);
                    assert!(c >= 4.5, "{:?} {n} on {bn}: {c:.2}", p.kind);
                }
            }
        }
    }

    #[test]
    fn dark_theme_marks_clear_their_floors() {
        let p = Palette::new(Kind::Hearth, Depth::True);
        assert!(contrast(p.chip_fg, p.chip_bg) >= 7.0);
        assert!(contrast(p.chip_fg, p.bg) >= 7.0);
        assert!(contrast(p.text, p.sel_bg) >= 7.0);
        assert!(contrast(p.text, p.cur_bg) >= 4.5);
        assert!(contrast(p.text, p.accent_bg) >= 7.0);
        // The marks are tints of the surface, not a second background colour: all of them are
        // told apart from the page and from each other.
        let marks = [p.chip_bg, p.sel_bg, p.cur_bg, p.accent_bg, p.raised];
        for (i, a) in marks.iter().enumerate() {
            assert_ne!(*a, p.bg);
            for b in &marks[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn ansi256_roles_clear_4_5_on_the_surfaces_they_sit_on() {
        for kind in [Kind::Hearth, Kind::Parchment] {
            let p = Palette::new(kind, Depth::Ansi256);
            let (bg, surface, raised) = (idx(&p, p.bg), idx(&p, p.surface), idx(&p, p.raised));
            for (name, fg, on_raised) in [
                ("text", p.text, true),
                ("dim", p.dim, true),
                ("faint", p.faint, true),
                ("accent", p.accent, true),
                ("user", p.user, true),
                ("ok", p.ok, false),
                ("warn", p.warn, false),
                ("err", p.err, false),
                ("info", p.info, false),
                ("kw", p.kw, false),
                ("str", p.str_, false),
                ("num", p.num, false),
                ("fn", p.func, false),
                ("type", p.ty, false),
                ("comment", p.comment, false),
            ] {
                let f = idx(&p, fg);
                for (bn, b, needed) in [
                    ("bg", bg, true),
                    ("surface", surface, true),
                    ("raised", raised, on_raised),
                ] {
                    let c = contrast(f, b);
                    assert!(!needed || c >= 4.5, "{kind:?} 256 {name} on {bn}: {c:.2}");
                }
            }
            for (name, fg, b) in [
                ("chip_fg on chip_bg", p.chip_fg, p.chip_bg),
                ("text on selection", p.text, p.sel_bg),
                ("text on current match", p.text, p.cur_bg),
                ("text on other matches", p.text, p.accent_bg),
                ("text on add_bg", p.text, p.add_bg),
                ("text on del_bg", p.text, p.del_bg),
                ("add_fg on add_bg", p.add_fg, p.add_bg),
                ("del_fg on del_bg", p.del_fg, p.del_bg),
            ] {
                let c = contrast(idx(&p, fg), idx(&p, b));
                assert!(c >= 4.5, "{kind:?} 256 {name}: {c:.2}");
            }
            // The tints must also be told apart from the page.
            for (name, t) in [
                ("chip", p.chip_bg),
                ("selection", p.sel_bg),
                ("match", p.accent_bg),
                ("current", p.cur_bg),
            ] {
                assert!(
                    contrast(idx(&p, t), bg) >= 1.1,
                    "{kind:?} 256 {name} tint is invisible on bg"
                );
            }
        }
    }

    /// `(fg, bg, modifiers)` a cell with these colours ends up with at `depth`.
    fn drawn(p: &Palette, fg: Color, bg: Color) -> (Color, Color, Modifier) {
        let mut b = Buffer::empty(ratatui::layout::Rect::new(0, 0, 1, 1));
        b[(0, 0)].set_style(Style::new().fg(fg).bg(bg));
        p.downgrade(&mut b);
        let c = &b[(0, 0)];
        (c.fg, c.bg, c.modifier)
    }

    #[test]
    fn every_role_is_told_apart_at_256_and_16_colours() {
        for kind in [Kind::Hearth, Kind::Parchment] {
            for depth in [Depth::Ansi256, Depth::Ansi16] {
                let base = Palette::new(kind, Depth::True);
                let p = Palette::new(kind, depth);
                let on = |fg| drawn(&p, fg, base.bg);
                // Semantic colours must differ from each other and from body text.
                let named = [
                    ("text", on(base.text)),
                    ("accent", on(base.accent)),
                    ("user", on(base.user)),
                    ("ok", on(base.ok)),
                    ("warn", on(base.warn)),
                    ("err", on(base.err)),
                    ("info", on(base.info)),
                ];
                for (i, (an, a)) in named.iter().enumerate() {
                    for (bn, b) in &named[i + 1..] {
                        assert_ne!(a, b, "{kind:?} {depth:?}: {an} and {bn} look the same");
                    }
                }
                // Text tiers: body, secondary, and the inline code chip.
                assert_ne!(
                    on(base.text),
                    on(base.dim),
                    "{kind:?} {depth:?}: text vs dim"
                );
                let chip = drawn(&p, base.chip_fg, base.chip_bg);
                assert_ne!(chip, on(base.text), "{kind:?} {depth:?}: chip vs text");
                // Diff rows: sign colour or word emphasis differs from plain text.
                assert_ne!(
                    drawn(&p, base.add_fg, base.add_bg),
                    drawn(&p, base.del_fg, base.del_bg)
                );
                assert_ne!(
                    drawn(&p, base.text, base.add_word),
                    drawn(&p, base.text, base.add_bg),
                    "{kind:?} {depth:?}: changed words vs the rest of the line"
                );
                // Marks: selection, current match, other matches, plain.
                let marks = [
                    ("plain", drawn(&p, base.text, base.bg)),
                    ("selection", drawn(&p, base.text, base.sel_bg)),
                    ("current", drawn(&p, base.text, base.cur_bg)),
                    ("match", drawn(&p, base.text, base.accent_bg)),
                ];
                for (i, (an, a)) in marks.iter().enumerate() {
                    for (bn, b) in &marks[i + 1..] {
                        assert_ne!(a, b, "{kind:?} {depth:?}: {an} and {bn} look the same");
                    }
                }
            }
        }
    }

    #[test]
    fn no_color_marks_differ_by_attribute_and_states_by_glyph() {
        let base = Palette::new(Kind::Hearth, Depth::True);
        let p = Palette::new(Kind::Hearth, Depth::Mono);
        let marks = [
            drawn(&p, base.text, base.bg),
            drawn(&p, base.chip_fg, base.chip_bg),
            drawn(&p, base.text, base.sel_bg),
            drawn(&p, base.text, base.cur_bg),
            drawn(&p, base.text, base.accent_bg),
        ];
        for (i, a) in marks.iter().enumerate() {
            assert_eq!((a.0, a.1), (Color::Reset, Color::Reset));
            for b in &marks[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Without colour, state rides on the glyph: every state glyph is its own character.
        for g in [UNICODE, ASCII] {
            let states = [
                g.ok, g.fail, g.stop, g.pending, g.active, g.think, g.folded, g.unfolded, g.bar,
            ];
            for (i, a) in states.iter().enumerate() {
                for b in &states[i + 1..] {
                    assert_ne!(a, b, "two state glyphs are both {a:?}");
                }
            }
        }
    }

    #[test]
    fn theme_builds_for_both_kinds() {
        for p in both() {
            let t = p.theme();
            assert_eq!(t.text, p.text);
            assert_eq!(t.syntax_keyword, p.kw);
        }
    }
}
