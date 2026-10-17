//! Per-code-point widths, for a terminal without mode 2027. Its own test binary: the width model
//! is process-wide.
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::Terminal;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What the terminal would print for one row, with the cursor moves and colours taken out.
fn drawn(text: &str) -> String {
    let sink = Sink::default();
    let mut term = Terminal::new(CrosstermBackend::new(sink.clone())).unwrap();
    term.resize(Rect::new(0, 0, 20, 1)).unwrap();
    term.draw(|f| {
        tuikit::paint::put_str(
            f.buffer_mut(),
            0,
            0,
            text,
            Style::default(),
            Rect::new(0, 0, 20, 1),
        );
    })
    .unwrap();
    let bytes = sink.0.lock().unwrap().clone();
    let s = String::from_utf8(bytes).unwrap();
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            // CSI ... final
            if it.peek() == Some(&'[') {
                it.next();
                for n in it.by_ref() {
                    if ('\x40'..='\x7e').contains(&n) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn a_cluster_that_a_terminal_draws_wider_does_not_get_blanks_written_over_it() {
    tuikit::width::set_cluster_widths(false);
    // Three emoji joined: 6 columns on a terminal that adds up code points, 2 by clusters.
    let family = "👨\u{200d}👩\u{200d}👧";
    assert_eq!(tuikit::width::display_width(family), 6);
    let out = drawn(&format!("a{family}b"));
    let trimmed = out.trim_end();
    assert!(
        trimmed.contains(&format!("a{family}b")),
        "cells around the glyph were written separately: {trimmed:?}"
    );
    // A keycap is one column there, two by clusters: the next letter must follow at once.
    let out = drawn("1\u{fe0f}\u{20e3}z");
    assert!(out.trim_end().contains("\u{20e3}z"), "{out:?}");
}
