//! The terminal the look draws on.

use std::io::{self, Stdout};

use ratatui::backend::CrosstermBackend;

/// The crossterm backend that does not ask the terminal where its cursor is. An inline viewport
/// wants that on every resize and clear, and the answer would arrive on the stdin our own input
/// reader owns, so the reply would be eaten and the query would time out. The position is read
/// once, before the reader starts, and tracked from the writes after that. Copied from
/// grokw's: ratatui 0.30's `Terminal::clear` reads the cursor position, and on ctrl+l, ctrl+g
/// or a focus-in that query timed out and ended the look.
pub struct Quiet {
    inner: CrosstermBackend<Stdout>,
    cursor: ratatui::layout::Position,
}

impl Quiet {
    /// `query` reads the cursor now; fullscreen never needs it.
    pub fn new(mut inner: CrosstermBackend<Stdout>, query: bool) -> Quiet {
        use ratatui::backend::Backend;
        let cursor = if query {
            inner.get_cursor_position().unwrap_or_default()
        } else {
            Default::default()
        };
        Quiet { inner, cursor }
    }
}

impl ratatui::backend::Backend for Quiet {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        let mut last = None;
        let content = content.inspect(|(x, y, _)| last = Some((*x, *y)));
        self.inner.draw(content)?;
        if let Some((x, y)) = last {
            self.cursor = ratatui::layout::Position { x, y };
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.cursor.y = self.cursor.y.saturating_add(n);
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<ratatui::layout::Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> io::Result<()> {
        let p = position.into();
        self.cursor = p;
        self.inner.set_cursor_position(p)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<ratatui::layout::Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<ratatui::backend::WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        ratatui::backend::Backend::flush(&mut self.inner)
    }
}

pub type Terminal = ratatui::Terminal<Quiet>;
