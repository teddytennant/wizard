//! The right rail: git, the context meter, the subagent runs, and the todos.
//!
//! Four groups of rows against the window edge, in the order a person asks
//! about them. Nothing here is a card floating in space; the spec is explicit
//! that the rail is "a hairline and groups of rows".
//!
//! # Where each number comes from, and why they are not the same number
//!
//! Two token counts sit two rows apart and they mean different things, which is
//! exactly why the browser GUI had to word them carefully and why they are
//! separate fields here rather than one:
//!
//! - **Context** is what the *next* model call will load, from
//!   [`AgentEvent::ContextSize`] and from the prompt half of
//!   [`AgentEvent::Usage`]. It goes **down** when history is compacted or
//!   cleared. It is the one with the bar, because it has a ceiling.
//! - **Spend** is the session's lifetime total, accumulated from every `Usage`.
//!   It only ever goes up, it has no ceiling, and a bar for it would be a lie.
//!
//! [`TranscriptModel`](crate::transcript::TranscriptModel) drops both — it is
//! the conversation, and these are facts about the account — so the rail keeps
//! them.
//!
//! # The meter hides rather than inventing a denominator
//!
//! A provider that names no context window gets no bar and no percentage, only
//! the count. A bar drawn against a guessed 128k would be a number the user
//! could act on and that nothing in the system stands behind.

use iced::widget::{column, container, row, text};
use iced::{Element, Length, Padding};

use crate::agent::AgentEvent;
use crate::gui::git::GitStatus;
use crate::gui::tasks::TodoRow;
use crate::native::subagent::{self, Rail as Subagents};
use crate::native::theme::Palette;
use crate::native::widget::chrome;
use crate::theme::Token;

/// The rail's own width, per the design spec.
pub const WIDTH: f32 = 300.0;

/// What the rail can be told.
#[derive(Debug, Clone)]
pub enum Message {
    /// A changed file was clicked: show its diff.
    ShowDiff(String),
    /// A subagent row was clicked: show its run.
    ShowRun(u64),
    /// A finished run's dismiss control was clicked.
    DismissRun(u64),
    /// Fold the changed-file list.
    ToggleFiles,
    /// Fold the branch list under the branch chip.
    ToggleBranches,
    /// Check this branch out in the chat's own directory.
    Checkout(String),
}

/// The counters the transcript model does not keep.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Meter {
    /// Tokens the next model call would load.
    pub context: Option<u64>,
    /// The active model's window, when the provider names one.
    pub window: Option<u32>,
    /// Session-lifetime prompt tokens.
    pub prompt: u64,
    /// Session-lifetime completion tokens.
    pub completion: u64,
}

impl Meter {
    /// Fold one event in. Returns whether anything moved.
    pub fn apply(&mut self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.prompt += prompt_tokens;
                self.completion += completion_tokens;
                // The prompt this call ran on *is* what the next one loads.
                // A provider that reported only completion tokens sends 0
                // here, which is not a context size.
                if *prompt_tokens > 0 {
                    self.context = Some(*prompt_tokens);
                }
                true
            }
            AgentEvent::ContextSize { tokens } => {
                self.context = Some(*tokens);
                true
            }
            _ => false,
        }
    }

    /// How full the window is, when there is a window to be full of.
    pub fn percent(&self) -> Option<u32> {
        let (tokens, window) = (self.context?, self.window?);
        if window == 0 {
            return None;
        }
        Some(((tokens * 100) / u64::from(window)).min(100) as u32)
    }
}

/// The whole rail's state.
#[derive(Default)]
pub struct Rail {
    pub meter: Meter,
    pub todos: Vec<TodoRow>,
    /// Hidden by `/todos`, and stays hidden while later frames update it.
    pub todos_hidden: bool,
    pub git: Option<GitStatus>,
    pub files_open: bool,
    /// Local branches, most recently committed first — read once when the
    /// chip is opened rather than on the git timer, because listing refs is a
    /// second `git` process and nobody is reading the answer while the chip is
    /// shut.
    pub branches: Vec<String>,
    pub branches_open: bool,
    pub subagents: Subagents,
}

impl Rail {
    pub fn update(&mut self, message: &Message) {
        match message {
            Message::ToggleFiles => self.files_open = !self.files_open,
            Message::ToggleBranches => self.branches_open = !self.branches_open,
            _ => {}
        }
    }

    /// A branch list landed. Closing the chip on an empty answer keeps a
    /// checkout from a stale list: a directory that is not a repository, or a
    /// `git` that failed, has no branches to offer and should not look like it
    /// is still loading.
    pub fn branches_loaded(&mut self, branches: Vec<String>) {
        self.branches_open = !branches.is_empty();
        self.branches = branches;
    }

    /// Fold an event into everything the rail keeps.
    pub fn apply(&mut self, event: &AgentEvent) -> bool {
        let mut moved = self.meter.apply(event);
        moved |= self.subagents.apply(event);
        if let AgentEvent::TodoUpdated(items) = event {
            self.todos = items
                .iter()
                .map(|item| TodoRow {
                    text: item.content.clone(),
                    done: item.status == crate::tools::todo::TodoStatus::Completed,
                    active: item.status == crate::tools::todo::TodoStatus::InProgress,
                })
                .collect();
            moved = true;
        }
        moved
    }

    /// `true` when every group the rail can draw is absent.
    ///
    /// The window asks before reserving [`WIDTH`] for it. A rail with nothing
    /// in it used to take its 300 px anyway, so a fresh chat on an 800 px
    /// window gave 260 px to the conversation and 300 px to an empty panel.
    /// Same four conditions as `view`, and they have to stay that way — a
    /// group that draws when this says empty is a rail with no room.
    pub fn is_empty(&self) -> bool {
        self.git.is_none()
            && self.meter_group_is_empty()
            && self.subagents.runs.is_empty()
            && (self.todos.is_empty() || self.todos_hidden)
    }

    pub fn view(&self, palette: &Palette) -> Element<'_, Message> {
        let mut groups = column![].spacing(18).width(Length::Fill);
        if let Some(git) = &self.git {
            groups = groups.push(self.git_group(git, palette));
        }
        if let Some(meter) = self.meter_group(palette) {
            groups = groups.push(meter);
        }
        if !self.subagents.runs.is_empty() {
            groups = groups.push(self.subagent_group(palette));
        }
        if !self.todos.is_empty() && !self.todos_hidden {
            groups = groups.push(self.todo_group(palette));
        }
        chrome::rail(
            chrome::scroll(groups).height(Length::Fill),
            WIDTH,
            true,
            palette,
        )
    }

    fn git_group<'a>(&'a self, git: &'a GitStatus, palette: &Palette) -> Element<'a, Message> {
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
        let summary = match git.files.len() {
            0 => "clean".to_string(),
            1 => "1 file".to_string(),
            count => format!("{count} files"),
        };
        rows.push(chrome::pick(
            chrome::spread(
                row![
                    chrome::body("Changes", palette),
                    chrome::muted(summary, palette)
                ]
                .spacing(8),
                diffstat(git.additions, git.deletions, palette),
            ),
            Message::ToggleFiles,
            self.files_open,
            palette,
        ));
        if self.files_open {
            for file in &git.files {
                rows.push(
                    container(chrome::pick(
                        // The path is the elastic half; the diffstat keeps its
                        // width. `spread` left the path `Shrink` at index 0,
                        // so it was laid out first against everything the rail
                        // had and the `+12 −3` got the remainder — which for a
                        // path of about thirty characters is nothing. That is
                        // the sidebar's `18d`-reads-as-`18c` bug in the panel
                        // next door, and a truncated diffstat is worse than a
                        // missing one: `+1` is a perfectly readable wrong
                        // answer.
                        //
                        // 26 rather than 34 because 34 monospace characters at
                        // `chrome::LITERAL` already exceed the ~245 px this
                        // row is given, before the diffstat is considered.
                        row![
                            container(
                                chrome::literal(elide_left(&file.path, 26), palette)
                                    .wrapping(iced::widget::text::Wrapping::None)
                            )
                            .width(Length::Fill)
                            .clip(true),
                            diffstat(file.additions, file.deletions, palette),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        Message::ShowDiff(file.path.clone()),
                        false,
                        palette,
                    ))
                    .padding(Padding::new(0.0).left(14.0))
                    .into(),
                );
            }
        }
        // Open is the lighter background and nothing else — the same fold the
        // changed-file list above uses, and for the reason the spec gives:
        // brightness, never a hue, and no glyph. A chevron would also be the
        // one character on this rail outside the two bundled font subsets.
        rows.push(chrome::pick(
            chrome::literal(format!("⎇ {}", git.branch), palette),
            Message::ToggleBranches,
            self.branches_open,
            palette,
        ));
        if self.branches_open {
            for branch in &self.branches {
                // The one that is checked out is not a checkout.
                if *branch == git.branch {
                    continue;
                }
                rows.push(
                    container(chrome::pick(
                        chrome::literal(elide_left(branch, 34), palette),
                        Message::Checkout(branch.clone()),
                        false,
                        palette,
                    ))
                    .padding(Padding::new(0.0).left(14.0))
                    .into(),
                );
            }
        }
        chrome::block("git tools", rows, palette)
    }

    /// The condition `meter_group` returns `None` on, without building the
    /// widget. Kept beside it so the two cannot answer differently.
    fn meter_group_is_empty(&self) -> bool {
        self.meter.context.is_none_or(|tokens| tokens == 0)
    }

    fn meter_group(&self, palette: &Palette) -> Option<Element<'_, Message>> {
        let tokens = self.meter.context?;
        if tokens == 0 {
            return None;
        }
        let percent = self.meter.percent();
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        if let Some(percent) = percent {
            // Amber at 85%: the point at which the next long turn is the one
            // that gets compacted out from under you.
            let color = match percent >= 85 {
                true => palette.color(Token::Warning),
                false => palette.color(Token::Muted),
            };
            let track = palette.raised;
            // Two portions rather than one filled child, because a single
            // `FillPortion` in a container fills it: the empty half has to be
            // a real widget for the full one to be a fraction of anything.
            let filled = percent.clamp(1, 100) as u16;
            rows.push(
                container(
                    row![
                        container(iced::widget::space().height(4))
                            .width(Length::FillPortion(filled))
                            .style(move |_theme| container::Style {
                                background: Some(iced::Background::Color(color)),
                                border: iced::Border::default().rounded(2.0),
                                ..container::Style::default()
                            }),
                        iced::widget::space().width(Length::FillPortion(100 - filled)),
                    ]
                    .height(4),
                )
                .width(Length::Fill)
                .style(move |_theme| container::Style {
                    background: Some(iced::Background::Color(track)),
                    border: iced::Border::default().rounded(2.0),
                    ..container::Style::default()
                })
                .into(),
            );
        }
        let line = match self.meter.window {
            Some(window) => format!(
                "{} of {} next turn",
                compact(tokens),
                compact(u64::from(window))
            ),
            None => format!("{} next turn", compact(tokens)),
        };
        rows.push(chrome::muted(line, palette).into());
        let spent = self.meter.prompt + self.meter.completion;
        if spent > 0 {
            rows.push(
                chrome::muted(format!("{} tokens this session", compact(spent)), palette).into(),
            );
        }
        Some(chrome::block(
            &match percent {
                Some(percent) => format!("context — {percent}%"),
                None => "context".to_string(),
            },
            rows,
            palette,
        ))
    }

    fn subagent_group(&self, palette: &Palette) -> Element<'_, Message> {
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        // Most recent first: the run you are waiting on is the one you just
        // started. The model stores them the other way up.
        for run in self.subagents.runs.iter().rev() {
            let (mark, dot) = match &run.status {
                subagent::Status::Running => ("●", Token::ToolRunning),
                subagent::Status::Done => ("✔", Token::ToolDone),
                subagent::Status::Budget => ("✔", Token::Warning),
                subagent::Status::Failed(_) => ("✗", Token::ToolFailed),
            };
            let finished = !matches!(run.status, subagent::Status::Running);
            let badge = match run.unread {
                0 => String::new(),
                count => format!("{count}"),
            };
            rows.push(chrome::pick(
                column![
                    // Same shape as the file rows above: the name is what a
                    // subagent was asked to do, so it is as long as the task
                    // description, and as the `Shrink` half of a `spread` it
                    // starved the unread badge to nothing. The badge is the
                    // only thing on this row that says a run has output you
                    // have not seen.
                    row![
                        container(
                            row![
                                text(mark).size(9.0).color(palette.color(dot)),
                                chrome::body(run.name.clone(), palette)
                                    .wrapping(iced::widget::text::Wrapping::None),
                            ]
                            .spacing(6)
                            .align_y(iced::Alignment::Center)
                        )
                        .width(Length::Fill)
                        .clip(true),
                        chrome::muted(badge, palette),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                    chrome::muted(run.activity(), palette),
                    {
                        let meta = chrome::literal(
                            format!(
                                "{} step{} · {} · {}",
                                run.steps,
                                if run.steps == 1 { "" } else { "s" },
                                duration(run.elapsed()),
                                run.status.label(),
                            ),
                            palette,
                        );
                        if finished {
                            row![
                                meta,
                                chrome::action("dismiss", Message::DismissRun(run.id), palette),
                            ]
                            .spacing(8)
                            .into()
                        } else {
                            meta.into()
                        }
                    },
                ]
                .spacing(2),
                Message::ShowRun(run.id),
                self.subagents.open == Some(run.id),
                palette,
            ));
        }
        chrome::block("subagents", rows, palette)
    }

    fn todo_group(&self, palette: &Palette) -> Element<'_, Message> {
        let rows = self
            .todos
            .iter()
            .map(|item| {
                let (glyph, token) = match (item.done, item.active) {
                    (true, _) => ("☒", Token::Faint),
                    (false, true) => ("▸", Token::Text),
                    (false, false) => ("☐", Token::Faint),
                };
                let color = match item.done {
                    true => palette.color(Token::Faint),
                    false => palette.color(Token::Text),
                };
                row![
                    text(glyph).size(chrome::UI).color(palette.color(token)),
                    text(item.text.clone()).size(chrome::UI).color(color),
                ]
                .spacing(7)
                .into()
            })
            .collect();
        chrome::block("todos", rows, palette)
    }
}

/// `+734` in green, `-7` in red — the only two coloured numbers in the rail.
fn diffstat<'a, M: 'a>(
    additions: u64,
    deletions: u64,
    palette: &Palette,
) -> iced::widget::Row<'a, M> {
    row![
        // The diff tokens, for the reason spelled out on `pane::diff_line`:
        // `minimal` sets `error = "white"`, so this row read `+30` in green
        // beside `−14` in white.
        text(format!("+{additions}"))
            .size(chrome::SMALL)
            .font(crate::native::font::MONO)
            .color(palette.color(Token::DiffAdd)),
        text(format!("−{deletions}"))
            .size(chrome::SMALL)
            .font(crate::native::font::MONO)
            .color(palette.color(Token::DiffDel)),
    ]
    .spacing(6)
}

/// `812`, `8.9K`, `89K`, `1.2M`.
///
/// Tabular in the font, so a count that ticks does not jog the rows beside it.
pub fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => scaled(n as f64 / 1_000.0, "K"),
        _ => scaled(n as f64 / 1_000_000.0, "M"),
    }
}

fn scaled(value: f64, suffix: &str) -> String {
    match value < 10.0 {
        true => format!("{value:.1}{suffix}").replace(".0", ""),
        false => format!("{}{suffix}", value.round() as u64),
    }
}

/// `42s`, `3m 1s`, `1h 12m`.
pub fn duration(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs().max(1);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let (minutes, seconds) = (seconds / 60, seconds % 60);
    if minutes < 60 {
        return match seconds {
            0 => format!("{minutes}m"),
            _ => format!("{minutes}m {seconds}s"),
        };
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    match minutes {
        0 => format!("{hours}h"),
        _ => format!("{hours}h {minutes}m"),
    }
}

/// Keep the tail of a path, which is the half that identifies the file.
///
/// The browser did this with an RTL trick and a leading LRM; a window has to do
/// it explicitly. `src/native/widget/transcript.rs` matters more at its end than
/// at its start.
pub fn elide_left(path: &str, width: usize) -> String {
    let count = path.chars().count();
    if count <= width {
        return path.to_string();
    }
    let tail: String = path.chars().skip(count - width + 1).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::from_theme(&crate::theme::minimal())
    }

    /// A rail with nothing in it says so, and one with anything in it does not.
    ///
    /// The window drops the rail outright when this is `true`, so a wrong
    /// answer is not cosmetic in either direction: `true` with a group to draw
    /// is a group nobody can see, and `false` on a fresh chat is the 300 px of
    /// empty panel this exists to stop. `is_empty` and `view` test the same
    /// four conditions and this is what keeps them saying the same thing.
    #[test]
    fn an_empty_rail_knows_it_is_empty_and_any_one_group_is_enough_to_fill_it() {
        assert!(
            Rail::default().is_empty(),
            "a fresh chat has no diff, no meter, no subagents and no todos"
        );

        // Each group on its own, because `is_empty` is a conjunction and a
        // dropped clause would still pass a test that sets all four.
        let git = Rail {
            git: Some(GitStatus {
                branch: "v2".to_string(),
                dirty: true,
                additions: 1,
                deletions: 0,
                files: Vec::new(),
            }),
            ..Rail::default()
        };
        assert!(!git.is_empty(), "a git status fills the rail");

        let mut meter = Rail::default();
        meter.meter.apply(&AgentEvent::Usage {
            prompt_tokens: 1_000,
            completion_tokens: 10,
        });
        assert!(!meter.is_empty(), "a context meter fills the rail");

        let mut todos = Rail {
            todos: vec![TodoRow {
                text: "something".to_string(),
                done: false,
                active: true,
            }],
            ..Rail::default()
        };
        assert!(!todos.is_empty(), "todos fill the rail");
        todos.todos_hidden = true;
        assert!(
            todos.is_empty(),
            "hidden todos draw nothing, so they do not count"
        );
    }

    /// The two counts are different numbers and the rail must not conflate
    /// them: spend only rises, context falls when history is compacted.
    #[test]
    fn spend_accumulates_while_context_tracks_the_next_call() {
        let mut meter = Meter::default();
        meter.apply(&AgentEvent::Usage {
            prompt_tokens: 1_000,
            completion_tokens: 200,
        });
        meter.apply(&AgentEvent::Usage {
            prompt_tokens: 3_000,
            completion_tokens: 400,
        });
        assert_eq!(meter.prompt, 4_000);
        assert_eq!(meter.completion, 600);
        assert_eq!(meter.context, Some(3_000));

        // Compaction shrank the history.
        meter.apply(&AgentEvent::ContextSize { tokens: 900 });
        assert_eq!(meter.context, Some(900));
        assert_eq!(meter.prompt, 4_000, "spend does not fall");
    }

    /// A provider that reports only completion tokens sends `prompt_tokens: 0`,
    /// which is not a context size. Taking it would blank the meter mid-turn.
    #[test]
    fn a_zero_prompt_count_does_not_move_the_context_reading() {
        let mut meter = Meter::default();
        meter.apply(&AgentEvent::ContextSize { tokens: 500 });
        meter.apply(&AgentEvent::Usage {
            prompt_tokens: 0,
            completion_tokens: 90,
        });
        assert_eq!(meter.context, Some(500));
        assert_eq!(meter.completion, 90);
    }

    /// No window, no percentage. A bar against a guessed denominator would be
    /// a number the user could act on that nothing stands behind.
    #[test]
    fn a_provider_with_no_window_gets_no_percentage() {
        let mut meter = Meter {
            context: Some(50_000),
            ..Meter::default()
        };
        assert_eq!(meter.percent(), None);
        meter.window = Some(100_000);
        assert_eq!(meter.percent(), Some(50));
        // And a reading past the window clamps rather than reading 140%.
        meter.context = Some(140_000);
        assert_eq!(meter.percent(), Some(100));
    }

    #[test]
    fn counts_and_durations_read_the_way_a_person_says_them() {
        assert_eq!(compact(812), "812");
        assert_eq!(compact(8_900), "8.9K");
        assert_eq!(compact(89_000), "89K");
        assert_eq!(compact(1_200_000), "1.2M");
        assert_eq!(duration(std::time::Duration::from_secs(0)), "1s");
        assert_eq!(duration(std::time::Duration::from_secs(42)), "42s");
        assert_eq!(duration(std::time::Duration::from_secs(181)), "3m 1s");
        assert_eq!(duration(std::time::Duration::from_secs(180)), "3m");
        assert_eq!(duration(std::time::Duration::from_secs(4_320)), "1h 12m");
    }

    /// A path is identified by its end. Eliding the tail would give three rows
    /// that all read `src/native/widget/…`.
    #[test]
    fn a_long_path_keeps_its_tail() {
        assert_eq!(elide_left("src/gui/git.rs", 30), "src/gui/git.rs");
        assert_eq!(
            elide_left("src/native/widget/transcript.rs", 14),
            "…transcript.rs"
        );
    }

    /// The rail shows nothing it has no reading for. An empty rail is correct
    /// on a fresh chat; a rail of zeroed groups is noise.
    #[test]
    fn an_untouched_rail_draws_no_groups() -> Result<(), iced_test::Error> {
        let rail = Rail::default();
        let mut ui = iced_test::simulator(rail.view(&palette()));
        for absent in ["Changes", "Progress"] {
            assert!(ui.find(absent).is_err(), "{absent}");
        }
        Ok(())
    }

    /// `/todos` hides the checklist and later updates do not reopen it — the
    /// flag is sticky on purpose, or a todo arriving would undo the toggle.
    #[test]
    fn hiding_the_todos_survives_a_later_update() -> Result<(), iced_test::Error> {
        let mut rail = Rail::default();
        rail.apply(&AgentEvent::TodoUpdated(vec![
            crate::tools::todo::TodoItem {
                content: "wire the meter".to_string(),
                status: crate::tools::todo::TodoStatus::InProgress,
            },
        ]));
        {
            let mut ui = iced_test::simulator(rail.view(&palette()));
            assert!(ui.find("wire the meter").is_ok());
        }
        rail.todos_hidden = true;
        rail.apply(&AgentEvent::TodoUpdated(vec![
            crate::tools::todo::TodoItem {
                content: "wire the meter".to_string(),
                status: crate::tools::todo::TodoStatus::Completed,
            },
        ]));
        let mut ui = iced_test::simulator(rail.view(&palette()));
        assert!(ui.find("wire the meter").is_err(), "still hidden");
        Ok(())
    }
}
