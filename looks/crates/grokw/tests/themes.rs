//! The six palettes, cell for cell against `reference/grok/120x36/{80,81}-theme-*`: the markdown
//! session at its tail and the long turn scrolled to its first prompt with the box on it.

mod common;

use common::cells::{assert_cells, Skip};
use common::*;
use grokw::theme::{Kind, Theme};

fn tail(kind: Kind) -> Harness {
    let mut h = Harness::new(120, 36);
    h.app.theme = Theme::of(kind);
    h.app.fixed_hm = Some((17, 43));
    h.markdown_turn();
    h
}

fn diff(kind: Kind) -> Harness {
    let mut h = Harness::new(120, 36);
    h.app.theme = Theme::of(kind);
    h.long_turn(5);
    h.pin_durations(&[2.3, 5.9, 2.5, 5.9], 39.0);
    h.render();
    h.press(crossterm::event::KeyCode::Tab);
    h.render();
    h.app.view.to_top();
    h.app.view.selected = h.app.view.doc.entries.first().map(|e| e.key);
    h
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn every_theme_matches_the_markdown_tail() {
    for k in Kind::ALL {
        let n = k.canonical();
        let mut h = tail(k);
        h.dump(&format!("theme-{n}-bottom"));
        assert_cells(
            &format!("120x36/80-theme-{n}-bottom"),
            &h.render(),
            &Skip {
                bg: Some(h.app.theme.bg_base),
                ..Default::default()
            },
        );
    }
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn every_theme_matches_the_long_turn() {
    for k in Kind::ALL {
        let n = k.canonical();
        let mut h = diff(k);
        h.dump(&format!("theme-{n}-diff"));
        assert_cells(
            &format!("120x36/81-theme-{n}-diff"),
            &h.render(),
            &Skip {
                bg: Some(h.app.theme.bg_base),
                ..Default::default()
            },
        );
    }
}

#[test]
fn slash_and_palette_dump_for_the_dialog_owner() {
    for k in Kind::ALL {
        let n = k.canonical();
        let mut h = tail(k);
        h.type_str("/");
        h.dump(&format!("theme-{n}-slash"));
        let mut h = tail(k);
        h.ctrl('p');
        h.dump(&format!("theme-{n}-palette"));
    }
}

#[test]
fn theme_names_and_aliases_parse() {
    assert_eq!(Kind::parse("DARK"), Some(Kind::Groknight));
    assert_eq!(Kind::parse("light"), Some(Kind::Grokday));
    assert_eq!(Kind::parse("rose-pine-moon"), Some(Kind::RosePineMoon));
    assert_eq!(Kind::parse("oscura"), Some(Kind::OscuraMidnight));
    assert_eq!(Kind::parse("native"), Some(Kind::Terminal));
    assert_eq!(Kind::parse("nope"), None);
    assert_eq!(Kind::OscuraMidnight.pretty(), "oscura-midnight");
}

#[test]
fn theme_command_cycles_without_terminal_and_toasts_the_pretty_name() {
    let mut h = Harness::new(120, 36);
    h.app.opts.state_dir = None;
    h.app.dispatch("/theme".into());
    assert_eq!(h.app.theme.kind, Kind::Grokday);
    assert!(h.app.toast.as_ref().unwrap().text.contains("Grok Day"));
    h.app.dispatch("/theme tokyo".into());
    assert_eq!(h.app.theme.kind, Kind::Tokyonight);
    h.app.dispatch("/theme oscura".into());
    assert_eq!(h.app.theme.kind, Kind::OscuraMidnight);
    // wraps past the last palette back to groknight, never landing on `terminal`
    h.app.dispatch("/theme".into());
    assert_eq!(h.app.theme.kind, Kind::Groknight);
    // the gate is closed here, so the name is not offered
    h.app.dispatch("/theme terminal".into());
    assert_eq!(h.app.theme.kind, Kind::Groknight);
}
