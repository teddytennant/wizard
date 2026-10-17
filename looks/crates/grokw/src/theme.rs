// OWNER: theme (all six palettes; slots are the spec 10.3 table)
//! Grok Build's colour slots (spec section 10.3) resolved to RGB for the six themes, and the
//! blend every derived colour goes through.

use ratatui::style::{Color, Modifier};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn hex(v: u32) -> Rgb {
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    pub const fn color(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }
}

/// `round(base*(1-a) + over*a)` per channel, half away from zero, as `blend_channel` in grok.
pub fn blend(base: Color, over: Color, a: f32) -> Color {
    let (Color::Rgb(br, bg, bb), Color::Rgb(or, og, ob)) = (base, over) else {
        return over;
    };
    let ch = |b: u8, o: u8| -> u8 {
        let v = b as f32 * (1.0 - a) + o as f32 * a;
        v.round().clamp(0.0, 255.0) as u8
    };
    Color::Rgb(ch(br, or), ch(bg, og), ch(bb, ob))
}

/// Linear interpolation between two colours at `t` in 0..=1, rounded per channel.
pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    blend(a, b, t.clamp(0.0, 1.0))
}

pub fn rgb_of(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    }
}

macro_rules! theme {
    ($($field:ident),* $(,)?) => {
        #[derive(Clone, Debug)]
        pub struct Theme {
            pub name: &'static str,
            pub kind: Kind,
            pub dark: bool,
            /// The `terminal` theme and `NO_COLOR` paint no opaque cell: selection and hover are
            /// reverse video.
            pub bandless: bool,
            /// Style of heading levels 1 to 6.
            pub heading: [Modifier; 6],
            $(pub $field: Color,)*
        }
    };
}

/// The six palettes `/theme` cycles through (spec 10.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Groknight,
    Grokday,
    Tokyonight,
    RosePineMoon,
    OscuraMidnight,
    Terminal,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Groknight,
        Kind::Grokday,
        Kind::Tokyonight,
        Kind::RosePineMoon,
        Kind::OscuraMidnight,
        Kind::Terminal,
    ];

    pub fn canonical(self) -> &'static str {
        match self {
            Kind::Groknight => "groknight",
            Kind::Grokday => "grokday",
            Kind::Tokyonight => "tokyonight",
            Kind::RosePineMoon => "rosepine-moon",
            Kind::OscuraMidnight => "oscura-midnight",
            Kind::Terminal => "terminal",
        }
    }

    /// What the `✓ Theme: ...` toast and `/settings` print. Oscura has no pretty name.
    pub fn pretty(self) -> &'static str {
        match self {
            Kind::Groknight => "Grok Night",
            Kind::Grokday => "Grok Day",
            Kind::Tokyonight => "Tokyo Night",
            Kind::RosePineMoon => "Rose Pine Moon",
            Kind::OscuraMidnight => "oscura-midnight",
            Kind::Terminal => "Terminal",
        }
    }

    /// Case-insensitive name or alias.
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "groknight" | "grok-night" | "dark" => Kind::Groknight,
            "grokday" | "grok-day" | "light" | "day" => Kind::Grokday,
            "tokyonight" | "tokyo-night" | "tokyo" => Kind::Tokyonight,
            "rosepine-moon" | "rosepine" | "rose-pine" | "rose-pine-moon" => Kind::RosePineMoon,
            "oscura-midnight" | "oscura" => Kind::OscuraMidnight,
            "terminal" | "terminal-default" | "transparent" | "native" => Kind::Terminal,
            _ => return None,
        })
    }

    /// Which of the three tmThemes colours code.
    pub fn syntax(self) -> Syntax {
        match self {
            Kind::Grokday => Syntax::Day,
            Kind::Tokyonight => Syntax::Tokyo,
            Kind::Terminal => Syntax::Ansi,
            _ => Syntax::Night,
        }
    }
}

/// Code colouring: `grok-night`, `grok-day`, `tokyo-night`, or grok-night mapped onto ANSI by hue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Syntax {
    Night,
    Day,
    Tokyo,
    Ansi,
}

const B: Modifier = Modifier::BOLD;
const N: Modifier = Modifier::empty();
const BU: Modifier = Modifier::BOLD.union(Modifier::UNDERLINED);
const BI: Modifier = Modifier::BOLD.union(Modifier::ITALIC);

theme!(
    bg_base,
    bg_light,
    bg_dark,
    bg_highlight,
    bg_hover,
    bg_visual,
    accent_user,
    accent_thinking,
    accent_tool,
    accent_system,
    accent_error,
    accent_success,
    accent_running,
    accent_skill,
    text_primary,
    text_secondary,
    gray_dim,
    gray,
    gray_bright,
    command,
    path,
    running,
    warning,
    fuzzy_accent,
    accent_plan,
    accent_verify,
    accent_remember,
    selection_border,
    hover_border,
    prompt_border,
    prompt_border_active,
    scrollbar_bg,
    scrollbar_fg,
    diff_delete_bg,
    diff_delete_fg,
    diff_insert_bg,
    diff_insert_fg,
    diff_equal_fg,
    diff_gutter_fg,
    paste_bg,
    paste_fg,
    paste_dim,
    md_h1,
    md_h2,
    md_h3,
    md_h4,
    md_h5,
    md_h6,
    md_code,
    md_task_checked,
    md_task_unchecked,
    md_muted,
    md_code_bg,
    md_text,
    link_fg,
    backtick,
    placeholder,
    hero_border,
    selection_fg,
    selection_bg,
);

/// The `terminal` theme is behind a rollout gate in Grok Build; `GROK_TERMINAL_THEME=1` opens it.
pub fn terminal_gate() -> bool {
    std::env::var("GROK_TERMINAL_THEME")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// Dark or light, from `GROK_APPEARANCE` then `COLORFGBG` (its last field: 0-6 and 8 dark, 7 and
/// 9-15 light). Nothing answering means dark.
pub fn appearance_is_dark() -> bool {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(v) = env("GROK_APPEARANCE").or_else(|| env("LC_GROK_APPEARANCE")) {
        match v.trim().to_ascii_lowercase().as_str() {
            "dark" | "night" => return true,
            "light" | "day" => return false,
            _ => {}
        }
    }
    if let Some(n) = env("COLORFGBG").and_then(|v| v.rsplit(';').next()?.parse::<u8>().ok()) {
        return matches!(n, 0..=6 | 8);
    }
    true
}

fn c(v: u32) -> Color {
    Rgb::hex(v).color()
}

/// Stands for "terminal default colour, dim" in the `terminal` theme, where secondary text is
/// SGR 2 rather than a grey. `ui::draw` rewrites it just before the frame goes out.
pub const DIM_FG: Color = Color::Rgb(1, 2, 255);

/// `Rgb(1, 2, n)` with `n` below 255 is indexed colour `n`, dim.
pub fn dim_indexed(n: u8) -> Color {
    Color::Rgb(1, 2, n)
}

/// A named terminal colour. Grok's fullscreen binary writes these as `38;5;n`.
const fn named(n: u8) -> Color {
    Color::Indexed(n)
}

impl Theme {
    pub fn groknight() -> Theme {
        let mut t = Theme {
            name: "groknight",
            kind: Kind::Groknight,
            dark: true,
            bandless: false,
            heading: [B, B, B, B, B, N],
            bg_base: c(0x141414),
            bg_light: c(0x242424),
            bg_dark: c(0x1c1c1c),
            bg_highlight: c(0x242424),
            bg_hover: c(0x2c2c2c),
            bg_visual: c(0x363636),
            accent_user: c(0xc8c8c8),
            accent_thinking: c(0xbb9af7),
            accent_tool: c(0x787878),
            accent_system: c(0x7aa2f7),
            accent_error: c(0xf7768e),
            accent_success: c(0x9ece6a),
            accent_running: c(0xbb9af7),
            accent_skill: c(0x7aa2f7),
            text_primary: c(0xe1e1e1),
            text_secondary: c(0xc8c8c8),
            gray_dim: c(0x585858),
            gray: c(0x6c6c6c),
            gray_bright: c(0x787878),
            command: c(0xe0af68),
            path: c(0xff9e64),
            running: c(0x7dcfff),
            warning: c(0xe0af68),
            fuzzy_accent: c(0x7aa2f7),
            accent_plan: c(0xffdb8d),
            accent_verify: c(0xbb9af7),
            accent_remember: c(0x8bc34a),
            selection_border: c(0x3c3c41),
            hover_border: c(0x1e1e22),
            prompt_border: c(0x323237),
            prompt_border_active: c(0x505058),
            scrollbar_bg: c(0x111111),
            scrollbar_fg: c(0x242424),
            diff_delete_bg: c(0x420e14),
            diff_delete_fg: c(0xf7768e),
            diff_insert_bg: c(0x063806),
            diff_insert_fg: c(0x9ece6a),
            diff_equal_fg: c(0x6c6c6c),
            diff_gutter_fg: c(0x6c6c6c),
            paste_bg: c(0x111111),
            paste_fg: c(0xc8c8c8),
            paste_dim: c(0x414141),
            md_h1: c(0x1abc9c),
            md_h2: c(0x7aa2f7),
            md_h3: c(0x9d7cd8),
            md_h4: c(0x787878),
            md_h5: c(0x6c6c6c),
            md_h6: c(0x5a5a5a),
            md_code: c(0x3a95ab),
            md_task_checked: c(0x9ece6a),
            md_task_unchecked: c(0xc8c8c8),
            md_muted: c(0x6c6c6c),
            md_code_bg: c(0x1c1c1c),
            md_text: c(0xc8c8c8),
            link_fg: c(0x7aa6da),
            backtick: c(0x9abdf5),
            placeholder: c(0x4e4e4e),
            hero_border: Color::Reset,
            selection_fg: c(0xc0caf5),
            selection_bg: c(0x313e73),
        };
        // blend(bg_base, gray_dim, 0.45); measured #333333 on groknight
        t.hero_border = blend(t.bg_base, t.gray_dim, 0.45);
        t
    }

    pub fn grokday() -> Theme {
        let mut t = Theme {
            name: "grokday",
            kind: Kind::Grokday,
            dark: false,
            bandless: false,
            heading: [B, B, B, B, B, N],
            bg_base: c(0xeeeeee),
            bg_light: c(0xdedede),
            bg_dark: c(0xe4e4e4),
            bg_highlight: c(0xdedede),
            bg_hover: c(0xd0d0d0),
            bg_visual: c(0xc6c6c6),
            accent_user: c(0x444444),
            accent_thinking: c(0x7d4bc6),
            accent_tool: c(0x626262),
            accent_system: c(0x2f64d2),
            accent_error: c(0xcd3048),
            accent_success: c(0x378e23),
            accent_running: c(0x7d4bc6),
            accent_skill: c(0x2f64d2),
            text_primary: c(0x262626),
            text_secondary: c(0x444444),
            gray_dim: c(0xa5a5a5),
            gray: c(0x767676),
            gray_bright: c(0x626262),
            command: c(0xa27612),
            path: c(0xc3691e),
            running: c(0x0082aa),
            warning: c(0xa27612),
            fuzzy_accent: c(0x2f64d2),
            accent_plan: c(0xa8780a),
            accent_verify: c(0x7850a0),
            accent_remember: c(0x4caf50),
            selection_border: c(0xb9b9be),
            hover_border: c(0xd4d4d8),
            prompt_border: c(0xc8c8cd),
            prompt_border_active: c(0xa5a5af),
            scrollbar_bg: c(0xeaeaea),
            scrollbar_fg: c(0xdedede),
            diff_delete_bg: c(0xf5dade),
            diff_delete_fg: c(0xcd3048),
            diff_insert_bg: c(0xdaf2dc),
            diff_insert_fg: c(0x378e23),
            diff_equal_fg: c(0x767676),
            diff_gutter_fg: c(0x767676),
            paste_bg: c(0xdedede),
            paste_fg: c(0x444444),
            paste_dim: c(0xb2b2b2),
            md_h1: c(0x0a8e70),
            md_h2: c(0x2f64d2),
            md_h3: c(0x6c3eb2),
            md_h4: c(0x626262),
            md_h5: c(0x767676),
            md_h6: c(0x8e8e8e),
            md_code: c(0x0f87a2),
            md_task_checked: c(0x378e23),
            md_task_unchecked: c(0x444444),
            md_muted: c(0x767676),
            md_code_bg: c(0xe4e4e4),
            md_text: c(0x444444),
            link_fg: c(0x2f64d2),
            backtick: c(0x4a72b0),
            placeholder: c(0x9f9f9f),
            hero_border: Color::Reset,
            selection_fg: c(0xc0caf5),
            selection_bg: c(0x313e73),
        };
        // blend(bg_base, gray_dim, 0.45); measured #333333 on groknight
        t.hero_border = blend(t.bg_base, t.gray_dim, 0.45);
        t
    }

    pub fn tokyonight() -> Theme {
        let mut t = Theme {
            name: "tokyonight",
            kind: Kind::Tokyonight,
            dark: true,
            bandless: false,
            heading: [B, B, B, B, B, B],
            bg_base: c(0x24283b),
            bg_light: c(0x292e42),
            bg_dark: c(0x292e42),
            bg_highlight: c(0x292e42),
            bg_hover: c(0x28314c),
            bg_visual: c(0x283457),
            accent_user: c(0x7aa2f7),
            accent_thinking: c(0x3b4261),
            accent_tool: c(0x737aa2),
            accent_system: c(0x7aa2f7),
            accent_error: c(0xf7768e),
            accent_success: c(0x9ece6a),
            accent_running: c(0xbb9af7),
            accent_skill: c(0x64b4aa),
            text_primary: c(0xc0caf5),
            text_secondary: c(0xa9b1d6),
            gray_dim: c(0x3b4261),
            gray: c(0x565f89),
            gray_bright: c(0x737aa2),
            command: c(0xe0af68),
            path: c(0xff9e64),
            running: c(0x7dcfff),
            warning: c(0xe0af68),
            fuzzy_accent: c(0x7aa2f7),
            accent_plan: c(0xe6b432),
            accent_verify: c(0xbb9af7),
            accent_remember: c(0x8bc34a),
            selection_border: c(0x3a4873),
            hover_border: c(0x373a50),
            prompt_border: c(0x3c4b78),
            prompt_border_active: c(0x4b5c8c),
            scrollbar_bg: c(0x1f2335),
            scrollbar_fg: c(0x292e42),
            diff_delete_bg: c(0x550f14),
            diff_delete_fg: c(0xf7768e),
            diff_insert_bg: c(0x0f4114),
            diff_insert_fg: c(0x9ece6a),
            diff_equal_fg: c(0x565f89),
            diff_gutter_fg: c(0x565f89),
            paste_bg: c(0x1f2335),
            paste_fg: c(0xa9b1d6),
            paste_dim: c(0x3b4261),
            md_h1: c(0x1abc9c),
            md_h2: c(0x7aa2f7),
            md_h3: c(0xff9e64),
            md_h4: c(0xf7768e),
            md_h5: c(0x9ece6a),
            md_h6: c(0xbb9af7),
            md_code: c(0x73daca),
            md_task_checked: c(0x7dcfff),
            md_task_unchecked: c(0x7aa2f7),
            md_muted: c(0x565f89),
            md_code_bg: c(0x292e42),
            md_text: c(0xc0caf5),
            link_fg: c(0x7aa2f7),
            backtick: c(0x9abdf5),
            placeholder: c(0x454c6e),
            hero_border: Color::Reset,
            selection_fg: c(0xc0caf5),
            selection_bg: c(0x313e73),
        };
        // blend(bg_base, gray_dim, 0.45); measured #333333 on groknight
        t.hero_border = blend(t.bg_base, t.gray_dim, 0.45);
        t
    }

    pub fn rosepine_moon() -> Theme {
        let mut t = Theme {
            name: "rosepine-moon",
            kind: Kind::RosePineMoon,
            dark: true,
            bandless: false,
            heading: [B, BU, B, BI, B, B],
            bg_base: c(0x232136),
            bg_light: c(0x393552),
            bg_dark: c(0x2a273f),
            bg_highlight: c(0x393552),
            bg_hover: c(0x44415a),
            bg_visual: c(0x44415a),
            accent_user: c(0xe0def4),
            accent_thinking: c(0x6e6a86),
            accent_tool: c(0x908caa),
            accent_system: c(0x3e8fb0),
            accent_error: c(0xeb6f92),
            accent_success: c(0x9ccfd8),
            accent_running: c(0x6e6a86),
            accent_skill: c(0x908caa),
            text_primary: c(0xe0def4),
            text_secondary: c(0x908caa),
            gray_dim: c(0x44415a),
            gray: c(0x6e6a86),
            gray_bright: c(0x908caa),
            command: c(0xf6c177),
            path: c(0xea9a97),
            running: c(0x9ccfd8),
            warning: c(0xf6c177),
            fuzzy_accent: c(0x3e8fb0),
            accent_plan: c(0xf6c177),
            accent_verify: c(0x3e8fb0),
            accent_remember: c(0x3e8fb0),
            selection_border: c(0x56526e),
            hover_border: c(0x44415a),
            prompt_border: c(0x44415a),
            prompt_border_active: c(0x56526e),
            scrollbar_bg: c(0x2a283e),
            scrollbar_fg: c(0x393552),
            diff_delete_bg: c(0x371e28),
            diff_delete_fg: c(0xeb6f92),
            diff_insert_bg: c(0x192d37),
            diff_insert_fg: c(0x9ccfd8),
            diff_equal_fg: c(0x6e6a86),
            diff_gutter_fg: c(0x6e6a86),
            paste_bg: c(0x2a273f),
            paste_fg: c(0x908caa),
            paste_dim: c(0x6e6a86),
            md_h1: c(0xe0def4),
            md_h2: c(0x9ccfd8),
            md_h3: c(0xc4a7e7),
            md_h4: c(0xea9a97),
            md_h5: c(0xf6c177),
            md_h6: c(0x3e8fb0),
            md_code: c(0x9ccfd8),
            md_task_checked: c(0x9ccfd8),
            md_task_unchecked: c(0x908caa),
            md_muted: c(0x6e6a86),
            md_code_bg: c(0x2a273f),
            md_text: c(0xe0def4),
            link_fg: c(0x9ccfd8),
            backtick: c(0x9abdf5),
            placeholder: c(0x55516b),
            hero_border: Color::Reset,
            selection_fg: c(0xc0caf5),
            selection_bg: c(0x313e73),
        };
        // blend(bg_base, gray_dim, 0.45); measured #333333 on groknight
        t.hero_border = blend(t.bg_base, t.gray_dim, 0.45);
        t
    }

    pub fn oscura_midnight() -> Theme {
        let mut t = Theme {
            name: "oscura-midnight",
            kind: Kind::OscuraMidnight,
            dark: true,
            bandless: false,
            heading: [B, B, B, BI, B, B],
            bg_base: c(0x030304),
            bg_light: c(0x0f1216),
            bg_dark: c(0x040507),
            bg_highlight: c(0x0f1216),
            bg_hover: c(0x242034),
            bg_visual: c(0x242034),
            accent_user: c(0xc4a7e7),
            accent_thinking: c(0x81868f),
            accent_tool: c(0x5e646c),
            accent_system: c(0x7dcfdf),
            accent_error: c(0xdc5a64),
            accent_success: c(0x50b48c),
            accent_running: c(0x6e5a9a),
            accent_skill: c(0x9b7ece),
            text_primary: c(0xe4e4e4),
            text_secondary: c(0xbebebe),
            gray_dim: c(0x5e646c),
            gray: c(0x81868f),
            gray_bright: c(0xbebebe),
            command: c(0xebd96e),
            path: c(0xf1bd00),
            running: c(0x7dcfdf),
            warning: c(0xebd96e),
            fuzzy_accent: c(0xc4a7e7),
            accent_plan: c(0xebd96e),
            accent_verify: c(0x9b7ece),
            accent_remember: c(0x8bc34a),
            selection_border: c(0x343048),
            hover_border: c(0x242034),
            prompt_border: c(0x242034),
            prompt_border_active: c(0x343048),
            scrollbar_bg: c(0x12101c),
            scrollbar_fg: c(0x343048),
            diff_delete_bg: c(0x2d0f19),
            diff_delete_fg: c(0xdc5a64),
            diff_insert_bg: c(0x0a231e),
            diff_insert_fg: c(0x50b48c),
            diff_equal_fg: c(0x81868f),
            diff_gutter_fg: c(0x81868f),
            paste_bg: c(0x040507),
            paste_fg: c(0xbebebe),
            paste_dim: c(0x81868f),
            md_h1: c(0xe4e4e4),
            md_h2: c(0xc4a7e7),
            md_h3: c(0x9b7ece),
            md_h4: c(0x50b48c),
            md_h5: c(0xebd96e),
            md_h6: c(0x7dcfdf),
            md_code: c(0x7dcfdf),
            md_task_checked: c(0x50b48c),
            md_task_unchecked: c(0xbebebe),
            md_muted: c(0x81868f),
            md_code_bg: c(0x040507),
            md_text: c(0xe4e4e4),
            link_fg: c(0x7dcfdf),
            backtick: c(0x9abdf5),
            placeholder: c(0x565960),
            hero_border: Color::Reset,
            selection_fg: c(0xc0caf5),
            selection_bg: c(0x313e73),
        };
        // blend(bg_base, gray_dim, 0.45); measured #333333 on groknight
        t.hero_border = blend(t.bg_base, t.gray_dim, 0.45);
        t
    }

    pub fn terminal() -> Theme {
        let mut t = Theme {
            name: "terminal",
            kind: Kind::Terminal,
            dark: true,
            bandless: true,
            heading: [BU, B, B, B, B, B],
            bg_base: Color::Reset,
            bg_light: Color::Reset,
            bg_dark: Color::Reset,
            bg_highlight: Color::Reset,
            bg_hover: Color::Reset,
            bg_visual: Color::Reset,
            accent_user: Color::Reset,
            accent_thinking: Color::Reset,
            accent_tool: Color::Reset,
            accent_system: named(4),
            accent_error: named(1),
            accent_success: named(2),
            accent_running: named(5),
            accent_skill: named(4),
            text_primary: Color::Reset,
            text_secondary: Color::Reset,
            gray_dim: named(8),
            gray: DIM_FG,
            gray_bright: Color::Reset,
            command: named(3),
            path: named(6),
            running: named(6),
            warning: named(3),
            fuzzy_accent: named(6),
            accent_plan: named(3),
            accent_verify: named(5),
            accent_remember: named(2),
            selection_border: named(8),
            hover_border: named(8),
            prompt_border: named(8),
            prompt_border_active: Color::Reset,
            scrollbar_bg: Color::Reset,
            scrollbar_fg: named(8),
            diff_delete_bg: Color::Reset,
            diff_delete_fg: named(1),
            diff_insert_bg: Color::Reset,
            diff_insert_fg: named(2),
            diff_equal_fg: Color::Reset,
            diff_gutter_fg: Color::Reset,
            paste_bg: Color::Reset,
            paste_fg: Color::Reset,
            paste_dim: Color::Reset,
            md_h1: Color::Reset,
            md_h2: Color::Reset,
            md_h3: Color::Reset,
            md_h4: Color::Reset,
            md_h5: Color::Reset,
            md_h6: Color::Reset,
            md_code: named(6),
            md_task_checked: named(2),
            md_task_unchecked: Color::Reset,
            md_muted: Color::Reset,
            md_code_bg: Color::Reset,
            md_text: Color::Reset,
            link_fg: named(4),
            backtick: named(4),
            placeholder: DIM_FG,
            hero_border: Color::Reset,
            selection_fg: c(0xc0caf5),
            selection_bg: c(0x313e73),
        };
        // blend(bg_base, gray_dim, 0.45); measured #333333 on groknight
        t.hero_border = blend(t.bg_base, t.gray_dim, 0.45);
        t
    }

    pub fn of(kind: Kind) -> Theme {
        match kind {
            Kind::Groknight => Theme::groknight(),
            Kind::Grokday => Theme::grokday(),
            Kind::Tokyonight => Theme::tokyonight(),
            Kind::RosePineMoon => Theme::rosepine_moon(),
            Kind::OscuraMidnight => Theme::oscura_midnight(),
            Kind::Terminal => Theme::terminal(),
        }
    }

    /// The theme `GROK_THEME` (then `LC_GROK_THEME`, then the saved choice) names; `auto` and
    /// anything unknown follow the terminal's polarity or fall back to groknight.
    pub fn from_env(state_dir: Option<&std::path::Path>) -> Theme {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let want = env("GROK_THEME")
            .or_else(|| env("LC_GROK_THEME"))
            .or_else(|| state_dir.and_then(|d| std::fs::read_to_string(d.join("theme")).ok()))
            .unwrap_or_default();
        match Kind::parse(&want) {
            Some(k) if k != Kind::Terminal || terminal_gate() => Theme::of(k),
            _ if matches!(want.trim().to_ascii_lowercase().as_str(), "auto" | "system") => {
                Theme::of(if appearance_is_dark() {
                    Kind::Groknight
                } else {
                    Kind::Grokday
                })
            }
            _ => Theme::groknight(),
        }
    }

    /// Remember the choice for the next launch.
    pub fn save(&self, state_dir: Option<&std::path::Path>) {
        if let Some(d) = state_dir {
            let _ = std::fs::create_dir_all(d);
            let _ = std::fs::write(d.join("theme"), self.kind.canonical());
        }
    }

    /// A name or alias; anything unknown is groknight.
    pub fn by_name(name: &str) -> Theme {
        Kind::parse(name).map_or_else(Theme::groknight, Theme::of)
    }

    pub fn syntax(&self) -> Syntax {
        self.kind.syntax()
    }

    /// The theme after this one in `/theme`'s cycle; `terminal` only joins it when `gate` is on.
    pub fn next_kind(&self, gate: bool) -> Kind {
        let list: Vec<Kind> = Kind::ALL
            .iter()
            .copied()
            .filter(|k| gate || *k != Kind::Terminal)
            .collect();
        let at = list.iter().position(|k| *k == self.kind).unwrap_or(0);
        list[(at + 1) % list.len()]
    }

    /// `blend(bg_base, c, a)`: a colour pulled toward the screen background. Without a
    /// background to blend with (the `terminal` theme) default-coloured text goes dim instead.
    pub fn recede(&self, c: Color, a: f32) -> Color {
        match (self.bg_base, c) {
            (Color::Rgb(..), Color::Rgb(..)) => blend(self.bg_base, c, a),
            (_, Color::Reset) => DIM_FG,
            (_, Color::Indexed(n)) => dim_indexed(n),
            _ => c,
        }
    }

    /// `gray` for glyphs and stamps. The `terminal` theme dims grey *text*; glyphs, timestamps and
    /// the scroll arrows stay in the default colour there.
    pub fn gray_solid(&self) -> Color {
        if self.gray == DIM_FG {
            Color::Reset
        } else {
            self.gray
        }
    }

    /// Collapsed finished bullet: the accent at half strength over the background.
    pub fn dim_accent(&self, accent: Color) -> Color {
        blend(self.bg_base, accent, 0.5)
    }

    /// Thinking body: markdown text blended 30% to the background.
    pub fn thinking_text(&self) -> Color {
        blend(self.bg_base, self.md_text, 0.7)
    }

    pub fn row_hover_bg(&self) -> Color {
        blend(self.bg_base, self.bg_dark, 0.5)
    }

    /// Context counter colour for a fill percentage: `text_primary`, `accent_user` at 50 to 65,
    /// `warning` at 75 to 85, `accent_error` from 95.
    pub fn context_color(&self, pct: f32) -> Color {
        let stops = [
            (0.0, self.text_primary),
            (50.0, self.accent_user),
            (65.0, self.accent_user),
            (75.0, self.warning),
            (85.0, self.warning),
            (95.0, self.accent_error),
        ];
        if pct >= 95.0 {
            return self.accent_error;
        }
        for w in stops.windows(2) {
            let ((p0, a), (p1, b)) = (w[0], w[1]);
            if pct <= p1 {
                let t = ((pct - p0) / (p1 - p0)).clamp(0.0, 1.0);
                return lerp(a, b, t);
            }
        }
        self.accent_error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hero_border_is_333333() {
        assert_eq!(Theme::groknight().hero_border, c(0x333333));
    }

    #[test]
    fn derived_colours_match_the_captures() {
        let t = Theme::groknight();
        assert_eq!(t.dim_accent(t.accent_success), c(0x59713f));
        assert_eq!(t.thinking_text(), c(0x929292));
        assert_eq!(blend(t.bg_base, t.accent_plan, 0.4), c(0x726444));
        assert_eq!(blend(t.bg_base, t.text_secondary, 0.6), c(0x808080));
        assert_eq!(blend(t.bg_base, t.text_secondary, 0.4), c(0x5c5c5c));
        assert_eq!(blend(t.scrollbar_bg, t.scrollbar_fg, 0.4), c(0x191919));
    }

    #[test]
    fn context_counter_colours() {
        let t = Theme::groknight();
        assert_eq!(t.context_color(0.0), c(0xe1e1e1));
        assert_eq!(t.context_color(60.0), c(0xc8c8c8));
        assert_eq!(t.context_color(80.0), c(0xe0af68));
        assert_eq!(t.context_color(99.0), c(0xf7768e));
        // 23.9K of 256K is 9.3%: #dcdcdc in 77-transcript-tall
        assert_eq!(t.context_color(9.3), c(0xdcdcdc));
    }
}
