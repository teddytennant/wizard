//! On a terminal that takes 24-bit colour, Claude Code writes RGB; under tmux and the like it
//! writes the 256-colour indexes in [`crate::theme`]. The two are not the same colours: its
//! orange is `rgb(215,119,87)` where index 174 is a pink. This maps the indexes the look draws
//! to the RGB the reference writes for them, read from captures of both made with and without
//! `COLORTERM=truecolor`. An index with no entry is left alone.

use ratatui::buffer::Buffer;
use ratatui::style::Color;

/// Dark theme: index, then the RGB the reference shows for it.
const DARK: &[(u8, [u8; 3])] = &[
    (16, [0, 0, 0]),
    (22, [2, 40, 0]),
    (28, [4, 71, 0]),
    (52, [61, 1, 0]),
    (73, [72, 150, 140]),
    (77, [80, 200, 80]),
    (81, [102, 217, 239]),
    (110, [106, 155, 204]),
    (114, [78, 186, 101]),
    (147, [175, 135, 255]),
    (148, [166, 226, 46]),
    (153, [177, 185, 249]),
    (167, [220, 90, 90]),
    (174, [215, 119, 87]),
    (186, [230, 219, 116]),
    (211, [253, 93, 177]),
    (220, [255, 193, 7]),
    (237, [55, 55, 55]),
    (239, [80, 80, 80]),
    (244, [136, 136, 136]),
    (246, [153, 153, 153]),
];

/// Light theme.
const LIGHT: &[(u8, [u8; 3])] = &[
    (16, [0, 0, 0]),
    (21, [0, 0, 255]),
    (30, [0, 102, 102]),
    (65, [44, 122, 57]),
    (105, [87, 105, 247]),
    (129, [135, 0, 255]),
    (174, [215, 119, 87]),
    (199, [255, 0, 135]),
    (241, [102, 102, 102]),
    (245, [138, 138, 138]),
    (248, [175, 175, 175]),
];

/// Whether `COLORTERM` says the terminal takes 24-bit colour.
pub fn supported() -> bool {
    std::env::var("COLORTERM").is_ok_and(|v| matches!(v.as_str(), "truecolor" | "24bit"))
}

pub fn map(color: Color, light: bool) -> Color {
    let Color::Indexed(n) = color else {
        return color;
    };
    let table = if light { LIGHT } else { DARK };
    match table.iter().find(|(index, _)| *index == n) {
        Some((_, [r, g, b])) => Color::Rgb(*r, *g, *b),
        None => color,
    }
}

/// [`map`] when `on`.
pub fn map_if(on: bool, color: Color, light: bool) -> Color {
    if on {
        map(color, light)
    } else {
        color
    }
}

/// Rewrite every indexed colour in `buf` to the reference's RGB.
pub fn paint(buf: &mut Buffer, light: bool) {
    for cell in buf.content.iter_mut() {
        cell.fg = map(cell.fg, light);
        cell.bg = map(cell.bg, light);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brand_index_becomes_the_reference_orange_and_unknown_ones_stay() {
        assert_eq!(map(Color::Indexed(174), false), Color::Rgb(215, 119, 87));
        assert_eq!(map(Color::Indexed(174), true), Color::Rgb(215, 119, 87));
        assert_eq!(map(Color::Indexed(246), false), Color::Rgb(153, 153, 153));
        assert_eq!(map(Color::Indexed(241), true), Color::Rgb(102, 102, 102));
        assert_eq!(map(Color::Indexed(200), false), Color::Indexed(200));
        assert_eq!(map(Color::Reset, false), Color::Reset);
        assert_eq!(map(Color::Indexed(12), false), Color::Indexed(12));
    }
}
