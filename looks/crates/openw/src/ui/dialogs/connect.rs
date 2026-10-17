// OWNER: dialogs
//! `/connect`: wizard keeps its own credentials, so this lists how each provider is signed in
//! and says the command to run. openw never asks for an API key itself, so no secret is typed
//! into or written by this UI.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::SelectDialog;
use tuikit::select::SelectItem;
use tuikit::Theme;

use super::list::{ListDialog, ListEvent};
use super::panel::Alert;
use super::{Dialog, Nav, Outcome};

/// One way to get a provider working with wizard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Provider {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub popular: bool,
    /// Shell command that does it.
    pub command: &'static str,
    pub how: &'static str,
}

pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "xai",
        title: "xAI",
        description: "(SuperGrok account sign-in)",
        popular: true,
        command: "wizard --login xai",
        how: "Opens your browser for the xAI sign-in and stores the token under ~/.wizard.",
    },
    Provider {
        id: "chatgpt",
        title: "ChatGPT",
        description: "(Plus, Pro or Team account sign-in)",
        popular: true,
        command: "wizard --login chatgpt",
        how: "Opens your browser for the ChatGPT sign-in and stores the token under ~/.wizard.",
    },
    Provider {
        id: "anthropic",
        title: "Anthropic",
        description: "(API key)",
        popular: false,
        command: "wizard setup",
        how: "The setup wizard asks which provider to use and keeps the key in ~/.wizard.",
    },
    Provider {
        id: "openai-compatible",
        title: "OpenAI-compatible",
        description: "(API key or local server)",
        popular: false,
        command: "wizard setup",
        how: "Covers OpenAI, OpenRouter, Groq, vLLM, LM Studio and similar endpoints.",
    },
    Provider {
        id: "local",
        title: "Local model",
        description: "(Ollama or llama.cpp)",
        popular: false,
        command: "wizard setup",
        how: "Setup picks a model that fits the machine and starts the server for you.",
    },
];

pub struct ConnectDialog {
    list: ListDialog<usize>,
}

pub fn items() -> Vec<SelectItem<usize>> {
    let mut v = Vec::new();
    for (pop, label) in [(true, "Popular"), (false, "Providers")] {
        for (i, p) in PROVIDERS
            .iter()
            .enumerate()
            .filter(|(_, p)| p.popular == pop)
        {
            v.push(
                SelectItem::new(i, p.title)
                    .description(p.description)
                    .group(label),
            );
        }
    }
    v
}

pub fn open() -> Box<dyn Dialog> {
    Box::new(ConnectDialog {
        list: ListDialog::new(SelectDialog::new("Connect a provider", items())).centered(),
    })
}

/// The explanation shown after picking a provider.
pub fn instructions(p: &Provider) -> Box<dyn Dialog> {
    Box::new(Alert::new(
        format!("Connect {}", p.title),
        format!(
            "Run this in a terminal: {}\n{}\nThen restart wizard so it picks up the new credentials.",
            p.command, p.how
        ),
    ))
}

impl ConnectDialog {
    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Select(i) => {
                let idx = self.list.items()[i].value;
                Outcome {
                    nav: Nav::Replace(instructions(&PROVIDERS[idx])),
                    effects: Vec::new(),
                }
            }
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for ConnectDialog {
    fn title(&self) -> &str {
        "Connect a provider"
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
