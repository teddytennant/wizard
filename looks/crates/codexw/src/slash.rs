//! Slash commands that need more than a message: the pickers and cells behind `/skills`,
//! `/theme`, `/raw`, `/copy` and the rest. `commands.rs` holds the table and the dispatch;
//! each command that opens something lands here as an `impl App` method.

use std::io::Write;

use crate::app::App;
use crate::ui::AppAction;
use crate::ui::history_cells::ErrorCell;
use crate::ui::pickers::selection::{
    ItemAction, ListSelectionView, SelectionItem, SelectionParams,
};

/// The first row of `/statusline`: not an item, the switch for theme colours.
const USE_THEME_COLORS: &str = "status-line-use-theme-colors";

impl<W: Write> App<W> {
    /// `/skills`: Codex's two-row menu. Listing opens the `$` popup; wizard has no per-skill
    /// switch, so the second row says so instead of faking a toggle.
    pub fn skills_command(&mut self) {
        let items = vec![
            SelectionItem::new(
                "List skills",
                ItemAction::Close(AppAction::InsertText("$".into())),
            )
            .described("Tip: press @ to open this list directly."),
            SelectionItem::new(
                "Enable/Disable Skills",
                ItemAction::Close(AppAction::Error(
                    "Enabling and disabling skills is not supported by wizard. Skills load from ~/.wizard/skills and .wizard/skills; move a folder out to turn it off."
                        .into(),
                )),
            )
            .described("Enable or disable skills."),
        ];
        let params = SelectionParams {
            title: Some("Skills".into()),
            subtitle: Some("Choose an action".into()),
            items,
            ..Default::default()
        };
        self.pane
            .push_view(Box::new(ListSelectionView::new(params)));
        self.request_draw();
    }

    /// Enter on a prompt in the backtrack preview (or `/rewind <n>`): find the wizard turn it
    /// started and ask before rewinding, since wizard also puts the edited files back.
    pub fn rewind_prompt(&mut self, nth: usize, text: String) {
        let prompts: Vec<String> = self
            .cells
            .iter()
            .filter(|c| c.is_user_message())
            .map(|c| {
                c.raw_lines()
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect();
        let markers = self
            .home
            .as_deref()
            .filter(|_| !self.session_id.is_empty())
            .map(|h| {
                crate::rewind::markers(
                    &std::path::PathBuf::from(h)
                        .join(".wizard/sessions")
                        .join(format!("{}.jsonl", self.session_id)),
                )
            })
            .unwrap_or_default();
        let turn = crate::rewind::turns_for(&prompts, &markers)
            .get(nth)
            .copied()
            .flatten();
        let Some(turn) = turn else {
            self.pane.composer.set_text(&text);
            self.say_error(
                "Failed to branch before the selected prompt: wizard has no turn marker for it (it was typed as a command, or the session was written by an older wizard). The prompt is back in the composer.",
            );
            return;
        };
        self.confirm_rewind(turn, text);
    }

    pub fn confirm_rewind(&mut self, turn: u64, text: String) {
        let items = vec![
            SelectionItem::new(
                "Yes, rewind",
                ItemAction::Close(AppAction::Rewind { turn, text }),
            )
            .described("Restore the files its edits touched and cut the conversation here"),
            SelectionItem::new("No, keep the conversation", ItemAction::None)
                .described("Return to the current session"),
        ];
        let params = SelectionParams {
            title: Some("Rewind to before this message?".into()),
            subtitle: Some(
                "Wizard puts back the files the agent edited and deletes everything after it from the session."
                    .into(),
            ),
            items,
            initial_selected_idx: Some(1),
            ..Default::default()
        };
        self.pane
            .push_view(Box::new(ListSelectionView::new(params)));
        self.request_draw();
    }

    /// Confirmed: wizard's `/rewind <turn>` runs as a command turn.
    pub fn start_rewind(&mut self, turn: u64, text: String) {
        self.rewind = Some(crate::app::RewindFlow { text, answer: None });
        let _ = self
            .tx
            .send(agent_core::Request::Prompt(format!("/rewind {turn}")));
    }

    /// The `/rewind` turn ended: load the cut session, or say why it did not happen.
    pub fn finish_rewind(&mut self) {
        let Some(flow) = self.rewind.take() else {
            return;
        };
        let ok = flow.answer.as_deref().is_some_and(crate::rewind::succeeded);
        if ok {
            self.switch = Some(crate::app::SessionSwitch {
                clear: true,
                load: true,
            });
            self.prefill_after_load = Some(flow.text);
            let _ = self
                .tx
                .send(agent_core::Request::LoadSession(self.session_id.clone()));
        } else {
            self.pane.composer.set_text(&flow.text);
            let why = flow
                .answer
                .unwrap_or_else(|| "wizard did not answer".to_string());
            self.say_error(format!(
                "Failed to branch before the selected prompt: {why}"
            ));
        }
    }

    /// `/rewind [turn]`: the backtrack picker, or straight to the confirmation for a wizard turn.
    pub fn rewind_command(&mut self, rest: &str) {
        if rest.is_empty() {
            self.backtrack();
            return;
        }
        match rest.parse::<u64>() {
            Ok(turn) => self.confirm_rewind(turn, String::new()),
            Err(_) => self.say_error("Usage: /rewind [turn]"),
        }
    }

    /// `/theme`: the picker with its live preview.
    pub fn theme_command(&mut self) {
        let dir = self
            .home
            .as_deref()
            .map(|h| std::path::PathBuf::from(h).join(".config/codexw"));
        let current = dir
            .as_ref()
            .and_then(|d| crate::config::get(&d.join("config.toml"), "theme"));
        let shown = self.home.as_deref().map(|_| "~/.config/codexw".to_string());
        let params = crate::ui::pickers::theme::theme_picker_params(
            current.as_deref(),
            dir.as_deref(),
            shown.as_deref(),
            self.screen.width,
        );
        self.pane
            .push_view(Box::new(ListSelectionView::new(params)));
        self.request_draw();
    }

    /// Enter in the theme picker: keep the theme and write it to the config file.
    pub fn set_syntax_theme_choice(&mut self, name: String) {
        let dir = self
            .home
            .as_deref()
            .map(|h| std::path::PathBuf::from(h).join(".config/codexw"));
        if let Some(why) =
            crate::highlight::set_configured_theme(Some(name.clone()), dir.as_deref())
        {
            self.say_error(why);
            return;
        }
        if let Some(d) = &dir {
            if let Err(e) = crate::config::set(&d.join("config.toml"), "theme", Some(&name)) {
                self.say_error(format!("Failed to save the theme: {e}"));
            }
        }
        self.request_draw();
    }

    /// A line of history made of ready-made rows, after the echoed command when there is one.
    pub fn push_output(&mut self, echo: Option<&str>, lines: Vec<ratatui::text::Line<'static>>) {
        use ratatui::style::Color;
        use ratatui::text::{Line, Span};
        let mut all: Vec<Line<'static>> = Vec::new();
        if let Some(c) = echo {
            all.push(Line::from(Span::styled(
                c.to_string(),
                ratatui::style::Style::default().fg(Color::Magenta),
            )));
            all.push(Line::default());
        }
        all.extend(lines);
        self.push_cell(Box::new(crate::ui::pickers::PlainLinesCell { lines: all }));
    }

    /// Run a wizard slash command as a command turn: no echo, the answer comes back as a notice.
    pub fn wizard_command(&mut self, cmd: String) {
        let _ = self.tx.send(agent_core::Request::Prompt(cmd));
    }

    /// `/mcp [verbose]`: the servers wizard is configured with.
    pub fn mcp_command(&mut self, rest: &str) {
        let verbose = match rest.to_ascii_lowercase().as_str() {
            "" => false,
            "verbose" => true,
            _ => {
                self.say_error("Usage: /mcp [verbose]");
                return;
            }
        };
        let servers = self
            .home
            .as_deref()
            .map(crate::outputs::mcp_servers)
            .unwrap_or_default();
        let echo = if verbose { "/mcp verbose" } else { "/mcp" };
        self.push_output(Some(echo), crate::outputs::mcp_lines(&servers, verbose));
    }

    /// `/hooks`: the events screen.
    pub fn hooks_command(&mut self) {
        let (global, project) = crate::outputs::hook_tables(self.home.as_deref(), &self.opts.cwd);
        self.pane
            .push_view(Box::new(crate::ui::pickers::hooks::HooksView::new(
                global, project,
            )));
        self.request_draw();
    }

    /// `/debug-config`: the config files in play.
    pub fn debug_config_command(&mut self) {
        let lines = crate::outputs::debug_config_lines(self.home.as_deref(), &self.opts.cwd);
        self.push_output(Some("/debug-config"), lines);
    }

    /// `/copy` and Ctrl+O: the last answer to the clipboard.
    pub fn copy_last_message(&mut self) {
        if self.last_assistant_text.trim().is_empty() {
            self.say_error("No agent response to copy");
            return;
        }
        match crate::clipboard::copy(&self.last_assistant_text) {
            Ok(()) => self.push_cell(Box::new(crate::ui::history_cells::InfoCell {
                text: "Copied last message to clipboard".into(),
                hint: None,
            })),
            Err(e) => self.say_error(format!("Copy failed: {e}")),
        }
    }

    /// `/statusline`: pick and order the items of the footer's status line.
    pub fn statusline_command(&mut self) {
        use crate::statusline::{STATUS_ITEMS, canonical};
        use crate::ui::pickers::multi_select::{MultiItem, MultiSelectView};
        let mut items = vec![MultiItem {
            id: USE_THEME_COLORS.into(),
            name: "Use theme colors".into(),
            description: Some("Apply colors from the active /theme".into()),
            enabled: self.pane.footer.status_colors,
            orderable: false,
            section_break_after: true,
        }];
        let mut seen: Vec<String> = Vec::new();
        for id in &self.pane.footer.status_items {
            let id = canonical(id).to_string();
            if let Some(def) = STATUS_ITEMS.iter().find(|d| d.id == id) {
                if !seen.contains(&id) {
                    items.push(MultiItem::new(def.id, def.description, true));
                    seen.push(id);
                }
            }
        }
        for def in STATUS_ITEMS
            .iter()
            .filter(|d| !seen.iter().any(|s| s == d.id))
        {
            items.push(MultiItem::new(def.id, def.description, false));
        }
        let values = self.status_values();
        let preview: crate::ui::pickers::multi_select::Preview = std::sync::Arc::new(move |its| {
            let colors = its
                .iter()
                .find(|i| i.id == USE_THEME_COLORS)
                .is_none_or(|i| i.enabled);
            let ids: Vec<String> = its
                .iter()
                .filter(|i| i.enabled && i.id != USE_THEME_COLORS)
                .map(|i| i.id.clone())
                .collect();
            crate::statusline::preview_line(&ids, colors, &values)
        });
        let confirm: crate::ui::pickers::multi_select::Confirm =
            std::sync::Arc::new(|ids| AppAction::StatusLineSet {
                colors: ids.iter().any(|i| i == USE_THEME_COLORS),
                items: ids
                    .iter()
                    .filter(|i| *i != USE_THEME_COLORS)
                    .cloned()
                    .collect(),
            });
        let view = MultiSelectView::new(
            "Configure Status Line",
            "Select which items to display in the status line.",
            items,
            true,
            Some(preview),
            confirm,
        );
        self.pane.push_view(Box::new(view));
        self.request_draw();
    }

    /// `/title`: the same for the terminal title.
    pub fn title_command(&mut self) {
        use crate::statusline::{TITLE_ITEMS, canonical};
        use crate::ui::pickers::multi_select::{MultiItem, MultiSelectView};
        let mut items: Vec<MultiItem> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for id in &self.title_items {
            let id = canonical(id).to_string();
            if let Some(def) = TITLE_ITEMS.iter().find(|d| d.id == id) {
                if !seen.contains(&id) {
                    items.push(MultiItem::new(def.id, def.description, true));
                    seen.push(id);
                }
            }
        }
        for def in TITLE_ITEMS
            .iter()
            .filter(|d| !seen.iter().any(|s| s == d.id))
        {
            items.push(MultiItem::new(def.id, def.description, false));
        }
        let mut values = self.status_values();
        if values.project.is_none() {
            values.project = self
                .opts
                .cwd
                .file_name()
                .map(|s| s.to_string_lossy().to_string());
        }
        let preview: crate::ui::pickers::multi_select::Preview = std::sync::Arc::new(move |its| {
            let ids: Vec<String> = its
                .iter()
                .filter(|i| i.enabled)
                .map(|i| i.id.clone())
                .collect();
            let text = if ids
                .iter()
                .any(|i| crate::statusline::canonical(i) == "activity")
            {
                crate::statusline::action_required_title(
                    "[ ! ] Action Required",
                    &ids,
                    &[],
                    &values,
                )
            } else {
                crate::statusline::title_text(&ids, None, &values)
            };
            (!text.is_empty()).then(|| ratatui::text::Line::from(text))
        });
        let confirm: crate::ui::pickers::multi_select::Confirm =
            std::sync::Arc::new(|ids| AppAction::TitleSet(ids.to_vec()));
        let view = MultiSelectView::new(
            "Configure Terminal Title",
            "Select which items to display in the terminal title.",
            items,
            true,
            Some(preview),
            confirm,
        );
        self.pane.push_view(Box::new(view));
        self.request_draw();
    }

    /// Confirmed in `/statusline`: use it and keep it in the config file.
    pub fn set_status_line(&mut self, items: Vec<String>, colors: bool) {
        self.pane.footer.status_line_enabled = !items.is_empty();
        self.pane.footer.status_items = items.clone();
        self.pane.footer.status_colors = colors;
        if let Some(h) = &self.home {
            let s = crate::statusline::Settings { items, colors };
            if let Err(e) = crate::statusline::save_status(&crate::config::path(h), &s) {
                self.say_error(format!("Failed to save the status line: {e}"));
            }
        }
        self.request_draw();
    }

    /// Confirmed in `/title`.
    pub fn set_title_items(&mut self, items: Vec<String>) {
        self.title_items = items.clone();
        if let Some(h) = &self.home {
            if let Err(e) = crate::statusline::save_title(&crate::config::path(h), &items) {
                self.say_error(format!("Failed to save the terminal title: {e}"));
            }
        }
        self.reset_title();
        self.request_draw();
    }

    /// An error row in history, for commands wizard cannot back.
    pub fn say_error(&mut self, text: impl Into<String>) {
        self.push_cell(Box::new(ErrorCell { text: text.into() }));
    }
}
