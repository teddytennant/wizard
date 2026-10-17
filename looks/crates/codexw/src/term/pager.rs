// OWNER: pager
//! Alt-screen guard for overlays (transcript pager, `/diff`, pickers). The mechanics live on
//! `InlineTerminal::{enter,leave}_alt_screen`; this guard pairs them so an early return cannot
//! leave the terminal on the alternate screen.

use std::io::{self, Write};

use ratatui::layout::Size;

use super::inline::InlineTerminal;

pub struct AltScreen<'a, W: Write> {
    term: &'a mut InlineTerminal<W>,
}

impl<'a, W: Write> AltScreen<'a, W> {
    pub fn enter(term: &'a mut InlineTerminal<W>, screen: Size) -> io::Result<Self> {
        term.enter_alt_screen(screen)?;
        Ok(Self { term })
    }
    pub fn term(&mut self) -> &mut InlineTerminal<W> {
        self.term
    }
}

impl<W: Write> Drop for AltScreen<'_, W> {
    fn drop(&mut self) {
        let _ = self.term.leave_alt_screen();
    }
}
