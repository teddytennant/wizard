// OWNER: dialogs
//! The padded dialogs that are not lists: `DialogAlert`, `DialogConfirm`, `DialogPrompt` and
//! the shared header. These use `paddingLeft/Right 2` and `gap 1`, unlike the list dialogs'
//! 4-column inset.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::dialog::{self, DialogLayout};
use tuikit::editor::{Editor, EditorStyle};
use tuikit::paint::{fill, pad, put_str};
use tuikit::width::{display_width, wrap};
use tuikit::Theme;

use super::{Dialog, Effect, Outcome};

/// Paint the frame and the header row; returns the body area starting under the header's gap.
/// `content_h` counts everything below the top padding row, header and trailing padding included.
pub fn frame(
    buf: &mut Buffer,
    screen: Rect,
    theme: &Theme,
    width: u16,
    content_h: u16,
    title: &str,
    hint: &str,
) -> (DialogLayout, Rect) {
    let fr = dialog::render_frame(buf, screen, theme, width, content_h);
    let inner = pad(fr.inner, 2, 0, 2, 0);
    let panel = theme.background_panel;
    put_str(
        buf,
        inner.x,
        inner.y,
        title,
        Style::new()
            .fg(theme.text)
            .bg(panel)
            .add_modifier(Modifier::BOLD),
        inner,
    );
    let hw = display_width(hint) as u16;
    if hw < inner.width {
        put_str(
            buf,
            inner.right() - hw,
            inner.y,
            hint,
            Style::new().fg(theme.text_muted).bg(panel),
            inner,
        );
    }
    let body = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(2),
    );
    (fr, body)
}

/// Paint a `paddingLeft 3, paddingRight 3` button with `label` ending at `right`.
pub fn ok_button(buf: &mut Buffer, theme: &Theme, right: u16, y: u16, clip: Rect) {
    let bx = right.saturating_sub(8);
    let btn = Rect::new(bx, y, 8, 1);
    fill(buf, btn, Style::new().bg(theme.primary));
    put_str(
        buf,
        bx + 3,
        y,
        "ok",
        Style::new()
            .fg(theme.selected_list_item_text)
            .bg(theme.primary),
        clip,
    );
}

fn paragraphs(message: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for p in message.split('\n') {
        out.extend(wrap(p, width.max(1)));
    }
    out
}

// ---- alert -------------------------------------------------------------------------------

pub struct Alert {
    pub title: String,
    pub message: String,
    /// `esc` for an alert, `esc/enter` for help.
    pub hint: &'static str,
    pub on_ok: Vec<Effect>,
    /// Where the `ok` button was last drawn.
    ok_rect: Rect,
}

impl Alert {
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Alert {
            title: title.into(),
            message: message.into(),
            hint: "esc",
            on_ok: Vec::new(),
            ok_rect: Rect::default(),
        }
    }
}

fn clicked(r: Rect, ev: &MouseEvent) -> bool {
    matches!(ev.kind, MouseEventKind::Up(MouseButton::Left))
        && ev.column >= r.x
        && ev.column < r.right()
        && ev.row >= r.y
        && ev.row < r.bottom()
}

impl Dialog for Alert {
    fn title(&self) -> &str {
        &self.title
    }

    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => Outcome::close(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Outcome::close(),
            KeyCode::Enter => Outcome::close_with(self.on_ok.clone()),
            _ => Outcome::stay(),
        }
    }

    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        if clicked(self.ok_rect, &ev) {
            Outcome::close_with(self.on_ok.clone())
        } else {
            Outcome::stay()
        }
    }

    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        let w = dialog::MEDIUM.min(screen.width.saturating_sub(2));
        let rows = paragraphs(&self.message, w.saturating_sub(4) as usize);
        // header, gap, message, its bottom padding, gap, button, button padding
        let content_h = 1 + 1 + rows.len() as u16 + 1 + 1 + 1 + 1;
        let (_, body) = frame(
            buf,
            screen,
            theme,
            dialog::MEDIUM,
            content_h,
            &self.title,
            self.hint,
        );
        let st = Style::new().fg(theme.text_muted).bg(theme.background_panel);
        for (i, l) in rows.iter().enumerate() {
            put_str(buf, body.x, body.y + i as u16, l, st, body);
        }
        let by = body.y + rows.len() as u16 + 2;
        ok_button(buf, theme, body.right(), by, body);
        self.ok_rect = Rect::new(body.right().saturating_sub(8), by, 8, 1);
        None
    }
}

// ---- confirm -----------------------------------------------------------------------------

pub struct Confirm {
    pub title: String,
    pub message: String,
    /// Replaces `Cancel` as the label of the negative button.
    pub label: Option<String>,
    pub on_confirm: Vec<Effect>,
    /// `true` while `Confirm` is the active button; it starts there.
    confirm_active: bool,
    /// Cancel and Confirm as last drawn.
    rects: [Rect; 2],
}

impl Confirm {
    pub fn new(
        title: impl Into<String>,
        message: impl Into<String>,
        on_confirm: Vec<Effect>,
    ) -> Self {
        Confirm {
            title: title.into(),
            message: message.into(),
            label: None,
            on_confirm,
            confirm_active: true,
            rects: [Rect::default(); 2],
        }
    }
}

impl Dialog for Confirm {
    fn title(&self) -> &str {
        &self.title
    }

    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => Outcome::close(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Outcome::close(),
            KeyCode::Left | KeyCode::Right => {
                self.confirm_active = !self.confirm_active;
                Outcome::stay()
            }
            KeyCode::Enter => {
                if self.confirm_active {
                    Outcome::close_with(self.on_confirm.clone())
                } else {
                    Outcome::close()
                }
            }
            _ => Outcome::stay(),
        }
    }

    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        if clicked(self.rects[0], &ev) {
            Outcome::close()
        } else if clicked(self.rects[1], &ev) {
            Outcome::close_with(self.on_confirm.clone())
        } else {
            Outcome::stay()
        }
    }

    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        let w = dialog::MEDIUM.min(screen.width.saturating_sub(2));
        let rows = paragraphs(&self.message, w.saturating_sub(4) as usize);
        let content_h = 1 + 1 + rows.len() as u16 + 1 + 1 + 1 + 1;
        let (_, body) = frame(
            buf,
            screen,
            theme,
            dialog::MEDIUM,
            content_h,
            &self.title,
            "esc",
        );
        let st = Style::new().fg(theme.text_muted).bg(theme.background_panel);
        for (i, l) in rows.iter().enumerate() {
            put_str(buf, body.x, body.y + i as u16, l, st, body);
        }
        let by = body.y + rows.len() as u16 + 2;
        let cancel = format!(
            " {} ",
            title_case(self.label.as_deref().unwrap_or("cancel"))
        );
        let confirm = " Confirm ".to_string();
        let total = (display_width(&cancel) + display_width(&confirm)) as u16;
        let mut x = body.right().saturating_sub(total);
        for (i, (text, active)) in [
            (cancel, !self.confirm_active),
            (confirm, self.confirm_active),
        ]
        .into_iter()
        .enumerate()
        {
            let w = display_width(&text) as u16;
            let r = Rect::new(x, by, w, 1);
            self.rects[i] = r;
            let style = if active {
                Style::new()
                    .fg(theme.selected_list_item_text)
                    .bg(theme.primary)
            } else {
                Style::new().fg(theme.text_muted).bg(theme.background_panel)
            };
            fill(buf, r, style);
            put_str(buf, x, by, &text, style, body);
            x += w;
        }
        None
    }
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

// ---- prompt ------------------------------------------------------------------------------

/// `DialogPrompt`: a title, a 3-row text area and `enter submit`.
pub struct Prompt {
    pub title: String,
    pub placeholder: String,
    pub editor: Editor,
    pub submit: Box<dyn Fn(String) -> Effect>,
    /// A line of muted text above the area.
    pub description: Option<String>,
}

impl Prompt {
    pub fn new(
        title: impl Into<String>,
        value: &str,
        placeholder: &str,
        submit: impl Fn(String) -> Effect + 'static,
    ) -> Self {
        Prompt {
            title: title.into(),
            placeholder: placeholder.into(),
            editor: Editor::with_text(value),
            submit: Box::new(submit),
            description: None,
        }
    }
}

impl Dialog for Prompt {
    fn title(&self) -> &str {
        &self.title
    }

    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => Outcome::close(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Outcome::close(),
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                let v = self.editor.text().trim().to_string();
                if v.is_empty() {
                    Outcome::stay()
                } else {
                    Outcome::close_with(vec![(self.submit)(v)])
                }
            }
            _ => {
                self.editor.apply_key(key);
                Outcome::stay()
            }
        }
    }

    fn handle_paste(&mut self, text: &str) -> Outcome {
        self.editor.paste(text);
        Outcome::stay()
    }

    fn handle_mouse(&mut self, _ev: MouseEvent) -> Outcome {
        Outcome::stay()
    }

    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        // header, gap, [description, gap], 3-row area, gap, hint, bottom padding
        let desc = u16::from(self.description.is_some()) * 2;
        let content_h = 1 + 1 + desc + 3 + 1 + 1 + 1;
        let (_, body) = frame(
            buf,
            screen,
            theme,
            dialog::MEDIUM,
            content_h,
            &self.title,
            "esc",
        );
        let panel = theme.background_panel;
        let mut y = body.y;
        if let Some(d) = &self.description {
            put_str(
                buf,
                body.x,
                y,
                d,
                Style::new().fg(theme.text_muted).bg(panel),
                body,
            );
            y += 2;
        }
        let area = Rect::new(body.x, y, body.width, 3);
        let style = EditorStyle {
            text: Style::new().fg(theme.text).bg(panel),
            placeholder: Some((
                self.placeholder.clone(),
                Style::new().fg(theme.text_muted).bg(panel),
            )),
        };
        let info = self.editor.render(area, buf, &style);
        let hy = y + 4;
        let x = put_str(
            buf,
            body.x,
            hy,
            "enter",
            Style::new().fg(theme.text).bg(panel),
            body,
        );
        put_str(
            buf,
            x,
            hy,
            " submit",
            Style::new().fg(theme.text_muted).bg(panel),
            body,
        );
        info.cursor
    }
}
