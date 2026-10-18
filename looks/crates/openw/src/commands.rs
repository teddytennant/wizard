//! The ctrl+p palette, and openw's own slash commands.
//!
//! The palette carries opencode's entries and wording. The slash menu is the one every look
//! has (`agent_core::commands::menu`): wizard's commands, the shared ones and the few extras
//! below.

use crate::keys::{Action, Keymap};
use agent_core::SlashCommand;

/// openw's slash commands on top of the shared ones: name, argument hint, description.
pub fn extras() -> Vec<SlashCommand> {
    agent_core::commands::extras(&[
        ("copy", "", "copy the last response"),
        ("theme", "[name]", "switch theme"),
        ("thinking", "", "expand or collapse thinking"),
        ("timestamps", "", "show or hide timestamps"),
    ])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Suggested,
    Session,
    Agent,
    Provider,
    System,
    Prompt,
    Vcs,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Suggested => "Suggested",
            Category::Session => "Session",
            Category::Agent => "Agent",
            Category::Provider => "Provider",
            Category::System => "System",
            Category::Prompt => "Prompt",
            Category::Vcs => "VCS",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Command {
    pub title: String,
    /// Muted text after the palette title (`Move to another project dir`).
    pub palette_desc: Option<String>,
    pub category: Category,
    pub action: Action,
    pub keybind: String,
    pub suggested: bool,
    pub session_only: bool,
    pub home_only: bool,
}

/// What the table needs to know about the app to word and filter its entries.
#[derive(Default)]
pub struct CmdCtx {
    pub in_session: bool,
    pub has_sessions: bool,
    pub sidebar_visible: bool,
    pub conceal: bool,
    pub thinking_shown: bool,
    pub timestamps: bool,
    pub tool_details: bool,
    pub generic_output: bool,
    pub scrollbar: bool,
    pub prompt_has_text: bool,
    pub stash_nonempty: bool,
    pub tips_hidden: bool,
    pub light: bool,
    /// The backend reported at least one model.
    pub connected: bool,
    /// The model has variants (wizard reports effort levels), which opencode lists as
    /// `Switch model variant`.
    pub has_variants: bool,
    /// `Disable terminal title` / `Enable terminal title` and the same for animations.
    pub title_enabled: bool,
    pub animations: bool,
}

struct B<'a> {
    keymap: &'a Keymap,
    out: Vec<Command>,
}

impl B<'_> {
    fn add(&mut self, title: &str, cat: Category, action: Action) -> &mut Command {
        let keybind = self.keymap.display(&action);
        self.out.push(Command {
            title: title.to_string(),
            palette_desc: None,
            category: cat,
            action,
            keybind,
            suggested: false,
            session_only: false,
            home_only: false,
        });
        self.out.last_mut().expect("just pushed")
    }
}

impl Command {
    fn suggest(&mut self) -> &mut Self {
        self.suggested = true;
        self
    }
    fn session(&mut self) -> &mut Self {
        self.session_only = true;
        self
    }
    fn home(&mut self) -> &mut Self {
        self.home_only = true;
        self
    }
}

/// Build the palette for the current state. Order within a category is the order entries are
/// added here.
pub fn build(ctx: &CmdCtx, keymap: &Keymap) -> Vec<Command> {
    use Category::*;
    let mut b = B {
        keymap,
        out: Vec::new(),
    };
    let s = ctx.in_session;

    // Session group. opencode hides `Share session` when sharing is disabled in its config, and
    // wizard has no sharing, so it stays out.
    {
        b.add("Rename session", Session, Action::Rename).session();
        b.add("Jump to message", Session, Action::Timeline)
            .session();
        b.add("Fork session", Session, Action::Fork).session();
        b.add("Compact session", Session, Action::Compact).session();
        b.add("Undo previous message", Session, Action::Undo)
            .session();
        let t = if ctx.sidebar_visible {
            "Hide sidebar"
        } else {
            "Show sidebar"
        };
        b.add(t, Session, Action::SidebarToggle).session();
        let t = if ctx.conceal {
            "Disable code concealment"
        } else {
            "Enable code concealment"
        };
        b.add(t, Session, Action::ToggleConceal).session();
        let t = if ctx.timestamps {
            "Hide timestamps"
        } else {
            "Show timestamps"
        };
        b.add(t, Session, Action::ToggleTimestamps).session();
        let t = if ctx.thinking_shown {
            "Collapse thinking"
        } else {
            "Expand thinking"
        };
        b.add(t, Session, Action::ToggleThinking).session();
        let t = if ctx.tool_details {
            "Hide tool details"
        } else {
            "Show tool details"
        };
        b.add(t, Session, Action::ToggleToolDetails).session();
        b.add("Toggle session scrollbar", Session, Action::ToggleScrollbar)
            .session();
        let t = if ctx.generic_output {
            "Hide generic tool output"
        } else {
            "Show generic tool output"
        };
        b.add(t, Session, Action::ToggleGenericOutput).session();
        b.add("Copy last assistant message", Session, Action::CopyLast)
            .session();
        b.add("Copy session transcript", Session, Action::CopyTranscript)
            .session();
        b.add("Export session transcript", Session, Action::Export)
            .session();
    }
    let editor = |b: &mut B| {
        b.add("Open editor", Session, Action::Editor);
        b.add("Move session", Session, Action::MoveSession)
            .palette_desc = Some("Move to another project dir".into());
    };
    let switcher = |b: &mut B| {
        let c = b.add("Switch session", Session, Action::Sessions);
        c.suggested = ctx.has_sessions;
        let c = b.add("New session", Session, Action::NewSession);
        c.suggested = s;
    };
    // Registration order in opencode moves with timing: after the first turn the switcher
    // re-registers behind the editor entries on some runs and not on others. Three of the four
    // in-session captures put Switch/New ahead of Open editor, so that is the order here; the
    // home screen is stable and reads editor first.
    if s {
        switcher(&mut b);
        editor(&mut b);
    } else {
        editor(&mut b);
        switcher(&mut b);
    }
    // Prompt group.
    if ctx.prompt_has_text {
        b.add("Stash prompt", Prompt, Action::StashPush);
    }
    if ctx.stash_nonempty {
        b.add("Stash pop", Prompt, Action::StashPop);
        b.add("Stash list", Prompt, Action::StashList);
    }
    b.add("Skills", Prompt, Action::Skills);

    // Agent group.
    b.add("Switch model", Agent, Action::Models).suggest();
    b.add("Switch agent", Agent, Action::Agents);
    b.add("Toggle MCPs", Agent, Action::Mcps);
    b.add("Variant cycle", Agent, Action::VariantCycle);
    if ctx.has_variants {
        b.add("Switch model variant", Agent, Action::Variants);
    }

    // Provider.
    let c = b.add("Connect provider", Provider, Action::Connect);
    c.suggested = !ctx.connected;

    // System group. Tips and plugins register first on the home screen and last in a session,
    // saved sessions or not (checked against opencode with a relaunch over an existing one).
    let tips_plugins = |b: &mut B| {
        if !s {
            let t = if ctx.tips_hidden {
                "Show tips"
            } else {
                "Hide tips"
            };
            b.add(t, System, Action::TipsToggle).home();
        }
        b.add("Plugins", System, Action::Plugins);
        b.add("Install plugin", System, Action::InstallPlugin);
    };
    if !s {
        tips_plugins(&mut b);
    }
    b.add("View status", System, Action::Status);
    b.add("View debug info", System, Action::Debug);
    b.add("Switch theme", System, Action::Themes);
    let t = if ctx.light {
        "Switch to dark mode"
    } else {
        "Switch to light mode"
    };
    b.add(t, System, Action::ToggleMode);
    b.add("Lock theme mode", System, Action::LockThemeMode);
    b.add("Help", System, Action::Help);
    b.add("Open docs", System, Action::OpenDocs);
    b.add("Exit the app", System, Action::Exit);
    b.add("Toggle debug panel", System, Action::DebugPanel);
    b.add("Toggle console", System, Action::Console);
    b.add("Write heap snapshot", System, Action::HeapSnapshot);
    let t = if ctx.title_enabled {
        "Disable terminal title"
    } else {
        "Enable terminal title"
    };
    b.add(t, System, Action::ToggleTerminalTitle);
    let t = if ctx.animations {
        "Disable animations"
    } else {
        "Enable animations"
    };
    b.add(t, System, Action::ToggleAnimations);
    for (title, what) in [
        ("Disable file context", "file context"),
        ("Disable diff wrapping", "diff wrapping"),
        ("Disable paste summary", "paste summary"),
        (
            "Disable session directory filtering",
            "session directory filtering",
        ),
        (
            "Enable auto-approve permissions",
            "auto-approve permissions",
        ),
    ] {
        b.add(title, System, Action::Unsupported(what));
    }
    if s {
        tips_plugins(&mut b);
    }

    // VCS.
    b.add("Open diff viewer", Vcs, Action::Diff);

    b.out
}

/// Palette categories in the order each screen shows them, as the real palette lists them in
/// the states captured in `reference/extra/dialog-palette-*` and re-checked by
/// `tools/critic/fid/f01_palette.py`: the order follows registration, so it moves with whether
/// the stash holds anything. The home screen reads the same with or without saved sessions.
pub fn category_order(in_session: bool, has_sessions: bool, stash: bool) -> &'static [Category] {
    use Category::*;
    match (in_session, has_sessions, stash) {
        (false, true, true) => &[Suggested, Prompt, Session, Agent, Provider, System, Vcs],
        (false, _, _) => &[Suggested, System, Session, Prompt, Vcs, Agent, Provider],
        (_, _, true) => &[Suggested, Session, Prompt, Agent, Provider, System, Vcs],
        _ => &[Suggested, Session, Agent, Provider, System, Prompt, Vcs],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_menu_is_wizards_commands_the_shared_ones_and_the_extras() {
        let be: Vec<SlashCommand> = ["model", "compact", "effort", "agents", "diff"]
            .into_iter()
            .map(|n| SlashCommand {
                name: n.into(),
                description: format!("wizard {n}"),
                input_hint: String::new(),
            })
            .collect();
        let menu = agent_core::commands::menu(&be, &extras());
        let names: Vec<&str> = menu.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "model",
                "compact",
                "effort",
                "agents",
                "diff",
                "clear",
                "exit",
                "export",
                "help",
                "quit",
                "resume",
                "settings",
                "copy",
                "theme",
                "thinking",
                "timestamps"
            ]
        );
    }

    fn titles(cmds: &[Command], cat: Category) -> Vec<String> {
        cmds.iter()
            .filter(|c| c.category == cat)
            .filter(|c| !c.session_only || cmds_in_session(cmds))
            .map(|c| c.title.clone())
            .collect()
    }

    fn cmds_in_session(cmds: &[Command]) -> bool {
        // a session build has no home-only entry
        !cmds.iter().any(|c| c.home_only)
    }

    /// Orders below are what real opencode 1.18.34 listed in `tools/critic/fid/f01_palette.py`
    /// with sharing disabled in its config, which wizard cannot do either.
    #[test]
    fn home_palette_matches_opencode_with_or_without_saved_sessions() {
        let km = Keymap::new();
        for has_sessions in [false, true] {
            let cmds = build(
                &CmdCtx {
                    has_sessions,
                    ..Default::default()
                },
                &km,
            );
            assert_eq!(
                &titles(&cmds, Category::System)[..4],
                ["Hide tips", "Plugins", "Install plugin", "View status"]
            );
            assert_eq!(titles(&cmds, Category::System).len(), 21);
            assert_eq!(
                titles(&cmds, Category::Session),
                [
                    "Open editor",
                    "Move session",
                    "Switch session",
                    "New session"
                ]
            );
            assert_eq!(
                category_order(false, has_sessions, false),
                [
                    Category::Suggested,
                    Category::System,
                    Category::Session,
                    Category::Prompt,
                    Category::Vcs,
                    Category::Agent,
                    Category::Provider
                ]
            );
        }
    }

    #[test]
    fn session_palette_has_no_share_entry_and_plugins_come_last_in_system() {
        let km = Keymap::new();
        let cmds = build(
            &CmdCtx {
                in_session: true,
                has_sessions: true,
                ..Default::default()
            },
            &km,
        );
        assert!(!cmds.iter().any(|c| c.title == "Share session"));
        let sys = titles(&cmds, Category::System);
        assert_eq!(sys[0], "View status");
        assert_eq!(&sys[sys.len() - 2..], ["Plugins", "Install plugin"]);
        let ses = titles(&cmds, Category::Session);
        assert_eq!(ses[0], "Rename session");
        assert_eq!(ses.last().unwrap(), "Move session");
    }

    #[test]
    fn switch_model_variant_follows_the_model_having_variants() {
        let km = Keymap::new();
        let with = |has_variants| {
            let cmds = build(
                &CmdCtx {
                    has_variants,
                    ..Default::default()
                },
                &km,
            );
            titles(&cmds, Category::Agent)
        };
        assert_eq!(
            with(true),
            [
                "Switch model",
                "Switch agent",
                "Toggle MCPs",
                "Variant cycle",
                "Switch model variant"
            ]
        );
        assert_eq!(with(false).len(), 4);
    }
}
