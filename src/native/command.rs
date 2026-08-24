//! The `/` palette, and how a command typed in this window gets run.
//!
//! # There is no second dispatcher, and this file is where that is decided
//!
//! `src/commands/` owns the table, the parser, and [`dispatch`] — every match
//! arm and every line of prose a built-in answers with. A surface supplies
//! *verbs* through [`CommandSurface`] and gets the semantics back. The test
//! `no_surface_hand_rolls_a_handler_that_shadows_the_registry` scans the whole
//! of `src/` for a `match` over [`SlashCommand`] outside `src/commands/` and
//! fails if it finds one — including here, since it is a text scan and does not
//! care that this module is behind a feature.
//!
//! So this window runs a command in exactly two ways, chosen by the table's own
//! column for [`Surface::Gui`]:
//!
//! - [`Execution::Agent`] — the command changes the *conversation*
//!   (`/model`, `/compact`, `/rewind`, `/status`, …). It needs `&mut Agent`,
//!   which lives on the task's worker and not here, so it goes through
//!   [`TaskManager::submit_command`], and the worker dispatches it against
//!   `GuiSurface`. The window writes no handler at all: the answer comes back
//!   as the same `AgentEvent`s everything else comes back as.
//! - [`Execution::Ui`] — the command changes the *window* (`/diff`, `/todos`,
//!   `/settings`, `/clear`, `/resume`, `/provider`, `/login`, `/dashboard`).
//!   There is nothing to ask the agent for. These go through the same
//!   [`dispatch`], against [`Native`] below, whose verbs return
//!   [`Action`]s for the app to apply.
//!
//! Why the window's half still goes through `dispatch` rather than being a
//! `match` here: because that `match` is the regression. `/provider` typed at a
//! surface with no picker has to say the same sentence everywhere, `/help` has
//! to stay derived from the table, and a command added to the table has to
//! reach this surface or fail to compile. Routing by the *column* and then
//! dispatching is what buys all three.
//!
//! # Why the verbs return actions instead of touching `App`
//!
//! [`dispatch`] is `async` and iced's `update` is not. A `CommandSurface` that
//! held `&mut App` could not be awaited from inside `update`, and one that held
//! an `Arc<Mutex<App>>` would be a lock around the whole window taken on a
//! worker thread. So [`Native`] collects [`Action`]s, the dispatch runs on an
//! [`iced::Task`], and the app applies the list when it lands — which is also
//! what makes the whole thing testable without a window.

use std::path::PathBuf;

use async_trait::async_trait;
use iced::widget::{column, container, row, text};
use iced::{Border, Element, Length, Padding};

use crate::agent::RewindCandidate;
use crate::commands::surface::{
    Chooser, CommandSurface, Panel, PlanState, SessionSnapshot, Surface, dispatch,
};
use crate::commands::{CommandSpec, CustomCommand, Execution, SlashCommand};
use crate::config::{Mode, ReasoningEffort};
use crate::native::theme::Palette;
use crate::native::widget::chrome;
use crate::theme::Token;
use crate::tools::tasks::Task;

/// What running a command asks the window to do.
///
/// One variant per window-owning verb, and no variant that means "print this" —
/// notices go through [`Action::Notice`] because [`dispatch`] is what writes
/// the words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Notice(String),
    Error(String),
    /// `/settings`, `/provider`, `/login`: open the sheet. `picker` deep-links
    /// straight to the add-provider list.
    OpenSettings {
        picker: bool,
    },
    /// `/diff [path]`.
    ShowDiff(Option<String>),
    TogglePanel(Panel),
    /// `/clear`: a fresh chat in the same workspace. Not a truncation of this
    /// one — the browser GUI that this replaced made the same call, and it is
    /// the right one: the conversation you cleared is still on disk and still
    /// in the sidebar.
    NewChat,
    /// `/resume [id]`: reveal the chat list, or open a chat by id.
    Resume(Option<String>),
    /// `/resume-claude`: unfold the sidebar's Claude Code section, which is
    /// shut by default and is where those conversations already live. The
    /// window has no modal picker to open — the list is the sidebar.
    RevealClaude,
}

/// The window as a [`CommandSurface`].
///
/// Read-only about the session (the snapshot and the plan state are handed in
/// by the app, which has them) and write-only about the window (every change is
/// an [`Action`]). It cannot reach the agent, and that is the point: an
/// [`Execution::Agent`] command never arrives here.
pub struct Native {
    project_root: PathBuf,
    snapshot: SessionSnapshot,
    plan: PlanState,
    /// What the dispatch asked the window to do, in order.
    pub actions: Vec<Action>,
}

impl Native {
    pub fn new(project_root: PathBuf, snapshot: SessionSnapshot, plan: PlanState) -> Self {
        Self {
            project_root,
            snapshot,
            plan,
            actions: Vec::new(),
        }
    }
}

#[async_trait]
impl CommandSurface for Native {
    fn surface(&self) -> Surface {
        // The GUI column, and deliberately not a second one of its own.
        //
        // A `Surface::Native` variant would need a column on every row of the
        // table, and every answer in it would be a copy of the `gui` column —
        // both halves run the same agent, offer the same commands and refuse
        // the same three (`/vim`, `/ui`, `/quit`). A column that is a
        // duplicate of another column is a column that will drift from it. The
        // column was the browser GUI's; when that surface was deleted this one
        // inherited it, which was a rename rather than a migration.
        Surface::Gui
    }

    fn project_root(&self) -> PathBuf {
        self.project_root.clone()
    }

    /// The keys `/help` lists. Every one of them is bound — see
    /// `native::tests::every_key_the_window_advertises_is_bound`, which exists
    /// because a button here once advertised Ctrl-D while nothing in the tree
    /// bound it.
    fn help_keys(&self) -> Option<&'static str> {
        Some(
            // "once you have selected something" is not padding: Ctrl-A only
            // reaches the conversation when a selection is already live in it,
            // deliberately, because the composer is the other thing on screen
            // that answers to Ctrl-A and stealing it would make the composer's
            // own select-all unreachable. Without the qualifier this line
            // promises a keystroke that, pressed first, selects your draft.
            "keys:\n  Ctrl-N — new chat\n  Ctrl-C — copy the selection\n  \
             Ctrl-A — select the whole conversation, once you have selected \
             something in it\n  Esc — close the pane, or clear the selection",
        )
    }

    fn notice(&mut self, text: String) {
        self.actions.push(Action::Notice(text));
    }

    fn error(&mut self, message: String) {
        self.actions.push(Action::Error(message));
    }

    fn snapshot(&self) -> SessionSnapshot {
        self.snapshot.clone()
    }

    fn plan(&self) -> PlanState {
        self.plan
    }

    fn background_tasks(&self) -> Result<Vec<Task>, String> {
        // The agent holds them and it is on the worker. `/bashes` is an
        // `Execution::Agent` command on this surface, so it never routes here;
        // this arm exists because the trait requires it and it answers
        // honestly rather than guessing.
        Err("the agent is on the task's worker — /bashes runs there".to_string())
    }

    fn rewind_candidates(&self) -> Vec<RewindCandidate> {
        Vec::new()
    }

    // --- the agent's verbs -------------------------------------------------
    //
    // Unreachable *by routing*: everything below is `Execution::Agent` on
    // `Surface::Gui`, so `run` sends it to the worker instead of dispatching it
    // here. They are still implemented rather than left to panic, because a
    // trait method that cannot be called is a claim about today's table, and
    // the honest answer costs one line each.

    async fn set_model(&mut self, tag: String) {
        self.elsewhere(&format!("model {tag}"));
    }

    async fn set_mode(&mut self, mode: Mode) -> bool {
        self.elsewhere(&format!("mode {mode}"));
        false
    }

    async fn set_effort(&mut self, _effort: Option<ReasoningEffort>) -> bool {
        self.elsewhere("effort");
        false
    }

    async fn set_plan(&mut self, _plan: PlanState) -> bool {
        self.elsewhere("plan");
        false
    }

    async fn compact(&mut self) {
        self.elsewhere("compact");
    }

    async fn reload(&mut self) {
        self.elsewhere("reload");
    }

    async fn rewind(&mut self, turn: u64) {
        self.elsewhere(&format!("rewind {turn}"));
    }

    async fn btw(&mut self, _question: String) {
        self.elsewhere("btw");
    }

    async fn fork(&mut self, _task: String) {
        self.elsewhere("fork");
    }

    async fn start_goal(&mut self, _goal: String) -> bool {
        false
    }

    async fn evolve(&mut self, _deep: bool, _description: String) {
        self.elsewhere("evolve");
    }

    async fn publish(&mut self, _branch: Option<String>) {
        self.elsewhere("publish");
    }

    async fn toggle_fusion(&mut self) {
        self.elsewhere("fusion");
    }

    async fn toggle_ultra(&mut self) {
        self.elsewhere("ultra");
    }

    async fn server(&mut self, _action: crate::commands::ServerAction) {
        self.elsewhere("server");
    }

    // --- the window's own verbs -------------------------------------------

    async fn open(&mut self, chooser: Chooser) -> bool {
        match chooser {
            // `/settings` and `/provider` are the same sheet, one step apart.
            Chooser::Settings => {
                self.actions.push(Action::OpenSettings { picker: false });
                true
            }
            Chooser::Provider => {
                self.actions.push(Action::OpenSettings { picker: true });
                true
            }
            Chooser::Resume => {
                self.actions.push(Action::Resume(None));
                true
            }
            Chooser::ResumeClaude => {
                self.actions.push(Action::RevealClaude);
                true
            }
            // No picker for these; `dispatch` says what to type instead, which
            // is the answer the terminal's own menus were built to avoid and is
            // still better than a menu that does nothing.
            Chooser::Model
            | Chooser::Mode
            | Chooser::Effort
            | Chooser::Rewind
            | Chooser::Agents
            | Chooser::FusionPanel
            | Chooser::UltraRoster => false,
        }
    }

    async fn clear(&mut self) {
        self.actions.push(Action::NewChat);
    }

    async fn resume(&mut self, id: String) {
        self.actions.push(Action::Resume(Some(id)));
    }

    async fn toggle_panel(&mut self, panel: Panel) {
        match panel {
            Panel::Diff => self.actions.push(Action::ShowDiff(None)),
            other => self.actions.push(Action::TogglePanel(other)),
        }
    }

    async fn login(&mut self, _provider: String, _force: bool) {
        self.actions.push(Action::OpenSettings { picker: true });
    }
}

impl Native {
    /// The answer for a verb that belongs to the task's worker. Routing means
    /// it is never reached; if the table ever moves a command's column, this is
    /// what the user sees instead of silence.
    fn elsewhere(&mut self, name: &str) {
        self.actions.push(Action::Error(format!(
            "'/{name}' runs against the chat's agent, not the window"
        )));
    }
}

/// Where one typed command goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Not a command at all: send it as a message.
    Message(String),
    /// A built-in the task's worker runs. Name and arguments, as
    /// [`crate::gui::tasks::CommandRequest`] takes them.
    Agent { name: String, args: String },
    /// A built-in the window runs, through [`dispatch`] against [`Native`].
    Window(SlashCommand),
    /// The line did not parse. The parser's own message.
    Refused(String),
}

/// Decide where `line` goes, without running anything.
///
/// A custom `.wizard/commands/*.md` command routes as a **message**, and the
/// `custom` list is why this function needs one: [`SlashCommand::parse`] does
/// not know about them and answers `unknown command '/deploy'` for every one,
/// so a route that only consulted the parser would refuse every workspace
/// command in the palette it had just offered. The expansion itself happens
/// where it happens on every other surface —
/// [`crate::commands::preprocess`], on the way into the turn — so the right
/// answer here is to get out of the way.
pub fn route(line: &str, custom: &[CustomCommand]) -> Route {
    if let Some(name) = line.trim().strip_prefix('/') {
        let name = name.split_whitespace().next().unwrap_or("");
        if custom.iter().any(|command| command.name == name) {
            return Route::Message(line.to_string());
        }
    }
    let Some(parsed) = SlashCommand::parse(line) else {
        return Route::Message(line.to_string());
    };
    let command = match parsed {
        Ok(command) => command,
        Err(why) => return Route::Refused(why),
    };
    match command.spec().execution(Surface::Gui) {
        Execution::Agent => {
            let body = line.trim().trim_start_matches('/');
            let (name, args) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
            Route::Agent {
                name: name.to_string(),
                args: args.trim().to_string(),
            }
        }
        // Unavailable routes to the window too, so `dispatch`'s own refusal —
        // which explains what the command *is* — is what the user reads.
        Execution::Ui | Execution::Unavailable => Route::Window(command),
    }
}

/// Run a window command and collect what it asked for.
pub async fn run(
    command: SlashCommand,
    project_root: PathBuf,
    snapshot: SessionSnapshot,
    plan: PlanState,
) -> Vec<Action> {
    let mut surface = Native::new(project_root, snapshot, plan);
    dispatch(command, &mut surface).await;
    surface.actions
}

/* ---------------------------------------------------------------------- */
/* The palette                                                            */
/* ---------------------------------------------------------------------- */

/// One row of the palette.
pub struct Entry {
    pub name: String,
    pub args: &'static str,
    pub detail: String,
    /// A workspace's own `.wizard/commands/*.md`.
    pub custom: bool,
    /// Listed, dimmed, and unpickable: the surface does not run it.
    pub unavailable: bool,
}

/// The `/` menu over the composer.
///
/// Not a global Ctrl-K overlay: it is autocomplete on a leading `/`, exactly
/// where the command is being typed. A separate overlay would be a second place
/// to find commands beside the one you are already in.
#[derive(Default)]
pub struct Menu {
    /// The workspace's custom commands, reloaded per chat.
    pub custom: Vec<CustomCommand>,
    /// The rows for the draft as it stands. Held rather than derived in `view`
    /// because the view borrows them, and because rebuilding the list on every
    /// frame is a walk over the command table per redraw for a value that
    /// changes only when the draft does.
    pub entries: Vec<Entry>,
    /// Where the keyboard cursor is among [`Menu::entries`].
    pub cursor: usize,
}

impl Menu {
    /// The prefix being completed, when the draft is a bare `/word` with no
    /// space yet. A space means arguments have started and the palette closes.
    pub fn prefix(draft: &str) -> Option<&str> {
        let rest = draft.strip_prefix('/')?;
        match rest.contains(char::is_whitespace) {
            true => None,
            false => Some(rest),
        }
    }

    /// Every command matching `draft`, built-ins first.
    pub fn matches(&self, draft: &str) -> Vec<Entry> {
        let Some(prefix) = Self::prefix(draft) else {
            return Vec::new();
        };
        let prefix = prefix.to_lowercase();
        let mut out: Vec<Entry> = crate::commands::COMMANDS
            .iter()
            .filter(|spec: &&CommandSpec| spec.name.starts_with(&prefix))
            .map(|spec| Entry {
                name: spec.name.to_string(),
                args: spec.args,
                detail: spec.description.to_string(),
                custom: false,
                unavailable: spec.execution(Surface::Gui) == Execution::Unavailable,
            })
            .collect();
        out.extend(
            self.custom
                .iter()
                .filter(|command| command.name.starts_with(&prefix))
                .map(|command| Entry {
                    name: command.name.clone(),
                    args: match command.expects_args() {
                        true => "[args]",
                        false => "",
                    },
                    detail: command.description.clone().unwrap_or_default(),
                    custom: true,
                    unavailable: false,
                }),
        );
        out
    }

    /// Move the cursor, wrapping — so ↑ from the first row reaches the last.
    pub fn step(&mut self, by: i32) {
        let len = self.entries.len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let len = len as i32;
        self.cursor = (((self.cursor as i32 + by) % len + len) % len) as usize;
    }

    /// Rebuild the rows for `draft`, and put the cursor back at the top.
    pub fn sync(&mut self, draft: &str) {
        self.entries = self.matches(draft);
        self.cursor = 0;
    }

    pub fn view<'a, M: Clone + 'a>(
        &'a self,
        pick: impl Fn(usize) -> M,
        palette: &Palette,
    ) -> Option<Element<'a, M>> {
        let entries = &self.entries;
        if entries.is_empty() {
            return None;
        }
        let mut rows = column![].spacing(1).width(Length::Fill);
        for (index, entry) in entries.iter().enumerate() {
            let dim = match entry.unavailable {
                true => Token::Faint,
                false => Token::Text,
            };
            let tag = match (entry.custom, entry.unavailable) {
                (_, true) => "TERMINAL ONLY",
                (true, _) => "CUSTOM",
                _ => "",
            };
            // The description is the elastic half and the tag is not. A
            // custom command's `detail` is whatever its author wrote, and as
            // the `Shrink` side of a `spread` it starves the `CUSTOM` /
            // `TERMINAL ONLY` tag beside it — which is the only thing on the
            // row saying this command is not one of Wizard's own, or will not
            // run in this window.
            let line = row![
                container(
                    row![
                        text(format!("/{}", entry.name))
                            .size(chrome::UI)
                            .font(crate::native::font::MONO)
                            .color(palette.color(dim)),
                        text(entry.args)
                            .size(chrome::LITERAL)
                            .font(crate::native::font::MONO)
                            .color(palette.color(Token::Faint)),
                        text(entry.detail.as_str())
                            .size(chrome::SMALL)
                            .color(palette.color(Token::Muted)),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                )
                .width(Length::Fill)
                .clip(true),
                text(tag)
                    .size(chrome::LABEL)
                    .color(palette.color(Token::Faint)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center);
            rows = rows.push(chrome::pick(
                line,
                pick(index),
                index == self.cursor,
                palette,
            ));
        }
        Some(
            container(chrome::scroll(rows).height(Length::Shrink))
                .max_height(300.0)
                .width(Length::Fill)
                .padding(Padding::new(5.0))
                .style({
                    let surface = palette.surface;
                    let hairline = palette.hairline;
                    move |_theme| container::Style {
                        background: Some(iced::Background::Color(surface)),
                        border: Border {
                            color: hairline,
                            width: 1.0,
                            radius: 10.0.into(),
                        },
                        ..container::Style::default()
                    }
                })
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> SessionSnapshot {
        use crate::config::ProviderKind;
        SessionSnapshot {
            model: "grok-4.5".to_string(),
            provider_name: "xai".to_string(),
            provider_kind: ProviderKind::XAI,
            provider_base_url: "https://api.x.ai/v1".to_string(),
            mode: Mode::Sovereign,
            effort: None,
            max_steps: None,
            session: None,
            prompt_tokens: 0,
            cache_tokens: None,
            completion_tokens: 0,
            context_tokens: None,
            background_tasks: None,
            todos: (0, 0),
            plan: PlanState::default(),
            ultra: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }
    }

    async fn actions(line: &str) -> Vec<Action> {
        match route(line, &[]) {
            Route::Window(command) => {
                run(
                    command,
                    PathBuf::from("/src/wizard"),
                    snapshot(),
                    PlanState::default(),
                )
                .await
            }
            other => panic!("{line} routed to {other:?}"),
        }
    }

    /// The routing decision, which is the whole design: a command that changes
    /// the conversation goes to the worker, one that changes the window stays
    /// here, and neither list is written down anywhere but the table.
    #[test]
    fn commands_route_by_the_tables_own_column() {
        assert_eq!(
            route("/model grok-4.5", &[]),
            Route::Agent {
                name: "model".to_string(),
                args: "grok-4.5".to_string()
            }
        );
        assert_eq!(
            route("/compact", &[]),
            Route::Agent {
                name: "compact".to_string(),
                args: String::new()
            }
        );
        assert!(matches!(
            route("/diff", &[]),
            Route::Window(SlashCommand::Diff)
        ));
        assert!(matches!(
            route("/settings", &[]),
            Route::Window(SlashCommand::Settings)
        ));
        // A refusal is still the window's, so `dispatch` gets to explain what
        // the command is rather than the line vanishing.
        assert!(matches!(
            route("/vim", &[]),
            Route::Window(SlashCommand::Vim)
        ));
        assert_eq!(
            route("hello there", &[]),
            Route::Message("hello there".to_string())
        );
        // A custom command is a message: `preprocess` expands it on the way in,
        // on every surface. Without the workspace's list this is a refusal,
        // because the built-in parser has never heard of it.
        let custom = vec![CustomCommand {
            name: "deploy".to_string(),
            description: None,
            template: "ship it".to_string(),
            path: PathBuf::from("/src/.wizard/commands/deploy.md"),
        }];
        assert_eq!(
            route("/deploy staging", &custom),
            Route::Message("/deploy staging".to_string())
        );
        assert!(matches!(route("/deploy staging", &[]), Route::Refused(_),));
    }

    /// Every routing decision is exhaustive over the table. A command with no
    /// home is one the palette offers and nothing runs.
    #[test]
    fn every_command_in_the_table_has_somewhere_to_go() {
        for spec in crate::commands::COMMANDS {
            let line = match spec.name {
                // The two that need an argument to parse at all.
                "memory" => "/memory".to_string(),
                name => format!("/{name}"),
            };
            let routed = route(&line, &[]);
            assert!(
                !matches!(routed, Route::Message(_)),
                "/{} is in the table but routes as a message: {routed:?}",
                spec.name
            );
        }
    }

    /// The window's verbs produce actions rather than prose, and the prose that
    /// does appear is `dispatch`'s.
    #[tokio::test]
    async fn window_commands_produce_actions() {
        assert_eq!(actions("/diff").await, vec![Action::ShowDiff(None)]);
        assert_eq!(
            actions("/todos").await,
            vec![Action::TogglePanel(Panel::Todos)]
        );
        assert_eq!(actions("/clear").await, vec![Action::NewChat]);
        assert_eq!(
            actions("/settings").await,
            vec![Action::OpenSettings { picker: false }]
        );
        assert_eq!(
            actions("/provider").await,
            vec![Action::OpenSettings { picker: true }]
        );
        assert_eq!(
            actions("/login xai").await,
            vec![Action::OpenSettings { picker: true }]
        );
        assert_eq!(actions("/resume").await, vec![Action::Resume(None)]);
    }

    /// A command the surface does not run is refused **by the dispatcher**,
    /// with the sentence that says what it is. If this window ever grew its own
    /// refusal, `/vim` would mean two things.
    #[tokio::test]
    async fn an_unavailable_command_is_refused_in_the_registrys_words() {
        let actions = actions("/vim").await;
        assert!(
            matches!(&actions[..], [Action::Error(why)] if why.contains("modal editing")),
            "{actions:?}"
        );
    }

    /// `/login` to an unknown provider is refused in the one place that knows
    /// which providers exist — not here.
    #[tokio::test]
    async fn an_unknown_login_provider_is_refused_by_the_dispatcher() {
        let actions = actions("/login gopher").await;
        assert!(
            matches!(&actions[..], [Action::Error(why)] if why.contains("unknown login provider")),
            "{actions:?}"
        );
    }

    /// The palette opens on a bare `/word` and closes the moment arguments
    /// start — otherwise `/home/user/notes` would open a menu over a path.
    #[test]
    fn the_palette_completes_a_bare_slash_word_only() {
        assert_eq!(Menu::prefix("/mod"), Some("mod"));
        assert_eq!(Menu::prefix("/"), Some(""));
        assert_eq!(Menu::prefix("/model grok"), None);
        assert_eq!(Menu::prefix("hello"), None);
    }

    #[test]
    fn the_palette_lists_builtins_and_the_workspaces_own() {
        let palette = Menu {
            entries: Vec::new(),
            custom: vec![CustomCommand {
                name: "modernize".to_string(),
                description: Some("run the codemod".to_string()),
                template: "do it".to_string(),
                path: PathBuf::from("/src/.wizard/commands/modernize.md"),
            }],
            cursor: 0,
        };
        let names: Vec<String> = palette
            .matches("/mod")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["model", "mode", "modernize"]);
        assert!(palette.matches("/mod")[2].custom);
        // And a terminal-only command is listed, dimmed, rather than hidden:
        // "wizard has no /vim" is a better answer than "/vim does not exist".
        let vim = palette.matches("/vim");
        assert_eq!(vim.len(), 1);
        assert!(vim[0].unavailable);
    }

    /// The cursor wraps in both directions, so ↑ from the first row reaches the
    /// last one.
    #[test]
    fn the_palette_cursor_wraps() {
        let mut menu = Menu::default();
        menu.sync("/plan");
        assert_eq!(menu.entries.len(), 1);
        for name in ["planner", "plantuml"] {
            menu.entries.push(Entry {
                name: name.to_string(),
                args: "",
                detail: String::new(),
                custom: true,
                unavailable: false,
            });
        }
        menu.step(-1);
        assert_eq!(menu.cursor, 2);
        menu.step(1);
        assert_eq!(menu.cursor, 0);
        menu.step(5);
        assert_eq!(menu.cursor, 2);
        menu.entries.clear();
        menu.step(1);
        assert_eq!(menu.cursor, 0, "an empty list has no cursor");
    }
}
