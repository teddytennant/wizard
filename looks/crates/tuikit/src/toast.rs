//! Toast, like `ui/toast.tsx`: one at a time, top right, split border in the variant colour.

use crate::border::SplitBox;
use crate::paint::{pad, put_str};
use crate::theme::{Theme, Variant};
use crate::width::{display_width, wrap};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use std::time::{Duration, Instant};

pub const DEFAULT_DURATION: Duration = Duration::from_millis(5000);

#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub title: Option<String>,
    pub message: String,
    pub variant: Variant,
    pub duration: Duration,
}

impl Toast {
    pub fn new(variant: Variant, message: impl Into<String>) -> Self {
        Self {
            title: None,
            message: message.into(),
            variant,
            duration: DEFAULT_DURATION,
        }
    }

    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn lasting(mut self, d: Duration) -> Self {
        self.duration = d;
        self
    }
}

/// Holds the current toast and its deadline. Time is passed in so tests do not sleep.
#[derive(Debug, Default)]
pub struct ToastState {
    current: Option<(Toast, Instant)>,
}

impl ToastState {
    /// Replace whatever is showing, like opencode (no queue).
    pub fn show(&mut self, toast: Toast, now: Instant) {
        let deadline = now + toast.duration;
        self.current = Some((toast, deadline));
    }

    pub fn error(&mut self, message: impl Into<String>, now: Instant) {
        self.show(Toast::new(Variant::Error, message), now);
    }

    pub fn current(&self) -> Option<&Toast> {
        self.current.as_ref().map(|(t, _)| t)
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.current.as_ref().map(|(_, d)| *d)
    }

    pub fn dismiss(&mut self) {
        self.current = None;
    }

    /// Drop an expired toast. True when the screen needs a redraw.
    pub fn tick(&mut self, now: Instant) -> bool {
        match &self.current {
            Some((_, deadline)) if now >= *deadline => {
                self.current = None;
                true
            }
            _ => false,
        }
    }
}

/// Where a toast lands on a `screen`-sized terminal and the wrapped message lines.
pub struct ToastLayout {
    pub area: Rect,
    pub lines: Vec<String>,
}

const PAD_X: u16 = 2;
const PAD_Y: u16 = 1;

pub fn layout(toast: &Toast, screen: Rect) -> Option<ToastLayout> {
    // opencode: top 2, right 2, maxWidth min(60, width - 6), borders 1 each side, padding 2/1.
    let max_w = 60u16.min(screen.width.saturating_sub(6));
    let chrome_x = 2 + PAD_X * 2;
    if max_w <= chrome_x || screen.height < 2 + 2 * PAD_Y + 1 {
        return None;
    }
    let inner_max = (max_w - chrome_x) as usize;
    let lines = wrap(&toast.message, inner_max);
    let title_w = toast
        .title
        .as_deref()
        .map_or(0, |t| display_width(t).min(inner_max));
    let body_w = lines.iter().map(|l| display_width(l)).max().unwrap_or(0);
    let content_w = body_w.max(title_w).max(1) as u16;
    let width = content_w + chrome_x;
    let title_rows = if toast.title.is_some() { 2 } else { 0 };
    let height = (lines.len() as u16 + title_rows + 2 * PAD_Y).min(screen.height.saturating_sub(2));
    let x = screen.right().saturating_sub(2 + width).max(screen.x);
    let area = Rect::new(x, screen.y + 2, width.min(screen.width), height);
    Some(ToastLayout { area, lines })
}

pub struct ToastView<'a> {
    pub toast: &'a Toast,
    pub theme: &'a Theme,
}

impl Widget for ToastView<'_> {
    fn render(self, screen: Rect, buf: &mut Buffer) {
        let Some(l) = layout(self.toast, screen) else {
            return;
        };
        let t = self.theme;
        let frame = SplitBox::both(t.variant(self.toast.variant), t.background_panel);
        frame.render(l.area, buf);
        let inner = pad(frame.inner(l.area), PAD_X, PAD_Y, PAD_X, PAD_Y);
        let mut y = inner.top();
        if let Some(title) = &self.toast.title {
            put_str(
                buf,
                inner.left(),
                y,
                title,
                Style::new()
                    .fg(t.text)
                    .bg(t.background_panel)
                    .add_modifier(Modifier::BOLD),
                inner,
            );
            y += 2;
        }
        for line in &l.lines {
            put_str(
                buf,
                inner.left(),
                y,
                line,
                Style::new().fg(t.text).bg(t.background_panel),
                inner,
            );
            y += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expires_on_deadline_and_replaces_without_queueing() {
        let t0 = Instant::now();
        let mut s = ToastState::default();
        s.show(Toast::new(Variant::Info, "one"), t0);
        s.show(
            Toast::new(Variant::Error, "two").lasting(Duration::from_secs(1)),
            t0,
        );
        assert_eq!(s.current().unwrap().message, "two");
        assert!(!s.tick(t0 + Duration::from_millis(999)));
        assert!(s.tick(t0 + Duration::from_secs(1)));
        assert!(s.current().is_none());
        assert!(!s.tick(t0 + Duration::from_secs(9)));
    }

    #[test]
    fn layout_hugs_top_right_with_opencode_margins() {
        let l = layout(
            &Toast::new(Variant::Info, "Copied to clipboard"),
            Rect::new(0, 0, 120, 36),
        )
        .unwrap();
        // 19 chars + padding 4 + borders 2 = 25 wide; right gap 2; top gap 2; 1 line + padding 2
        assert_eq!(l.area, Rect::new(93, 2, 25, 3));
    }

    #[test]
    fn long_messages_wrap_at_sixty_columns_total() {
        let msg = "word ".repeat(40);
        let l = layout(&Toast::new(Variant::Warning, msg), Rect::new(0, 0, 120, 36)).unwrap();
        assert_eq!(l.area.width, 60);
        assert!(l.lines.len() > 2);
        assert!(l.lines.iter().all(|x| display_width(x) <= 54));
    }

    #[test]
    fn renders_title_message_and_bars() {
        let theme = Theme::builtin("opencode").unwrap();
        let toast = Toast::new(Variant::Success, "saved").titled("Done");
        let area = Rect::new(0, 0, 40, 12);
        let mut b = Buffer::empty(area);
        ToastView {
            toast: &toast,
            theme: &theme,
        }
        .render(area, &mut b);
        let l = layout(&toast, area).unwrap();
        assert_eq!(b[(l.area.x, l.area.y)].symbol(), "┃");
        assert_eq!(b[(l.area.x, l.area.y)].fg, theme.success);
        let row = |y: u16| -> String {
            (0..area.width)
                .map(|x| b[(x, y)].symbol().to_string())
                .collect()
        };
        assert!(row(l.area.y + 1).contains("Done"));
        assert!(row(l.area.y + 3).contains("saved"));
    }

    #[test]
    fn tiny_screens_show_nothing_instead_of_panicking() {
        let theme = Theme::builtin("opencode").unwrap();
        let toast = Toast::new(Variant::Error, "boom");
        for (w, h) in [(0, 0), (5, 3), (9, 20), (40, 3)] {
            let area = Rect::new(0, 0, w, h);
            let mut b = Buffer::empty(area);
            ToastView {
                toast: &toast,
                theme: &theme,
            }
            .render(area, &mut b);
        }
    }
}
