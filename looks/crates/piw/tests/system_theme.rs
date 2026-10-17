//! Pi's default theme, `system`, built from what the terminal reports. The generator itself is
//! tested against Pi's own output in `src/system_theme.rs`; here the app wiring is: replies in,
//! cells out, compared with the vectors Pi printed for the same terminal.

mod common;

use common::*;
use piw::system_theme::{Appearance, Gen, Reported, Rgb};
use piw::theme::{ColorQuery, PiTheme, TermColors};
use ratatui::style::{Color, Modifier};
use serde_json::Value;
use tuikit::input::{ColorTarget, Reply};

const GOLDEN: &str = include_str!("../../tuikit/themes-pi/system-golden-more.json");

fn hex(s: &str) -> (u8, u8, u8) {
    let v = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).unwrap();
    (v(1), v(3), v(5))
}

fn vector(name: &str) -> Value {
    let all: Value = serde_json::from_str(GOLDEN).unwrap();
    all.as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no vector {name}"))
        .clone()
}

fn replies(v: &Value) -> Vec<Reply> {
    let mut out = Vec::new();
    if let Some(fg) = v["fg"].as_str() {
        out.push(Reply::Color {
            target: ColorTarget::Foreground,
            rgb: Some(hex(fg)),
        });
    }
    if let Some(bg) = v["bg"].as_str() {
        out.push(Reply::Color {
            target: ColorTarget::Background,
            rgb: Some(hex(bg)),
        });
    }
    if let Some(p) = v["palette"].as_array() {
        for (i, c) in p.iter().enumerate() {
            out.push(Reply::Color {
                target: ColorTarget::Palette(i as u8),
                rgb: Some(hex(c.as_str().unwrap())),
            });
        }
    }
    out.push(Reply::DeviceAttributes);
    out
}

/// A harness running Pi's defaults: no theme chosen anywhere.
fn default_theme_harness(w: u16, h: u16) -> Harness {
    Harness::with_theme(w, h, None)
}

fn feed_all(h: &mut Harness, name: &str) {
    h.app.start_color_query();
    for r in replies(&vector(name)) {
        h.app.on_term_reply(r);
    }
}

fn rgb(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// Row index of the first row containing `needle`.
fn row_of(t: &tuikit::testing::TestTerminal, needle: &str) -> u16 {
    t.plain()
        .lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle} not on screen:\n{}", t.plain())) as u16
}

#[test]
fn with_no_theme_chosen_and_nothing_reported_the_screen_is_pis_fallback_tier() {
    let mut h = default_theme_harness(120, 36);
    h.turn("hello", "Hello there.");
    let t = h.render();
    let y = row_of(&t, "hello");
    // user message: no panel background, default text colour, no faint
    let c = t.cell(2, y).unwrap();
    assert_eq!((c.bg, c.fg), (Color::Reset, Color::Reset), "{c:?}");
    assert!(!c.modifier.contains(Modifier::DIM));
    // the editor rules are palette index 6 (thinking medium), not 24-bit
    let rules: Vec<u16> = t
        .plain()
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with(&"─".repeat(60)))
        .map(|(i, _)| i as u16)
        .collect();
    assert_eq!(
        t.cell(5, rules[rules.len() - 2]).unwrap().fg,
        Color::Indexed(6)
    );
    // the footer is faint over the default colour
    let f = row_of(&t, "(main)");
    let c = t.cell(2, f).unwrap();
    assert!(
        c.modifier.contains(Modifier::DIM) && c.fg == Color::Reset,
        "{c:?}"
    );
}

#[test]
fn a_terminal_that_reports_its_colours_gets_the_theme_pi_generates_for_them() {
    let want = vector("onedark-palette");
    let colors = &want["out"]["colors"];
    let mut h = default_theme_harness(120, 36);
    feed_all(&mut h, "onedark-palette");
    h.turn("hello", "Hello there.");
    let t = h.render();
    let y = row_of(&t, "hello");
    let user_bg = hex(colors["userMessageBg"].as_str().unwrap());
    // the user message is three full-width rows on userMessageBg (pad, text, pad)
    for dy in [0u16, 1, 2] {
        let row = y - 1 + dy;
        for x in [0u16, 1, 60, 119] {
            assert_eq!(
                rgb(t.cell(x, row).unwrap().bg),
                Some(user_bg),
                "row {row} col {x}"
            );
        }
    }
    // text on it: userMessageText is "" (the terminal's foreground) when that is strong enough
    let text = colors["userMessageText"].as_str().unwrap();
    let fg = t.cell(2, y).unwrap().fg;
    if text.is_empty() {
        assert_eq!(fg, Color::Reset);
    } else {
        assert_eq!(rgb(fg), Some(hex(text)));
    }
    // the editor's rules are thinking medium
    let rules: Vec<u16> = t
        .plain()
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with(&"─".repeat(60)))
        .map(|(i, _)| i as u16)
        .collect();
    assert_eq!(
        rgb(t.cell(5, rules[rules.len() - 2]).unwrap().fg),
        Some(hex(colors["thinkingMedium"].as_str().unwrap()))
    );
    // nothing is faint in a generated theme; the footer is `dim` as a colour
    let f = row_of(&t, "(main)");
    let c = t.cell(2, f).unwrap();
    assert!(!c.modifier.contains(Modifier::DIM));
    assert_eq!(rgb(c.fg), Some(hex(colors["dim"].as_str().unwrap())));
}

#[test]
fn a_light_terminal_gets_a_light_theme() {
    let mut h = default_theme_harness(120, 36);
    feed_all(&mut h, "solarized-light-palette");
    assert!(h.app.theme.light);
    h.turn("hello", "Hello there.");
    let t = h.render();
    let y = row_of(&t, "hello");
    let want = vector("solarized-light-palette");
    assert_eq!(
        rgb(t.cell(0, y).unwrap().bg),
        Some(hex(want["out"]["colors"]["userMessageBg"]
            .as_str()
            .unwrap()))
    );
}

#[test]
fn replies_that_arrive_after_the_startup_wait_still_theme_the_screen() {
    let mut h = default_theme_harness(120, 36);
    h.app.start_color_query();
    // 100 ms passed with nothing; the screen is the fallback tier
    h.app.color_query_timed_out();
    assert_eq!(
        h.app.theme.color(piw::theme::Tok::UserMessageBg),
        Color::Reset
    );
    h.app.dirty = false;
    for r in replies(&vector("black-bg-only")) {
        h.app.on_term_reply(r);
    }
    assert!(h.app.dirty, "a late report must repaint");
    let want = vector("black-bg-only");
    assert_eq!(
        h.app.theme.color(piw::theme::Tok::UserMessageBg),
        Color::Rgb(
            hex(want["out"]["colors"]["userMessageBg"].as_str().unwrap()).0,
            hex(want["out"]["colors"]["userMessageBg"].as_str().unwrap()).1,
            hex(want["out"]["colors"]["userMessageBg"].as_str().unwrap()).2
        )
    );
}

#[test]
fn cached_rows_are_redrawn_when_the_colours_arrive() {
    // the transcript caches rendered rows by theme name, and the name stays `system`
    let mut h = default_theme_harness(120, 36);
    h.turn("hello", "Hello there.");
    let before = h.render();
    let y = row_of(&before, "hello");
    assert_eq!(before.cell(0, y).unwrap().bg, Color::Reset);
    feed_all(&mut h, "black-bg-only");
    let after = h.render();
    assert_ne!(after.cell(0, y).unwrap().bg, Color::Reset);
}

#[test]
fn an_explicit_theme_ignores_the_terminal() {
    for name in ["dark", "light"] {
        let mut h = Harness::with_theme(120, 36, Some(name));
        let before = PiTheme::builtin(name).unwrap();
        feed_all(&mut h, "onedark-palette");
        assert_eq!(
            h.app.theme.color(piw::theme::Tok::Accent),
            before.color(piw::theme::Tok::Accent)
        );
        assert_eq!(h.app.theme.name, name);
    }
}

#[test]
fn a_light_dark_pair_follows_the_terminals_appearance() {
    let mut h = Harness::with_theme(120, 36, Some("light/dark"));
    feed_all(&mut h, "solarized-light-palette");
    assert_eq!(h.app.theme.name, "light");
    // and back, when the terminal switches and its new colours arrive
    h.app.on_color_scheme(true);
    feed_all(&mut h, "onedark-palette");
    assert_eq!(h.app.theme.name, "dark");
}

#[test]
fn a_scheme_report_asks_for_the_colours_again() {
    let mut h = default_theme_harness(120, 36);
    h.app.on_color_scheme(false);
    assert!(h.app.pending.contains(&piw::app::Deferred::QueryColors));
    // with no background known the report itself decides the appearance
    assert_eq!(h.app.term.appearance_with(None), Appearance::Light);
}

#[test]
fn appearance_without_a_background_reads_the_scheme_then_colorfgbg_then_dark() {
    let mut t = TermColors::default();
    assert_eq!(t.appearance_with(None), Appearance::Dark);
    assert_eq!(t.appearance_with(Some("15;0")), Appearance::Dark);
    assert_eq!(t.appearance_with(Some("0;15")), Appearance::Light);
    assert_eq!(t.appearance_with(Some("0;default;7")), Appearance::Light);
    assert_eq!(
        t.appearance_with(Some("0;8")),
        Appearance::Dark,
        "bright black is a dark background"
    );
    assert_eq!(t.appearance_with(Some("0;99")), Appearance::Dark);
    t.scheme = Some(Appearance::Light);
    assert_eq!(
        t.appearance_with(Some("15;0")),
        Appearance::Light,
        "the report wins over the variable"
    );
    t.reported.background = Some(Rgb::new(0, 0, 0));
    assert_eq!(
        t.appearance_with(None),
        Appearance::Dark,
        "a reported background wins over both"
    );
}

#[test]
fn the_batch_ends_on_device_attributes_or_all_eighteen_replies() {
    let mut q = ColorQuery::new();
    assert!(!q.feed(Reply::Color {
        target: ColorTarget::Background,
        rgb: Some((1, 2, 3))
    }));
    assert!(q.feed(Reply::DeviceAttributes));
    let r = q.reported();
    assert_eq!(r.background, Some(Rgb::new(1, 2, 3)));
    assert_eq!(
        r.palette, None,
        "a palette counts only when all sixteen arrived"
    );
    assert!(!q.feed(Reply::DeviceAttributes), "nothing after the end");

    let mut q = ColorQuery::new();
    let mut done = false;
    for r in replies(&vector("onedark-palette"))
        .into_iter()
        .filter(|r| *r != Reply::DeviceAttributes)
    {
        done = q.feed(r);
        // a repeated reply is not a new one
        assert!(!q.feed(r) || done);
    }
    assert!(done, "fg, bg and 16 palette colours are the 18 replies");
    assert!(q.reported().palette.is_some());
}

#[test]
fn the_fallback_tier_is_what_the_generator_prints_for_nothing_reported() {
    let t = PiTheme::system(&Reported::default(), Appearance::Dark);
    let fb = PiTheme::system_fallback();
    for k in piw::theme::ALL_TOKENS {
        assert_eq!(t.paint(*k), fb.paint(*k));
    }
    let g = piw::system_theme::generate(&Reported::default(), 1.0, Appearance::Dark);
    assert_eq!(g.colors["thinkingXhigh"], Gen::Index(13));
}
