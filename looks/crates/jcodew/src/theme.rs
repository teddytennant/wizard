//! jcode's palette. Every colour goes through [`rgb`], which gives the exact value on a
//! truecolor terminal and the nearest xterm-256 index otherwise, the way jcode decides it.

use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::style::Color;

static TRUECOLOR: AtomicBool = AtomicBool::new(true);

pub fn set_truecolor(on: bool) {
    TRUECOLOR.store(on, Ordering::Relaxed);
}

/// Truecolor when the environment says so; anything else gets 256 colours.
pub fn detect_truecolor() -> bool {
    let var = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
    let colorterm = var("COLORTERM");
    if colorterm == "truecolor" || colorterm == "24bit" {
        return true;
    }
    if matches!(
        var("TERM_PROGRAM").as_str(),
        "ghostty" | "iterm.app" | "wezterm" | "warp" | "alacritty" | "hyper"
    ) {
        return true;
    }
    if [
        "GHOSTTY_RESOURCES_DIR",
        "GHOSTTY_BIN_DIR",
        "WEZTERM_EXECUTABLE",
        "WEZTERM_PANE",
    ]
    .iter()
    .any(|k| std::env::var_os(k).is_some())
    {
        return true;
    }
    let term = var("TERM");
    term.contains("kitty") || term.contains("ghostty") || term.contains("alacritty")
}

const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn dist(a: (u8, u8, u8), b: (u8, u8, u8)) -> i32 {
    let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2);
    2 * d(a.0, b.0) + 4 * d(a.1, b.1) + 3 * d(a.2, b.2)
}

/// Nearest xterm-256 index: the 6x6x6 cube or the grey ramp, grey only when strictly closer.
pub fn xterm256(r: u8, g: u8, b: u8) -> u8 {
    let near = |v: u8| {
        (0..6)
            .min_by_key(|&i| (CUBE[i] as i32 - v as i32).abs())
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let cube = (CUBE[ri], CUBE[gi], CUBE[bi]);
    let avg = (r as u32 + g as u32 + b as u32) / 3;
    let gi2 = if avg > 243 {
        23
    } else {
        ((avg as i32 - 8 + 5) / 10).clamp(0, 23)
    };
    let gv = (8 + 10 * gi2) as u8;
    if dist((r, g, b), (gv, gv, gv)) < dist((r, g, b), cube) {
        232 + gi2 as u8
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    if TRUECOLOR.load(Ordering::Relaxed) {
        Color::Rgb(r, g, b)
    } else {
        Color::Indexed(xterm256(r, g, b))
    }
}

macro_rules! palette {
    ($($name:ident = ($r:expr, $g:expr, $b:expr);)*) => {
        $(pub fn $name() -> Color { rgb($r, $g, $b) })*
    };
}

palette! {
    user = (138, 180, 248);
    ai = (129, 199, 132);
    tool = (120, 120, 120);
    dim = (80, 80, 80);
    accent = (186, 139, 255);
    system = (255, 170, 220);
    queued = (255, 193, 7);
    asap = (110, 210, 255);
    pending = (140, 140, 140);
    user_text = (245, 245, 255);
    user_bg = (35, 40, 50);
    header_name = (190, 210, 235);
    success = (100, 200, 100);
    warning = (255, 200, 100);
    error = (255, 100, 100);
    info = (140, 180, 255);
    muted = (140, 140, 150);
    selected = (255, 213, 128);
    unselected = (128, 203, 196);
    md_text = (200, 200, 195);
    md_bold = (240, 240, 235);
    md_code_fg = (180, 180, 180);
    md_code_bg = (45, 45, 45);
    md_dim = (100, 100, 100);
    md_link = (120, 180, 240);
    md_table = (150, 150, 150);
    widget_border = (70, 70, 80);
    widget_label = (175, 175, 190);
    scroll_thumb = (136, 148, 172);
    model_pink = (255, 150, 200);
    diff_add = (100, 200, 100);
    diff_del = (200, 100, 100);
    shell = (110, 214, 151);
}

const RAINBOW: [(u8, u8, u8); 7] = [
    (255, 80, 80),
    (255, 160, 80),
    (255, 230, 80),
    (80, 220, 100),
    (80, 200, 220),
    (100, 140, 255),
    (180, 100, 255),
];

/// Colour of a prompt number `distance` prompts back from the next one: a rainbow that decays
/// into grey.
pub fn rainbow_prompt(distance: usize) -> Color {
    let decay = (-0.4 * distance as f32).exp();
    let (r, g, b) = RAINBOW[distance.min(6)];
    let mix = |c: u8| (c as f32 * decay + 80.0 * (1.0 - decay)) as u8;
    rgb(mix(r), mix(g), mix(b))
}

pub fn heading(level: u8) -> Color {
    match level {
        1 => rgb(255, 215, 100),
        2 => rgb(240, 190, 90),
        3 => rgb(220, 170, 80),
        _ => rgb(200, 155, 75),
    }
}

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(elapsed_secs: f32) -> &'static str {
    SPINNER[((elapsed_secs * 12.5) as usize) % SPINNER.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rainbow_decays_toward_grey() {
        set_truecolor(true);
        assert_eq!(rainbow_prompt(0), Color::Rgb(255, 80, 80));
        // distance 1: (255,160,80) blended with grey at exp(-0.4)
        assert_eq!(rainbow_prompt(1), Color::Rgb(197, 133, 80));
    }

    #[test]
    fn quantises_to_cube_and_ramp() {
        assert_eq!(xterm256(255, 255, 255), 231);
        assert_eq!(xterm256(80, 80, 80), 239);
        assert_eq!(xterm256(0, 0, 0), 16);
    }
}
