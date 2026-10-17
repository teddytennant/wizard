// OWNER: input
//! The completion popup above the composer: `/` commands and `@` files. Rows are one line
//! each, never wrapped (design section d).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::paint::{fill, put_str};
use tuikit::width::{display_width, truncate, truncate_left};

use crate::palette::{Glyphs, Palette};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopKind {
    Slash,
    File,
}

#[derive(Clone, Debug, Default)]
pub struct PItem {
    pub label: String,
    /// Argument hint of a command (`[n]`, `<text>`), drawn before the description.
    pub hint: String,
    pub desc: String,
    pub tag: String,
    /// Text that replaces the token being completed.
    pub insert: String,
    /// Char indices into `label` that matched the query.
    pub indices: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct Popup {
    pub kind: PopKind,
    pub items: Vec<PItem>,
    pub sel: usize,
    /// Byte range of the token in the editor text that accepting replaces.
    pub token: std::ops::Range<usize>,
}

/// Files for `@q`, scored inline. A big index goes through `files::FileSearch` instead.
pub fn file_items(files: &[String], q: &str, limit: usize) -> Vec<PItem> {
    crate::files::search(files, q, limit, &|| false).unwrap_or_default()
}

/// Where a popup goes: column origin and width of the transcript column, the row just above
/// the composer (`bottom`) and how many rows it may take.
#[derive(Clone, Copy)]
pub struct PopupPlace {
    pub x: u16,
    pub c: u16,
    pub bottom: u16,
    pub max_rows: u16,
}

/// The popup above the composer.
pub fn draw_popup(buf: &mut Buffer, pop: &Popup, place: PopupPlace, p: &Palette, g: &Glyphs) {
    let PopupPlace {
        x,
        c,
        bottom,
        max_rows,
    } = place;
    let n = pop.items.len().min(max_rows as usize);
    if n == 0 || bottom == 0 {
        return;
    }
    let n = n.min(bottom as usize);
    // A list longer than the window says so: the last row becomes `3 of 24` (finding 17).
    let counter = pop.items.len() > n && n >= 3;
    let total_rows = n;
    let n = if counter { n - 1 } else { n };
    let y0 = bottom - total_rows as u16;
    // Keep the selection in view, with it at the bottom edge when scrolling down.
    let first = pop
        .sel
        .saturating_sub(n.saturating_sub(1))
        .min(pop.items.len() - n);
    let first = if pop.sel < first { pop.sel } else { first };
    let rect = Rect::new(x, y0, c, total_rows as u16);
    fill(buf, rect, Style::new().bg(p.surface));
    if counter {
        let label = format!("{} of {}", pop.sel + 1, pop.items.len());
        let lw = display_width(&label) as u16;
        put_str(
            buf,
            x + c.saturating_sub(lw + 2),
            y0 + n as u16,
            &label,
            Style::new().fg(p.faint).bg(p.surface),
            rect,
        );
    }
    let files = pop.kind == PopKind::File;
    let tag_w = pop
        .items
        .iter()
        .map(|i| display_width(&i.tag))
        .max()
        .unwrap_or(0);
    let label_w = if files {
        (c as usize).saturating_sub(6)
    } else {
        // Sized to the rows in view, so a short list does not leave a wide gap.
        pop.items
            .iter()
            .skip(first)
            .take(n)
            .map(|i| display_width(&i.label))
            .max()
            .unwrap_or(0)
            .min(26)
    };
    for (row, item) in pop.items.iter().enumerate().skip(first).take(n) {
        let y = y0 + (row - first) as u16;
        let sel = row == pop.sel;
        let bg = if sel { p.raised } else { p.surface };
        let reverse = sel
            && matches!(
                p.depth,
                crate::palette::Depth::Ansi16 | crate::palette::Depth::Mono
            );
        let base = Style::new().bg(bg);
        let base = if reverse {
            base.add_modifier(Modifier::REVERSED)
        } else {
            base
        };
        fill(buf, Rect::new(x, y, c, 1), base);
        put_str(
            buf,
            x + 1,
            y,
            if sel { g.select } else { " " },
            base.fg(p.accent),
            rect,
        );
        // Files keep the end of the path, because the name is what you are looking for.
        let (label, skip) = if files && display_width(&item.label) > label_w {
            let t = truncate_left(&item.label, label_w);
            let cut = item
                .label
                .chars()
                .count()
                .saturating_sub(t.chars().count().saturating_sub(1));
            (t, cut as isize - 1)
        } else {
            (truncate(&item.label, label_w), 0)
        };
        let dir_end = if files {
            item.label
                .trim_end_matches('/')
                .rfind('/')
                .map_or(0, |i| item.label[..=i].chars().count())
        } else {
            0
        };
        let mut cx = x + 3;
        for (i, ch) in label.chars().enumerate() {
            // Index of this char in the full label (the left cut adds an ellipsis first).
            let orig = (i as isize + skip).max(0) as usize;
            let hit = item.indices.contains(&orig);
            let fg = if files && orig < dir_end && !hit {
                p.dim
            } else {
                p.text
            };
            let st = if hit {
                base.fg(p.text).add_modifier(Modifier::BOLD)
            } else {
                base.fg(fg)
            };
            cx = put_str(buf, cx, y, &ch.to_string(), st, rect);
        }
        if files {
            continue;
        }
        let dx = x + 3 + label_w as u16 + 2;
        let room = (c as usize).saturating_sub(dx.saturating_sub(x) as usize + tag_w + 3);
        let mut dx = dx;
        if !item.hint.is_empty() && room > 3 {
            let h = truncate(&item.hint, room);
            dx = put_str(buf, dx, y, &h, base.fg(p.faint), rect) + 1;
        }
        let room = (c as usize).saturating_sub(dx.saturating_sub(x) as usize + tag_w + 3);
        if !item.desc.is_empty() && room > 3 {
            put_str(
                buf,
                dx,
                y,
                &truncate(&item.desc, room),
                base.fg(p.dim),
                rect,
            );
        }
        if !item.tag.is_empty() {
            let tx = x + c - 1 - display_width(&item.tag) as u16;
            put_str(buf, tx, y, &item.tag, base.fg(p.faint), rect);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_list_ends_in_a_counter_and_a_short_one_does_not() {
        use crate::palette::{Depth, Kind, UNICODE};
        use tuikit::testing::TestTerminal;
        let items: Vec<PItem> = (0..24)
            .map(|i| PItem {
                label: format!("/cmd{i}"),
                insert: format!("/cmd{i}"),
                ..PItem::default()
            })
            .collect();
        let p = Palette::new(Kind::Hearth, Depth::True);
        let draw = |items: Vec<PItem>, sel: usize| {
            let pop = Popup {
                kind: PopKind::Slash,
                items,
                sel,
                token: 0..1,
            };
            let mut t = TestTerminal::new(50, 12);
            t.draw(|b, a| {
                let place = PopupPlace {
                    x: 0,
                    c: a.width,
                    bottom: 10,
                    max_rows: 8,
                };
                draw_popup(b, &pop, place, &p, &UNICODE);
            });
            t.plain()
        };
        let s = draw(items.clone(), 2);
        assert!(s.contains("3 of 24"), "{s}");
        // Seven items and the counter fill the eight rows; the selection is inside them.
        assert_eq!(s.lines().filter(|l| l.contains("/cmd")).count(), 7, "{s}");
        assert!(s.contains("▸ /cmd2"), "{s}");
        let s = draw(items.into_iter().take(5).collect(), 0);
        assert!(!s.contains(" of "), "{s}");
    }

    fn files() -> Vec<String> {
        [
            "README.md",
            "src/main.rs",
            "src/lib.rs",
            "library/notes.txt",
            "docs/openc-design.md",
            "crates/openc/src/ui/composer.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn a_hit_in_the_file_name_beats_one_in_a_directory() {
        let it = file_items(&files(), "lib", 10);
        assert_eq!(it[0].insert, "src/lib.rs", "{it:?}");
        let it = file_items(&files(), "composer", 10);
        assert_eq!(it[0].insert, "crates/openc/src/ui/composer.rs");
        // Highlighted chars are inside the file name.
        assert!(it[0]
            .indices
            .iter()
            .all(|i| *i >= "crates/openc/src/ui/".len()));
    }

    #[test]
    fn no_query_lists_shallow_files_first() {
        let it = file_items(&files(), "", 3);
        assert_eq!(it[0].insert, "README.md");
        assert!(!it.iter().any(|i| i.insert.starts_with("crates")));
    }

    #[test]
    fn a_long_path_keeps_its_end_and_the_matched_letters_stay_bold() {
        use crate::palette::{Depth, Kind, UNICODE};
        use tuikit::testing::TestTerminal;
        let files =
            vec!["crates/openc/src/ui/composer/deeply/nested/long_file_name.rs".to_string()];
        let items = file_items(&files, "lfn", 5);
        let pop = Popup {
            kind: PopKind::File,
            items,
            sel: 0,
            token: 0..4,
        };
        let p = Palette::new(Kind::Hearth, Depth::True);
        let mut t = TestTerminal::new(40, 6);
        t.draw(|b, a| {
            let place = PopupPlace {
                x: 0,
                c: a.width,
                bottom: 3,
                max_rows: 4,
            };
            draw_popup(b, &pop, place, &p, &UNICODE);
        });
        let s = t.plain();
        assert!(s.contains("long_file_name.rs"), "{s}");
        assert!(s.contains('…'), "{s}");
    }
}
