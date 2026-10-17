// OWNER: dialogs
//! `/agents`: `build` and `plan` are frontend agents (plan is wizard's plan mode); the other
//! wizard modes (`sovereign`, `chat`) are listed after them and send `SetMode`.

use agent_core::Request;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::SelectDialog;
use tuikit::select::SelectItem;
use tuikit::Theme;

use super::list::{ListDialog, ListEvent};
use super::{Ctx, Dialog, Effect, Outcome};
use crate::app::AgentKind;

#[derive(Clone, Debug, PartialEq)]
enum Pick {
    Agent(AgentKind),
    Mode(String),
}

pub struct AgentDialog {
    list: ListDialog<Pick>,
    /// The mode wizard is in; a pick of `build` or `plan` switches back to it first.
    default_mode: Option<String>,
    in_default_mode: bool,
}

/// The wizard mode that `build` and `plan` run in.
const BASE_MODE: &str = "genie";

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    let modes: Vec<&String> = ctx
        .config
        .modes
        .iter()
        .filter(|m| m.as_str() != BASE_MODE)
        .collect();
    let in_base = ctx.config.mode.is_empty() || ctx.config.mode == BASE_MODE;
    let agent = ctx.agent.unwrap_or(AgentKind::Build);
    let mut items = Vec::new();
    for a in [AgentKind::Build, AgentKind::Plan] {
        items.push(
            SelectItem::new(Pick::Agent(a), a.title().to_lowercase())
                .description("native")
                .current(in_base && a == agent),
        );
    }
    for m in modes {
        items.push(
            SelectItem::new(Pick::Mode(m.clone()), m.clone())
                .description("wizard mode")
                .current(*m == ctx.config.mode),
        );
    }
    let has_base = ctx.config.modes.iter().any(|m| m == BASE_MODE);
    Box::new(AgentDialog {
        list: ListDialog::new(SelectDialog::new("Select agent", items)).centered(),
        default_mode: has_base.then(|| BASE_MODE.to_string()),
        in_default_mode: in_base,
    })
}

impl AgentDialog {
    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Select(i) => match self.list.items()[i].value.clone() {
                Pick::Agent(a) => {
                    let mut out = Outcome::close();
                    if !self.in_default_mode {
                        if let Some(m) = &self.default_mode {
                            out = out.with(Effect::Request(Request::SetMode(m.clone())));
                        }
                    }
                    out.with(Effect::SetAgent(a))
                }
                Pick::Mode(m) => Outcome::close_with(vec![Effect::Request(Request::SetMode(m))]),
            },
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for AgentDialog {
    fn title(&self) -> &str {
        "Select agent"
    }
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ev = self.list.handle_key(key);
        self.pick(ev)
    }
    fn handle_paste(&mut self, text: &str) -> Outcome {
        let ev = self.list.handle_paste(text);
        self.pick(ev)
    }
    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        let ev = self.list.handle_mouse(ev);
        self.pick(ev)
    }
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.list.draw(buf, screen, theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn ctx(mode: &str) -> Ctx {
        let mut c = Ctx::default();
        c.config.mode = mode.into();
        c.config.modes = vec!["genie".into(), "sovereign".into(), "chat".into()];
        c.agent = Some(AgentKind::Build);
        c
    }

    #[test]
    fn plan_sends_the_agent_effect_only_in_the_base_mode() {
        let mut d = open(&ctx("genie"));
        d.handle_key(key(KeyCode::Down));
        let out = d.handle_key(key(KeyCode::Enter));
        assert_eq!(out.effects, vec![Effect::SetAgent(AgentKind::Plan)]);
    }

    #[test]
    fn leaving_another_mode_goes_back_to_genie_first() {
        let mut d = open(&ctx("sovereign"));
        // the cursor starts on the current item, `sovereign`; go to `build`
        d.handle_key(key(KeyCode::Up));
        d.handle_key(key(KeyCode::Up));
        let out = d.handle_key(key(KeyCode::Enter));
        assert_eq!(
            out.effects,
            vec![
                Effect::Request(Request::SetMode("genie".into())),
                Effect::SetAgent(AgentKind::Build)
            ]
        );
    }

    #[test]
    fn a_mode_row_sends_set_mode() {
        let mut d = open(&ctx("genie"));
        d.handle_key(key(KeyCode::Down));
        d.handle_key(key(KeyCode::Down));
        let out = d.handle_key(key(KeyCode::Enter));
        assert_eq!(
            out.effects,
            vec![Effect::Request(Request::SetMode("sovereign".into()))]
        );
    }
}
