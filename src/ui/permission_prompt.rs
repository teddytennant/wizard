//! Claude Code's permission dialog, for the house TUI.
//!
//! A tool call that needs approval parks the turn
//! ([`AgentEvent::PermissionRequest`](crate::agent::AgentEvent)); this holds
//! the question, takes the keys, and draws the box at the bottom of the screen
//! with Claude's three answers: yes, yes and don't ask again, no (and say what
//! to do differently).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use serde_json::Value;
use tokio::sync::oneshot;

use super::{accent, dim, muted, wrap_lines};
use crate::agent::PermissionAnswer;
use crate::hooks::toolmap::{self, ToolClass};
use crate::permissions::Offer;
use crate::theme::{self, Token};

/// One open question, and the channel its answer goes back through.
#[derive(Debug)]
pub struct PermissionPrompt {
    pub tool: String,
    pub args: Value,
    pub reason: String,
    pub offer: Option<Offer>,
    /// Index into [`Self::labels`].
    pub selected: usize,
    /// What has been typed while the last option is selected.
    pub feedback: String,
    respond: Option<oneshot::Sender<PermissionAnswer>>,
}

impl PermissionPrompt {
    pub fn new(
        tool: String,
        args: Value,
        reason: String,
        offer: Option<Offer>,
        respond: oneshot::Sender<PermissionAnswer>,
    ) -> Self {
        Self {
            tool,
            args,
            reason,
            offer,
            selected: 0,
            feedback: String::new(),
            respond: Some(respond),
        }
    }

    /// The answers, in order.
    pub fn labels(&self) -> Vec<String> {
        let mut labels = vec!["Yes".to_string()];
        if let Some(offer) = &self.offer {
            labels.push(offer.label.clone());
        }
        labels.push("No, and tell Claude what to do differently (esc)".to_string());
        labels
    }

    fn deny_index(&self) -> usize {
        self.labels().len() - 1
    }

    /// Send the answer back into the paused turn.
    pub fn answer(&mut self, answer: PermissionAnswer) {
        if let Some(respond) = self.respond.take() {
            let _ = respond.send(answer);
        }
    }

    /// Handle a key. `true` once the prompt is answered and can be dropped.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let deny = self.deny_index();
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => self.answer(PermissionAnswer::Deny { feedback: None }),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.answer(PermissionAnswer::Deny { feedback: None });
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => self.selected = (self.selected + 1).min(deny),
            KeyCode::Enter => return self.commit(),
            KeyCode::Char(digit @ '1'..='9') if plain && self.selected != deny => {
                let at = digit as usize - '1' as usize;
                if at < deny {
                    self.selected = at;
                    return self.commit();
                }
                if at == deny {
                    self.selected = deny;
                }
            }
            KeyCode::Char('y') if plain && self.selected != deny => {
                self.selected = 0;
                return self.commit();
            }
            KeyCode::Char('n') if plain && self.selected != deny => {
                self.selected = deny;
            }
            KeyCode::Backspace if self.selected == deny => {
                self.feedback.pop();
            }
            KeyCode::Char(c) if plain && self.selected == deny => self.feedback.push(c),
            _ => {}
        }
        self.respond.is_none()
    }

    fn commit(&mut self) -> bool {
        let deny = self.deny_index();
        let answer = if self.selected == 0 {
            PermissionAnswer::Allow { remember: false }
        } else if self.selected == deny {
            let text = self.feedback.trim();
            PermissionAnswer::Deny {
                feedback: (!text.is_empty()).then(|| text.to_string()),
            }
        } else {
            PermissionAnswer::Allow { remember: true }
        };
        self.answer(answer);
        true
    }

    /// The heading Claude puts over the tool's own detail.
    fn heading(&self) -> &'static str {
        if toolmap::claude_name(&self.tool) == "Workflow" {
            return super::workflow_panel::approval(&self.args).0;
        }
        match toolmap::class(&self.tool) {
            ToolClass::Shell => "Bash command",
            ToolClass::Edit => "Edit file",
            ToolClass::Web => "Fetch",
            ToolClass::Mcp => "Tool use",
            _ => "Tool use",
        }
    }

    /// What the call is, as lines of text: the command, the file and the first
    /// lines of what is written to it, the URL.
    fn detail(&self) -> Vec<String> {
        let text = |key: &str| self.args.get(key).and_then(Value::as_str);
        if toolmap::claude_name(&self.tool) == "Workflow" {
            return super::workflow_panel::approval(&self.args).1;
        }
        match toolmap::class(&self.tool) {
            ToolClass::Shell => {
                let mut out: Vec<String> = text("command")
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect();
                if let Some(description) = text("description") {
                    out.push(description.to_string());
                }
                out
            }
            ToolClass::Edit => {
                let path = toolmap::paths(&self.tool, &self.args)
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let mut out = vec![path];
                if let Some(body) = text("content").or_else(|| text("new_string")) {
                    out.extend(body.lines().take(6).map(|line| format!("+ {line}")));
                }
                out
            }
            ToolClass::Web => vec![
                text("url")
                    .or_else(|| text("query"))
                    .unwrap_or_default()
                    .to_string(),
            ],
            _ => {
                let shown = serde_json::to_string(&self.args).unwrap_or_default();
                vec![toolmap::claude_name(&self.tool), shown]
            }
        }
    }
}

/// Draw the dialog over the bottom of the frame.
pub fn draw(frame: &mut Frame, prompt: &PermissionPrompt) {
    let area = frame.area();
    let width = area.width.max(20);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        prompt.heading().to_string(),
        theme::style(Token::Heading).bold(),
    )));
    lines.push(Line::default());
    for row in prompt.detail() {
        lines.push(Line::from(Span::styled(format!("  {row}"), muted())));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::raw("Do you want to proceed?")));
    let deny = prompt.deny_index();
    for (at, label) in prompt.labels().into_iter().enumerate() {
        let picked = at == prompt.selected;
        let marker = if picked { "❯" } else { " " };
        let mut text = format!("{marker} {}. {label}", at + 1);
        if at == deny && picked && !prompt.feedback.is_empty() {
            text = format!("{marker} {}. {}", at + 1, prompt.feedback);
        }
        let style = if picked { accent().bold() } else { dim() };
        lines.push(Line::from(Span::styled(text, style)));
    }
    let wrapped = wrap_lines(Text::from(lines), width.saturating_sub(4) as usize);
    let height = (wrapped.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x,
        y: area.bottom().saturating_sub(height),
        width: width.min(area.width),
        height,
    }
    .intersection(area);
    if rect.height < 3 || rect.width < 12 {
        return;
    }
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::style(Token::Warning))
        .title_bottom(Line::from(Span::styled(" enter confirm · esc no ", dim())).centered());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let skip = wrapped.len().saturating_sub(inner.height as usize);
    frame.render_widget(
        Paragraph::new(Text::from(
            wrapped.into_iter().skip(skip).collect::<Vec<_>>(),
        )),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEventKind;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        let mut event = KeyEvent::new(code, KeyModifiers::NONE);
        event.kind = KeyEventKind::Press;
        event
    }

    fn open(offer: bool) -> (PermissionPrompt, oneshot::Receiver<PermissionAnswer>) {
        let (send, receive) = oneshot::channel();
        let offer = offer.then(|| Offer {
            label: "Yes, and don't ask again for git commit:* commands".to_string(),
            rules: vec!["Bash(git commit:*)".to_string()],
            scope: crate::permissions::Scope::Project,
        });
        let prompt = PermissionPrompt::new(
            "execute".to_string(),
            serde_json::json!({"command": "git commit -m x"}),
            "needs approval".to_string(),
            offer,
            send,
        );
        (prompt, receive)
    }

    #[test]
    fn enter_on_the_first_option_allows_once() {
        let (mut prompt, mut answer) = open(true);
        assert!(prompt.handle_key(key(KeyCode::Enter)));
        assert_eq!(
            answer.try_recv().unwrap(),
            PermissionAnswer::Allow { remember: false }
        );
    }

    #[test]
    fn the_second_option_remembers() {
        let (mut prompt, mut answer) = open(true);
        assert!(prompt.handle_key(key(KeyCode::Char('2'))));
        assert_eq!(
            answer.try_recv().unwrap(),
            PermissionAnswer::Allow { remember: true }
        );
    }

    #[test]
    fn without_an_offer_two_is_no_and_typing_is_the_feedback() {
        let (mut prompt, mut answer) = open(false);
        assert_eq!(prompt.labels().len(), 2);
        assert!(!prompt.handle_key(key(KeyCode::Char('2'))));
        assert_eq!(prompt.selected, 1);
        for c in "use rg".chars() {
            prompt.handle_key(key(KeyCode::Char(c)));
        }
        assert!(prompt.handle_key(key(KeyCode::Enter)));
        assert_eq!(
            answer.try_recv().unwrap(),
            PermissionAnswer::Deny {
                feedback: Some("use rg".to_string())
            }
        );
    }

    #[test]
    fn escape_refuses_without_feedback() {
        let (mut prompt, mut answer) = open(true);
        assert!(prompt.handle_key(key(KeyCode::Esc)));
        assert_eq!(
            answer.try_recv().unwrap(),
            PermissionAnswer::Deny { feedback: None }
        );
    }

    #[test]
    fn arrows_move_and_clamp() {
        let (mut prompt, _answer) = open(true);
        prompt.handle_key(key(KeyCode::Down));
        prompt.handle_key(key(KeyCode::Down));
        prompt.handle_key(key(KeyCode::Down));
        assert_eq!(prompt.selected, 2);
        prompt.handle_key(key(KeyCode::Up));
        assert_eq!(prompt.selected, 1);
    }
}
