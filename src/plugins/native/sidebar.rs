//! The chat list: every session on this machine, grouped by workspace.
//!
//! # Nothing about *what* a chat is lives here
//!
//! The merge (sessions on disk + heartbeats + this process's own tasks), the
//! grouping and the `2m` are [`crate::session_registry`]'s, because they are
//! facts about the session store rather than about a window — and because the
//! TUI's `/resume` picker wants the same three. This module is the rendering
//! and the refresh cadence.
//!
//! # The refresh is on a timer, and it has to be
//!
//! The list is derived from files and from heartbeats that other processes
//! write. Nothing tells this window when another Wizard starts a chat, and the
//! ages tick on their own. So it is re-read on a timer, off the draw thread —
//! `read_dir` plus a `stat` per session is cheap but it is I/O, and doing it in
//! `view` would put a directory walk in every frame.
//!
//! # Claude Code's sessions are in the same list, and they are not the same
//!
//! [`crate::session_registry::claude_chats`] lists what Claude Code recorded
//! for a workspace, and those rows land in this list beside Wizard's own. Two
//! things follow, and both are visible in the widget rather than assumed:
//!
//! - **They look different, and clicking one does something different.** A
//!   Claude row carries a [`Message::OpenClaude`], not a [`Message::Select`],
//!   because opening it is an *import*: the conversation is walked back from
//!   its leaf and written as a new Wizard session. A row that looked like the
//!   others would be offering one gesture for two different acts.
//! - **The list is not read on the timer.** Listing them parses every
//!   transcript in the project — tens of megabytes for a repository that has
//!   been worked in for months — so the section is folded shut and reads when
//!   it is opened. What *does* ride the timer is
//!   [`crate::session_registry::claude_here`], a directory probe, so the
//!   section is hidden outright on the ordinary machine that has no `~/.claude`
//!   at all.

use std::path::PathBuf;

use iced::widget::{column, container, row, text};
use iced::{Element, Length, Padding};

use crate::plugins::native::theme::Palette;
use crate::plugins::native::widget::chrome;
use crate::session_registry::{ChatRow, Origin, SessionState, Workspace};
use crate::theme::Token;

/// The sidebar's width, per the design spec.
pub const WIDTH: f32 = 240.0;

/// Tallest the open workspace picker may get before it scrolls inside itself.
///
/// It lives in the sidebar's `Shrink` footer, directly under the chat list,
/// which is the column's only `Fill` — so without a bound every workspace row
/// it grows is taken out of the chat list's height. See [`Sidebar::footer`].
const MAX_PICKER_HEIGHT: f32 = 220.0;

/// How often the list is re-read. Slow, because it is a directory walk and
/// nothing on it changes fast; fast enough that a chat started in another
/// window appears while you are still looking for it.
pub const REFRESH: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum Message {
    /// A re-read landed.
    Loaded(Listing),
    Select(String),
    New,
    OpenSettings,
    /// Fold the list of directories a new chat could open in.
    ToggleWorkspaces,
    /// Open the next new chat in this directory (an absolute path).
    UseWorkspace(String),
    /// Fold the Claude Code section. Opening it starts the read that fills it.
    ToggleClaude,
    /// A Claude Code read landed, for the workspace it was asked about.
    ClaudeLoaded(String, Vec<ChatRow>),
    /// Import that Claude Code transcript and open the result. Not a
    /// [`Message::Select`], because it is not the same act: a file Wizard does
    /// not own becomes a session Wizard does.
    OpenClaude {
        /// The transcript to read, from the row's [`Origin::Claude`].
        source: PathBuf,
        /// The leaf of its DAG to walk the conversation back from.
        leaf: Option<String>,
    },
}

/// One refresh's worth of chat list.
///
/// The Claude Code probe rides along rather than being its own timer because it
/// answers the same question at the same moment — "what could this window open
/// right now" — and a second five-second task to ask one `is_dir` would be a
/// second thing to keep in step.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub workspaces: Vec<Workspace>,
    /// Whether Claude Code has recorded anything for the directory a new chat
    /// would open in. Cheap ([`crate::session_registry::claude_here`]); false
    /// on the ordinary machine, where it hides the section entirely.
    pub claude_here: bool,
}

/// The sidebar's state.
#[derive(Default)]
pub struct Sidebar {
    pub workspaces: Vec<Workspace>,
    /// The chat on screen, so its row is marked.
    pub selected: String,
    /// Whether the footer is showing the directories to choose between.
    picking_workspace: bool,
    /// Wall-clock seconds at the last refresh, so the ages are rendered
    /// against the moment the list was read rather than against a clock read
    /// per row in `view`.
    now: u64,
    /// Whether Claude Code has anything for the current workspace at all.
    claude_here: bool,
    /// Whether the Claude Code section is unfolded.
    claude_open: bool,
    /// A read is in flight, so the section says so instead of looking empty.
    claude_reading: bool,
    /// The workspace [`Sidebar::claude`] was read for. Held because the footer
    /// can move the window to another directory, and rows from the one before
    /// it would be another project's conversations under this project's name.
    claude_cwd: String,
    /// Claude Code's sessions for [`Sidebar::claude_cwd`], newest first.
    claude: Vec<ChatRow>,
}

impl Sidebar {
    /// Re-read every chat this machine knows about, and probe whether Claude
    /// Code has anything for `cwd`. Blocking: the caller runs it on a task,
    /// never in `view`.
    pub fn read(live: &std::collections::HashMap<String, SessionState>, cwd: &str) -> Listing {
        Listing {
            workspaces: crate::session_registry::group_by_workspace(
                crate::session_registry::chats(live),
            ),
            claude_here: crate::session_registry::claude_here(cwd),
        }
    }

    /// Read Claude Code's sessions for `cwd`. Blocking, and *slow* — it parses
    /// every transcript in the project — so this runs on a task and only when
    /// the section is opened. See the module header.
    pub fn read_claude(cwd: &str) -> Vec<ChatRow> {
        crate::session_registry::claude_chats(cwd)
    }

    pub fn loaded(&mut self, listing: Listing) {
        self.workspaces = listing.workspaces;
        self.claude_here = listing.claude_here;
        self.now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);
    }

    /// Whether Claude Code has anything recorded for the workspace this
    /// sidebar last read. False on most machines, and the reason the section
    /// is not drawn at all there.
    pub fn claude_here(&self) -> bool {
        self.claude_here
    }

    /// Open or close the Claude Code section. Returns the workspace whose
    /// sessions still need reading, or `None` when there is nothing to do —
    /// the section was closed, or its rows are already the right workspace's.
    pub fn toggle_claude(&mut self, cwd: &str) -> Option<String> {
        self.claude_open = !self.claude_open;
        self.claude_needs(cwd)
    }

    /// Open the Claude Code section, whether or not it already was. Returns
    /// the same "needs reading" answer [`Sidebar::toggle_claude`] does.
    ///
    /// What `/resume-claude` runs. A toggle would be wrong there: the command
    /// names an outcome rather than a flip, and typing it at an open section
    /// would shut the list it asked for.
    pub fn reveal_claude(&mut self, cwd: &str) -> Option<String> {
        self.claude_open = true;
        self.claude_needs(cwd)
    }

    /// The workspace whose sessions still need reading, or `None` — the
    /// section is shut, or its rows are already the right workspace's.
    fn claude_needs(&mut self, cwd: &str) -> Option<String> {
        if !self.claude_open || (self.claude_cwd == cwd && !self.claude.is_empty()) {
            return None;
        }
        self.claude_reading = true;
        Some(cwd.to_string())
    }

    /// A Claude Code read landed, for the workspace named in `cwd`.
    pub fn claude_loaded(&mut self, cwd: String, rows: Vec<ChatRow>) {
        self.claude_reading = false;
        self.claude_cwd = cwd;
        self.claude = rows;
    }

    /// Forget the Claude Code rows, because the window moved to another
    /// workspace. They are another project's conversations, and leaving them on
    /// screen under the new project's heading would be worse than a fold the
    /// user has to open again.
    pub fn forget_claude(&mut self) {
        self.claude_open = false;
        self.claude_reading = false;
        self.claude_cwd = String::new();
        self.claude.clear();
    }

    /// Open or close the footer's directory list. Returns nothing to run: the
    /// candidates are the workspaces already read for the chat list, so there
    /// is no I/O behind opening it.
    pub fn toggle_workspaces(&mut self) {
        self.picking_workspace = !self.picking_workspace;
    }

    /// Close the directory list, once a choice has been applied.
    pub fn close_workspaces(&mut self) {
        self.picking_workspace = false;
    }

    /// The workspace a chat runs in, for the top bar's repo chip.
    pub fn workspace_of(&self, id: &str) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|group| group.chats.iter().any(|chat| chat.id == id))
            .map(|group| group.name.as_str())
    }

    pub fn view(&self, cwd: &str, palette: &Palette) -> Element<'_, Message> {
        // The two actions sit together on the right, as a plain row.
        //
        // NOT a nested `spread`: `spread` puts a `Length::Fill` space between
        // its halves, and an inner row carrying one is greedy — it competes
        // with the outer spread's own Fill for the leftover width, wins a
        // share of it, and then spends that share pushing `settings` to the
        // far end of an allocation wider than the rail. The visible result was
        // a 100 px hole between `wizard` and the actions while `settings` ran
        // off the edge of a 240 px sidebar. Two Fills in one line is one too
        // many: the title goes left, the actions go right, and only the gap
        // between them is elastic.
        //
        // The row is kept even at one action, because the mesh button that
        // used to sit beside `settings` is coming back — see
        // [`crate::plugins::native::graph`].
        let head = chrome::spread(
            text("wizard")
                .size(chrome::UI)
                .font(crate::plugins::native::font::MONO)
                .color(palette.color(Token::Text)),
            row![chrome::action("settings", Message::OpenSettings, palette)]
                .spacing(4)
                .align_y(iced::Alignment::Center),
        );

        let new_chat = chrome::pick(
            chrome::spread(
                chrome::body("New Chat", palette),
                chrome::muted("Ctrl-N", palette),
            ),
            Message::New,
            false,
            palette,
        );

        let mut tree = column![].spacing(8).width(Length::Fill);
        for group in &self.workspaces {
            let mut rows = column![].spacing(1).width(Length::Fill);
            for chat in &group.chats {
                rows = rows.push(self.chat_row(chat, palette));
            }
            tree = tree.push(
                column![
                    container(chrome::literal(group.name.clone(), palette))
                        .padding(Padding::new(2.0).left(4.0)),
                    rows,
                ]
                .spacing(2),
            );
        }
        if let Some(section) = self.claude_section(cwd, palette) {
            tree = tree.push(section);
        }

        chrome::rail(
            column![
                head,
                new_chat,
                chrome::label("chats", palette),
                chrome::scroll(tree).height(Length::Fill),
                chrome::separator(palette),
                self.footer(cwd, palette),
            ]
            .spacing(8),
            WIDTH,
            false,
            palette,
        )
    }

    /// Where the *next* new chat opens, and the control that changes it.
    ///
    /// It names the directory [`Message::New`] will use, not the directory the
    /// chat on screen runs in: a session's own directory is fixed when it is
    /// created and is never retroactively moved, so a control that appeared to
    /// move the open chat would be claiming something the session store does
    /// not support.
    ///
    /// The candidates are the workspaces already on screen. That is a
    /// deliberate limit rather than a first step toward a file picker: a native
    /// directory chooser means linking GTK on Linux, which is the dependency
    /// the whole `native` feature flag exists to avoid, and `wizard --cwd
    /// <path> gui` already opens the window anywhere at all. What this covers
    /// is the case the browser GUI's top bar covered and this window did
    /// not — moving between the repositories you are already working in,
    /// without restarting the window.
    fn footer<'a>(&'a self, cwd: &str, palette: &Palette) -> Element<'a, Message> {
        let here = crate::session_registry::workspace_name(cwd);
        // Open is the lighter background and nothing else, the same fold the
        // rail's lists use. See the note in `rail::Rail::git_group`.
        let chip = chrome::pick(
            chrome::literal(here, palette),
            Message::ToggleWorkspaces,
            self.picking_workspace,
            palette,
        );
        if !self.picking_workspace {
            return chip;
        }
        let mut rows = column![].spacing(1).width(Length::Fill);
        for group in &self.workspaces {
            // The one it already opens in is not a choice, and offering it
            // would be the only row in the list that does nothing.
            if group.path == cwd {
                continue;
            }
            rows = rows.push(
                container(chrome::pick(
                    chrome::literal(group.name.clone(), palette),
                    Message::UseWorkspace(group.path.clone()),
                    false,
                    palette,
                ))
                .padding(Padding::new(0.0).left(10.0)),
            );
        }
        // Bounded, and scrolled past that.
        //
        // The picker was a plain unbounded `Column` in the sidebar's `Shrink`
        // footer, directly under the chat list — the column's only `Fill` — so
        // every workspace row it grew came straight out of the chat list. On a
        // 220 px window five workspaces left the list at *zero* height, with no
        // chat reachable at all, and drew the fifth workspace past the bottom
        // of the window where it could not be clicked. Not only a small-window
        // problem: the chrome above is about 158 px and each row is 27, so
        // roughly twenty workspaces does the same to an ordinary 700 px
        // sidebar.
        //
        // The chip stays outside the scroll so the control that closes the
        // picker cannot itself be scrolled out of reach.
        container(
            column![
                chip,
                chrome::scroll(rows)
                    .height(Length::Shrink)
                    .width(Length::Fill),
            ]
            .spacing(1)
            .width(Length::Fill),
        )
        .max_height(MAX_PICKER_HEIGHT)
        .width(Length::Fill)
        .into()
    }

    /// Claude Code's sessions for this workspace, folded shut.
    ///
    /// `None` when Claude Code has nothing here, which is what most machines
    /// look like: a heading over an empty list would be a permanent
    /// advertisement for a program the user may not have installed.
    fn claude_section<'a>(&'a self, cwd: &str, palette: &Palette) -> Option<Element<'a, Message>> {
        if !self.claude_here {
            return None;
        }
        // The heading says what the section is *and*, once open, what opening a
        // row does — the fold is the only place there is room to say it, and it
        // is the thing a user most needs to know before the first click.
        let heading = chrome::pick(
            chrome::spread(
                chrome::literal(CLAUDE_SECTION, palette),
                chrome::muted(
                    match self.claude_open {
                        true => "▾",
                        false => "▸",
                    },
                    palette,
                ),
            ),
            Message::ToggleClaude,
            self.claude_open,
            palette,
        );
        let mut section = column![heading].spacing(2).width(Length::Fill);
        if !self.claude_open {
            return Some(section.into());
        }

        section = section.push(
            container(chrome::muted(
                match self.claude_reading {
                    true => format!("reading {}…", crate::session_registry::workspace_name(cwd)),
                    false => CLAUDE_HINT.to_string(),
                },
                palette,
            ))
            .padding(Padding::new(2.0).left(8.0)),
        );
        if self.claude_reading {
            return Some(section.into());
        }
        if self.claude.is_empty() {
            section = section.push(
                container(chrome::muted("no sessions here", palette))
                    .padding(Padding::new(2.0).left(8.0)),
            );
        }
        for chat in &self.claude {
            section = section.push(self.chat_row(chat, palette));
        }
        Some(section.into())
    }

    fn chat_row<'a>(&'a self, chat: &'a ChatRow, palette: &Palette) -> Element<'a, Message> {
        let selected = chat.id == self.selected;
        let (glyph, token) = mark(&chat.origin, chat.state, selected);
        // A Claude Code row says so where a Wizard row says how long ago it
        // moved. Two signals, neither of them colour alone: the glyph is a
        // different *shape*, and the tag is a word.
        // The budgets are in *characters* against a proportional font, so they
        // are a bound on the string and not on its width: 24 narrow ones fit
        // the column and 24 wide ones do not. They are set low enough that the
        // wide case still fits rather than being set to the average and
        // letting the wide case collide with the age beside it — which is what
        // 26 did, wrapping long rows onto a second line and pushing `18d` under
        // the scrollbar to render as `18c`.
        let (right, width) = match &chat.origin {
            Origin::Wizard => (
                crate::session_registry::relative_age(chat.updated_unix, self.now),
                18,
            ),
            Origin::Claude { .. } => (CLAUDE_TAG.to_string(), 13),
        };
        chrome::pick(
            row![
                container(
                    row![
                        text(glyph).size(8.0).color(palette.color(token)),
                        text(one_line(&chat.title, width))
                            .size(chrome::UI)
                            // One line, clipped by the container rather than
                            // reflowed into two. Reserving the age column properly
                            // takes 44 px out of this row, and a budget tuned when
                            // the title could spill into that space now wraps.
                            .wrapping(iced::widget::text::Wrapping::None)
                            .color(palette.color(match selected {
                                true => Token::Text,
                                false => Token::Muted,
                            })),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center)
                )
                .width(Length::Fill)
                .clip(true),
                // The age gets a column of its own rather than whatever is
                // left over. Sharing a row with a title that can be one
                // character too wide means the age is what gets compressed,
                // and an age is short enough that losing its last glyph turns
                // it into a different, entirely readable, wrong answer.
                //
                // The title half has to be the `Fill` one for that to be true.
                // Under `spread` it was `Shrink` at index 0, so it was laid
                // out first and this `Fixed(44)` was clamped to the remainder
                // — measured at 7 px for a wide title. It read as reserved
                // only because the character budget above had been tuned
                // against a screenshot, which is a coincidence rather than a
                // guarantee, and one that breaks silently the next time
                // either the budget or the font moves.
                container(chrome::muted(right, palette))
                    .width(Length::Fixed(AGE_COLUMN))
                    .align_x(iced::alignment::Horizontal::Right),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            open(chat),
            selected,
            palette,
        )
    }
}

/// Width of the right-hand column on a chat row, holding the age or the
/// `claude` tag. Wide enough for the longest thing either can be — `claude`
/// itself — so neither is ever the thing that gets squeezed.
const AGE_COLUMN: f32 = 44.0;

/// The Claude Code section's heading.
const CLAUDE_SECTION: &str = "claude code";

/// The word on a Claude Code row, where a Wizard row carries its age.
const CLAUDE_TAG: &str = "claude";

/// What opening one of these rows does, said before the first click rather
/// than after it.
const CLAUDE_HINT: &str = "opens as a copy · file untouched";

/// What clicking `chat` asks for.
///
/// The two are deliberately different messages rather than one carrying a flag:
/// resuming a Wizard session and importing a Claude Code transcript are
/// different acts on different stores, and a single `Select(id)` would have made
/// them the same call with a branch somewhere further in.
fn open(chat: &ChatRow) -> Message {
    match &chat.origin {
        Origin::Wizard => Message::Select(chat.id.clone()),
        Origin::Claude { path, leaf, .. } => Message::OpenClaude {
            source: path.clone(),
            leaf: leaf.clone(),
        },
    }
}

/// The gutter mark beside a chat.
///
/// A Claude Code row takes a hollow diamond and keeps it in every state, which
/// is the point: the gutter is the one column a reader scans, and a foreign
/// session must be findable there without reading the tag. A *shape*, not a
/// colour — the three reported states are already told apart by hue, and a
/// fourth hue would be both crowded and invisible to a colourblind reader.
fn mark(origin: &Origin, state: Option<SessionState>, selected: bool) -> (&'static str, Token) {
    if origin.is_foreign() {
        return (
            "◇",
            match selected {
                true => Token::Text,
                false => Token::Muted,
            },
        );
    }
    dot(state, selected)
}

/// The gutter mark beside a Wizard chat.
///
/// A dot for a state worth reporting, and for the selected row so it has a
/// gutter mark; a blank otherwise. A column of grey dots beside every idle chat
/// is noise that makes the amber one harder to find, which is the only reason
/// the gutter exists.
fn dot(state: Option<SessionState>, selected: bool) -> (&'static str, Token) {
    match state {
        Some(SessionState::Working) => ("●", Token::ToolRunning),
        Some(SessionState::NeedsInput) => ("●", Token::Warning),
        Some(SessionState::Failed) => ("●", Token::ToolFailed),
        _ if selected => ("●", Token::Muted),
        _ => (" ", Token::Faint),
    }
}

/// A chat's title as one truncated line.
///
/// The first line, because a prompt is often a paragraph and the row is one
/// line high; truncated at the end, because unlike a path a prompt is
/// identified by how it *starts*.
fn one_line(title: &str, width: usize) -> String {
    let line = title.lines().next().unwrap_or("").trim();
    if line.chars().count() <= width {
        return line.to_string();
    }
    let head: String = line.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::from_theme(&crate::theme::minimal())
    }

    fn chat(id: &str, title: &str, state: Option<SessionState>) -> ChatRow {
        ChatRow {
            id: id.to_string(),
            title: title.to_string(),
            cwd: "/src/wizard".to_string(),
            updated_unix: 0,
            state,
            origin: Origin::Wizard,
        }
    }

    /// A Claude Code row as [`crate::session_registry::claude_chats_in`] would
    /// have produced it.
    fn claude(id: &str, title: &str) -> ChatRow {
        ChatRow {
            origin: Origin::Claude {
                path: PathBuf::from(format!("/home/u/.claude/projects/x/{id}.jsonl")),
                leaf: Some(format!("{id}-leaf")),
                branch_points: 1,
            },
            ..chat(id, title, None)
        }
    }

    fn sidebar(chats: Vec<ChatRow>) -> Sidebar {
        let mut sidebar = Sidebar::default();
        sidebar.loaded(Listing {
            workspaces: crate::session_registry::group_by_workspace(chats),
            claude_here: false,
        });
        sidebar
    }

    /// A sidebar with the Claude Code section present, open, and filled.
    fn with_claude(chats: Vec<ChatRow>, claude: Vec<ChatRow>) -> Sidebar {
        let mut sidebar = Sidebar::default();
        sidebar.loaded(Listing {
            workspaces: crate::session_registry::group_by_workspace(chats),
            claude_here: true,
        });
        assert_eq!(
            sidebar.toggle_claude("/src/wizard"),
            Some("/src/wizard".to_string()),
            "opening an unread section asks for a read"
        );
        sidebar.claude_loaded("/src/wizard".to_string(), claude);
        sidebar
    }

    /// A prompt is identified by its start, so the truncation takes the head —
    /// the opposite of a path, which is identified by its end.
    #[test]
    fn a_title_is_one_line_truncated_at_the_end() {
        assert_eq!(one_line("short", 26), "short");
        assert_eq!(one_line("first line\nsecond", 26), "first line");
        assert_eq!(
            one_line("create an intelligent go opponent", 20),
            "create an intellige…"
        );
    }

    /// The chat tree is grouped and the workspace is named. Without the group
    /// head, four repositories' chats are one undifferentiated list.
    #[test]
    fn chats_are_grouped_under_their_workspace() -> Result<(), iced_test::Error> {
        let sidebar = sidebar(vec![
            chat("a", "fix the lock", None),
            chat("b", "add a test", None),
        ]);
        let mut ui = iced_test::simulator(sidebar.view("/src/wizard", &palette()));
        assert!(ui.find("wizard").is_ok(), "the group head names the repo");
        assert!(ui.find("fix the lock").is_ok());
        assert!(ui.find("add a test").is_ok());
        Ok(())
    }

    /// Clicking a row asks for that chat by id. Everything about switching
    /// hangs off this one message.
    #[test]
    fn clicking_a_chat_selects_it_by_id() -> Result<(), iced_test::Error> {
        let sidebar = sidebar(vec![chat("abc", "fix the lock", None)]);
        let mut ui = iced_test::simulator(sidebar.view("/src/wizard", &palette()));
        ui.click("fix the lock")?;
        assert!(matches!(
            ui.into_messages().next(),
            Some(Message::Select(id)) if id == "abc"
        ));
        Ok(())
    }

    /// A chat that needs input has to be findable across four repositories
    /// without reading every row, which is what the dot is for — and why an
    /// idle chat does not get one.
    #[test]
    fn only_states_worth_reporting_get_a_dot() {
        assert_eq!(dot(Some(SessionState::Working), false).0, "●");
        assert_eq!(dot(Some(SessionState::NeedsInput), false).1, Token::Warning);
        assert_eq!(dot(Some(SessionState::Failed), false).1, Token::ToolFailed);
        assert_eq!(dot(Some(SessionState::Idle), false).0, " ", "idle is quiet");
        assert_eq!(dot(None, false).0, " ", "and so is dormant");
        // Except when it is the row you are in, which needs a gutter mark of
        // its own or the selected row shifts left by a dot's width.
        assert_eq!(dot(None, true).0, "●");
        assert_eq!(dot(None, true).1, Token::Muted);
        // And the three reported states stay distinguishable from each other.
        let reported = [
            dot(Some(SessionState::Working), false).1,
            dot(Some(SessionState::NeedsInput), false).1,
            dot(Some(SessionState::Failed), false).1,
        ];
        assert_eq!(
            reported
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3,
            "working, waiting and failed must not look the same"
        );
    }

    /* ------------------------------------------------------------------ */
    /* Claude Code's sessions                                             */
    /* ------------------------------------------------------------------ */

    /// The machine that has never run Claude Code — which is most of them —
    /// gets no heading, no fold, and nothing to click.
    #[test]
    fn nothing_about_claude_code_appears_when_there_is_none() -> Result<(), iced_test::Error> {
        let sidebar = sidebar(vec![chat("a", "fix the lock", None)]);
        let mut ui = iced_test::simulator(sidebar.view("/src/wizard", &palette()));
        assert!(ui.find("fix the lock").is_ok());
        assert!(
            ui.find(CLAUDE_SECTION).is_err(),
            "an empty section is an advertisement for another program"
        );
        Ok(())
    }

    /// The section is shut until it is asked for, because opening it parses
    /// every transcript in the project. Closing it again asks for nothing, and
    /// re-opening it does not re-read what is already there.
    #[test]
    fn the_claude_section_reads_only_when_it_is_opened() -> Result<(), iced_test::Error> {
        let mut sidebar = sidebar(vec![chat("a", "fix the lock", None)]);
        sidebar.loaded(Listing {
            workspaces: crate::session_registry::group_by_workspace(vec![chat(
                "a",
                "fix the lock",
                None,
            )]),
            claude_here: true,
        });

        // Shut: the heading is there, the rows are not.
        let mut ui = iced_test::simulator(sidebar.view("/src/wizard", &palette()));
        assert!(ui.find(CLAUDE_SECTION).is_ok());
        assert!(ui.find(CLAUDE_HINT).is_err());
        ui.click(CLAUDE_SECTION)?;
        assert!(matches!(
            ui.into_messages().next(),
            Some(Message::ToggleClaude)
        ));

        assert_eq!(
            sidebar.toggle_claude("/src/wizard"),
            Some("/src/wizard".to_string()),
            "the first open asks for the read"
        );
        sidebar.claude_loaded(
            "/src/wizard".to_string(),
            vec![claude("c1", "port the CLI")],
        );
        assert_eq!(
            sidebar.toggle_claude("/src/wizard"),
            None,
            "closing reads nothing"
        );
        assert_eq!(
            sidebar.toggle_claude("/src/wizard"),
            None,
            "and re-opening does not re-parse what is already held"
        );

        // A move to another workspace drops them: they are another project's.
        sidebar.forget_claude();
        assert_eq!(
            sidebar.toggle_claude("/src/other"),
            Some("/src/other".to_string())
        );
        Ok(())
    }

    /// Provenance, rendered. A Claude Code row must not be mistakable for a
    /// Wizard one, and the section has to say what opening it does *before* the
    /// click rather than after it.
    #[test]
    fn a_claude_row_is_marked_as_one_and_says_what_opening_it_does() -> Result<(), iced_test::Error>
    {
        let sidebar = with_claude(
            vec![chat("a", "fix the lock", None)],
            vec![claude("c1", "port the CLI")],
        );
        let mut ui = iced_test::simulator(sidebar.view("/src/wizard", &palette()));

        assert!(ui.find("fix the lock").is_ok(), "the Wizard chat is there");
        assert!(ui.find("port the CLI").is_ok(), "and the Claude one");
        assert!(ui.find(CLAUDE_SECTION).is_ok(), "under its own heading");
        assert!(
            ui.find(CLAUDE_TAG).is_ok(),
            "and the row itself carries the word"
        );
        assert!(
            ui.find(CLAUDE_HINT).is_ok(),
            "opening one is a copy, and the fold says so before the click"
        );
        Ok(())
    }

    /// The two rows are different acts, so they are different messages. A
    /// `Select` here would have opened a session id that does not exist in
    /// Wizard's store.
    #[test]
    fn clicking_a_claude_row_asks_for_an_import_at_its_leaf() -> Result<(), iced_test::Error> {
        let sidebar = with_claude(
            vec![chat("a", "fix the lock", None)],
            vec![claude("c1", "port the CLI")],
        );
        let mut ui = iced_test::simulator(sidebar.view("/src/wizard", &palette()));
        ui.click("port the CLI")?;
        let message = ui.into_messages().next().expect("a message");
        let Message::OpenClaude { source, leaf } = message else {
            panic!("a Claude row is not a Select: {message:?}");
        };
        assert_eq!(
            source,
            PathBuf::from("/home/u/.claude/projects/x/c1.jsonl"),
            "the file to read comes off the row"
        );
        assert_eq!(
            leaf.as_deref(),
            Some("c1-leaf"),
            "and so does the leaf, or the import would resolve a branch of its own"
        );

        // The Wizard row beside it still resumes in place.
        let mut ui = iced_test::simulator(sidebar.view("/src/wizard", &palette()));
        ui.click("fix the lock")?;
        assert!(matches!(
            ui.into_messages().next(),
            Some(Message::Select(id)) if id == "a"
        ));
        Ok(())
    }

    /// The gutter is the column a reader scans, so the foreign rows are told
    /// apart there by shape — not by a fourth hue beside three that already
    /// mean something.
    #[test]
    fn a_foreign_row_keeps_its_own_gutter_shape_in_every_state() {
        let claude = Origin::Claude {
            path: PathBuf::new(),
            leaf: None,
            branch_points: 0,
        };
        for state in [
            None,
            Some(SessionState::Working),
            Some(SessionState::NeedsInput),
            Some(SessionState::Failed),
            Some(SessionState::Idle),
        ] {
            assert_eq!(mark(&claude, state, false).0, "◇", "{state:?}");
            assert_ne!(
                mark(&claude, state, false).0,
                mark(&Origin::Wizard, state, false).0,
                "a Claude row and a Wizard row must not share a glyph: {state:?}"
            );
        }
        // Selected changes the ink, never the shape.
        assert_eq!(mark(&claude, None, true).0, "◇");
        assert_eq!(mark(&claude, None, true).1, Token::Text);
        // And a Wizard row is exactly what it was.
        assert_eq!(mark(&Origin::Wizard, None, true), dot(None, true));
    }
}
