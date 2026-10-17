//! The colours Claude Code writes, as 256-colour indexes: what it emits under tmux, where the
//! reference captures were taken.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub brand: Color,
    pub shimmer: Color,
    pub mark_bg: Color,
    pub text: Color,
    pub secondary: Color,
    pub subtle: Color,
    pub border: Color,
    pub suggestion: Color,
    /// Dialog titles.
    pub title: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub bash: Color,
    pub plan: Color,
    pub accept: Color,
    pub user_bg: Color,
    pub bash_bg: Color,
    pub user_fg: Color,
    pub bullet: Color,
    pub tab_bg: Color,
    pub tab_fg: Color,
    pub added_bg: Color,
    pub added_word: Color,
    pub added_fg: Color,
    pub removed_bg: Color,
    pub removed_fg: Color,
    /// Syntax colours for diffs: plain, keyword, name, string, number, comment, macro.
    pub code: [Color; 7],
}

const fn c(n: u8) -> Color {
    Color::Indexed(n)
}

impl Theme {
    pub const fn dark() -> Theme {
        Theme {
            brand: c(174),
            shimmer: c(216),
            mark_bg: c(16),
            text: Color::Reset,
            secondary: c(246),
            subtle: c(239),
            border: c(244),
            suggestion: c(153),
            title: c(153),
            success: c(114),
            error: c(211),
            warning: c(220),
            bash: c(211),
            plan: c(73),
            accept: c(147),
            user_bg: c(237),
            bash_bg: c(59),
            user_fg: c(231),
            bullet: c(231),
            tab_bg: c(110),
            tab_fg: c(16),
            added_bg: c(22),
            added_word: c(28),
            added_fg: c(77),
            removed_bg: c(52),
            removed_fg: c(167),
            code: [c(231), c(81), c(148), c(186), c(141), c(242), c(148)],
        }
    }

    /// The light theme. The colours the reference shows in its light captures are copied; the
    /// diff, warning and spinner colours were not on those screens and are picked to match.
    pub const fn light() -> Theme {
        Theme {
            brand: c(174),
            shimmer: c(209),
            mark_bg: c(16),
            text: Color::Reset,
            secondary: c(241),
            subtle: c(248),
            border: c(245),
            suggestion: c(105),
            title: c(21),
            success: c(65),
            error: c(161),
            warning: c(136),
            bash: c(199),
            plan: c(30),
            accept: c(129),
            user_bg: c(255),
            bash_bg: c(254),
            user_fg: c(16),
            bullet: c(16),
            tab_bg: c(25),
            tab_fg: c(231),
            added_bg: c(194),
            added_word: c(157),
            added_fg: c(28),
            removed_bg: c(224),
            removed_fg: c(124),
            code: [c(16), c(25), c(28), c(94), c(91), c(244), c(28)],
        }
    }

    pub fn fg(&self, color: Color) -> Style {
        Style::new().fg(color)
    }

    pub fn dim(&self) -> Style {
        Style::new().fg(self.secondary)
    }

    pub fn plain(&self) -> Style {
        Style::new()
    }

    pub fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }
}
