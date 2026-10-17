//! One table behind the slash popup and the ctrl+p palette.
//!
//! Frontend commands carry opencode's names and wording. Commands the backend advertises
//! (`Event::Commands`) are merged in after them; when both define a name, the frontend entry
//! wins and calls the backend underneath. Anything wizard cannot do is simply not in the table,
//! so it never shows up pretending to work.

use crate::keys::{Action, Keymap};
use agent_core::SlashCommand;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Frontend,
    Backend,
}

#[derive(Clone, Debug)]
pub struct Command {
    /// Slash name without the slash; `None` for palette-only entries.
    pub name: Option<String>,
    pub aliases: Vec<String>,
    /// Palette title.
    pub title: String,
    /// Text in the slash popup. Defaults to the title.
    pub description: String,
    /// Muted text after the palette title (`Move to another project dir`).
    pub palette_desc: Option<String>,
    pub category: Category,
    pub action: Action,
    pub keybind: String,
    pub suggested: bool,
    pub palette: bool,
    pub source: Source,
    pub session_only: bool,
    pub home_only: bool,
}

/// What the table needs to know about the app to word and filter its entries.
#[derive(Default)]
pub struct CmdCtx<'a> {
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
    pub backend: &'a [SlashCommand],
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
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        name: Option<&str>,
        aliases: &[&str],
        title: &str,
        desc: Option<&str>,
        cat: Category,
        action: Action,
    ) -> &mut Command {
        let keybind = self.keymap.display(&action);
        self.out.push(Command {
            name: name.map(String::from),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            title: title.to_string(),
            description: desc.unwrap_or(title).to_string(),
            palette_desc: None,
            category: cat,
            action,
            keybind,
            suggested: false,
            palette: true,
            source: Source::Frontend,
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

/// Names the backend refuses or that a frontend entry covers.
const HIDDEN_BACKEND: &[&str] = &[
    "model",
    "new",
    "clear",
    "exit",
    "quit",
    "resume",
    "login",
    "settings",
    "dashboard",
    "vim",
    "view",
    "resume-claude",
    "help",
    "cost",
];

/// Build the full list for the current state. Palette order within a category is the order
/// entries are added here; the slash popup sorts alphabetically itself.
pub fn build(ctx: &CmdCtx, keymap: &Keymap) -> Vec<Command> {
    use Category::*;
    let mut b = B {
        keymap,
        out: Vec::new(),
    };
    let s = ctx.in_session;

    // Session group. opencode hides `Share session` when sharing is disabled in its config, and
    // wizard has no sharing, so it stays out of the palette; typed, `/share` says so instead of
    // the popup's fuzzy match running something else. The session-only entries are
    // always built so a backend command of the same name is shadowed on
    // the home screen too; the popup and palette filter them by screen.
    {
        b.add(
            Some("share"),
            &[],
            "Share session",
            Some("wizard has no session sharing"),
            Session,
            Action::Share,
        )
        .palette = false;
        b.add(
            Some("rename"),
            &[],
            "Rename session",
            None,
            Session,
            Action::Rename,
        )
        .session();
        b.add(
            Some("timeline"),
            &[],
            "Jump to message",
            None,
            Session,
            Action::Timeline,
        )
        .session();
        b.add(
            Some("fork"),
            &[],
            "Fork session",
            None,
            Session,
            Action::Fork,
        )
        .session();
        b.add(
            Some("compact"),
            &["summarize"],
            "Compact session",
            None,
            Session,
            Action::Compact,
        )
        .session();
        b.add(
            Some("undo"),
            &[],
            "Undo previous message",
            None,
            Session,
            Action::Undo,
        )
        .session();
        let t = if ctx.sidebar_visible {
            "Hide sidebar"
        } else {
            "Show sidebar"
        };
        b.add(None, &[], t, None, Session, Action::SidebarToggle)
            .session();
        let t = if ctx.conceal {
            "Disable code concealment"
        } else {
            "Enable code concealment"
        };
        b.add(None, &[], t, None, Session, Action::ToggleConceal)
            .session();
        let t = if ctx.timestamps {
            "Hide timestamps"
        } else {
            "Show timestamps"
        };
        b.add(
            Some("timestamps"),
            &["toggle-timestamps"],
            t,
            None,
            Session,
            Action::ToggleTimestamps,
        )
        .session();
        let t = if ctx.thinking_shown {
            "Collapse thinking"
        } else {
            "Expand thinking"
        };
        b.add(
            Some("thinking"),
            &["toggle-thinking"],
            t,
            None,
            Session,
            Action::ToggleThinking,
        )
        .session();
        let t = if ctx.tool_details {
            "Hide tool details"
        } else {
            "Show tool details"
        };
        b.add(None, &[], t, None, Session, Action::ToggleToolDetails)
            .session();
        b.add(
            None,
            &[],
            "Toggle session scrollbar",
            None,
            Session,
            Action::ToggleScrollbar,
        )
        .session();
        let t = if ctx.generic_output {
            "Hide generic tool output"
        } else {
            "Show generic tool output"
        };
        b.add(None, &[], t, None, Session, Action::ToggleGenericOutput)
            .session();
        b.add(
            None,
            &[],
            "Copy last assistant message",
            None,
            Session,
            Action::CopyLast,
        )
        .session();
        b.add(
            Some("copy"),
            &[],
            "Copy session transcript",
            None,
            Session,
            Action::CopyTranscript,
        )
        .session();
        b.add(
            Some("export"),
            &[],
            "Export session transcript",
            None,
            Session,
            Action::Export,
        )
        .session();
    }
    let editor = |b: &mut B| {
        b.add(
            Some("editor"),
            &[],
            "Open editor",
            None,
            Session,
            Action::Editor,
        );
        b.add(
            Some("move"),
            &[],
            "Move session",
            None,
            Session,
            Action::MoveSession,
        )
        .palette_desc = Some("Move to another project dir".into());
        let c = b.out.last_mut().expect("just pushed");
        c.description = "Move to another project dir".into();
    };
    let switcher = |b: &mut B| {
        let c = b.add(
            Some("sessions"),
            &["resume", "continue"],
            "Switch session",
            None,
            Session,
            Action::Sessions,
        );
        c.suggested = ctx.has_sessions;
        let c = b.add(
            Some("new"),
            &["clear"],
            "New session",
            None,
            Session,
            Action::NewSession,
        );
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
        b.add(None, &[], "Stash prompt", None, Prompt, Action::StashPush);
    }
    if ctx.stash_nonempty {
        b.add(None, &[], "Stash pop", None, Prompt, Action::StashPop);
        b.add(None, &[], "Stash list", None, Prompt, Action::StashList);
    }
    b.add(Some("skills"), &[], "Skills", None, Prompt, Action::Skills);

    // Agent group.
    b.add(
        Some("models"),
        &["mo"],
        "Switch model",
        None,
        Agent,
        Action::Models,
    )
    .suggest();
    b.add(
        Some("agents"),
        &[],
        "Switch agent",
        None,
        Agent,
        Action::Agents,
    );
    b.add(Some("mcps"), &[], "Toggle MCPs", None, Agent, Action::Mcps);
    b.add(
        None,
        &[],
        "Variant cycle",
        None,
        Agent,
        Action::VariantCycle,
    );
    if ctx.has_variants {
        b.add(
            Some("variants"),
            &[],
            "Switch model variant",
            None,
            Agent,
            Action::Variants,
        );
    }

    // Provider.
    let c = b.add(
        Some("connect"),
        &[],
        "Connect provider",
        None,
        Provider,
        Action::Connect,
    );
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
            b.add(None, &[], t, None, System, Action::TipsToggle).home();
        }
        b.add(None, &[], "Plugins", None, System, Action::Plugins);
        b.add(
            None,
            &[],
            "Install plugin",
            None,
            System,
            Action::InstallPlugin,
        );
    };
    if !s {
        tips_plugins(&mut b);
    }
    b.add(
        Some("status"),
        &[],
        "View status",
        None,
        System,
        Action::Status,
    );
    b.add(
        Some("debug"),
        &[],
        "View debug info",
        None,
        System,
        Action::Debug,
    );
    b.add(
        Some("themes"),
        &["theme"],
        "Switch theme",
        None,
        System,
        Action::Themes,
    );
    let t = if ctx.light {
        "Switch to dark mode"
    } else {
        "Switch to light mode"
    };
    b.add(None, &[], t, None, System, Action::ToggleMode);
    b.add(
        None,
        &[],
        "Lock theme mode",
        None,
        System,
        Action::LockThemeMode,
    );
    b.add(Some("help"), &[], "Help", None, System, Action::Help);
    b.add(None, &[], "Open docs", None, System, Action::OpenDocs);
    b.add(
        Some("exit"),
        &["quit", "q"],
        "Exit the app",
        None,
        System,
        Action::Exit,
    );
    b.add(
        None,
        &[],
        "Toggle debug panel",
        None,
        System,
        Action::DebugPanel,
    );
    b.add(None, &[], "Toggle console", None, System, Action::Console);
    b.add(
        None,
        &[],
        "Write heap snapshot",
        None,
        System,
        Action::HeapSnapshot,
    );
    let t = if ctx.title_enabled {
        "Disable terminal title"
    } else {
        "Enable terminal title"
    };
    b.add(None, &[], t, None, System, Action::ToggleTerminalTitle);
    let t = if ctx.animations {
        "Disable animations"
    } else {
        "Enable animations"
    };
    b.add(None, &[], t, None, System, Action::ToggleAnimations);
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
        b.add(None, &[], title, None, System, Action::Unsupported(what));
    }
    if s {
        tips_plugins(&mut b);
    }

    // VCS.
    b.add(
        Some("diff"),
        &[],
        "Open diff viewer",
        None,
        Vcs,
        Action::Diff,
    );

    // Prompt templates opencode ships as slash commands; not in its palette.
    b.add(
        Some("init"),
        &[],
        "Initialize AGENTS.md",
        Some("guided AGENTS.md setup"),
        Prompt,
        Action::Template("init"),
    )
    .palette = false;
    b.add(
        Some("review"),
        &[],
        "Review changes",
        Some("review changes [commit|branch|pr], defaults to uncommitted"),
        Prompt,
        Action::Template("review"),
    )
    .palette = false;

    let mut out = b.out;

    // Slash-only descriptions that differ from the palette title.
    for c in out.iter_mut() {
        if c.title == "Switch theme" {
            c.description = "Switch theme".into();
        }
    }
    // Backend commands, after the frontend names so those win a clash.
    for sc in ctx.backend {
        if HIDDEN_BACKEND.contains(&sc.name.as_str())
            || out.iter().any(|c| {
                c.name.as_deref() == Some(sc.name.as_str())
                    || c.aliases.iter().any(|a| a == &sc.name)
            })
        {
            continue;
        }
        let mut desc = sc.description.clone();
        if !sc.input_hint.is_empty() {
            desc = format!("{desc} {}", sc.input_hint);
        }
        out.push(Command {
            name: Some(sc.name.clone()),
            aliases: Vec::new(),
            title: sc.name.clone(),
            description: desc,
            palette_desc: None,
            category: Category::Session,
            action: Action::InsertSlash(sc.name.clone()),
            keybind: String::new(),
            suggested: false,
            palette: false,
            source: Source::Backend,
            session_only: false,
            home_only: false,
        });
    }
    out
}

/// The prompt a template command sends. `args` is whatever followed the command.
pub fn template(name: &str, args: &str) -> String {
    match name {
        "init" => "Look through this repository and write an AGENTS.md at its root for coding \
                   agents: how to build, test and lint, the code style in use, how the \
                   directories are laid out, and anything an agent would get wrong without being \
                   told. Keep it short and specific to this project. If an AGENTS.md exists, \
                   improve it instead of starting over."
            .to_string(),
        _ => {
            let what = if args.is_empty() {
                "the uncommitted changes in the working tree".to_string()
            } else {
                args.to_string()
            };
            format!(
                "Review {what}. Read the diff, then report bugs, risky changes and missing \
                 tests, each with the file and line. Do not edit anything."
            )
        }
    }
}

/// Entries for the slash popup, alphabetical by name, filtered to the current screen.
/// Commands the backend added come after the frontend ones, so what opencode shows first stays
/// first.
pub fn slash_entries(cmds: &[Command], in_session: bool) -> Vec<&Command> {
    let mut v: Vec<&Command> = cmds
        .iter()
        .filter(|c| c.name.is_some())
        .filter(|c| in_session || !c.session_only)
        .filter(|c| !in_session || !c.home_only)
        .collect();
    v.sort_by(|a, b| {
        (a.source == Source::Backend, &a.name).cmp(&(b.source == Source::Backend, &b.name))
    });
    v
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

/// Look a typed `/name` up, aliases included.
pub fn find<'a>(cmds: &'a [Command], name: &str) -> Option<&'a Command> {
    cmds.iter()
        .find(|c| c.name.as_deref() == Some(name) || c.aliases.iter().any(|a| a == name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&Command]) -> Vec<String> {
        v.iter().map(|c| c.name.clone().unwrap()).collect()
    }

    #[test]
    fn home_popup_is_alphabetical_and_hides_session_commands() {
        let km = Keymap::new();
        let cmds = build(&CmdCtx::default(), &km);
        let n = names(&slash_entries(&cmds, false));
        assert!(n.windows(2).all(|w| w[0] <= w[1]), "{n:?}");
        assert!(n.contains(&"models".to_string()));
        assert!(!n.contains(&"compact".to_string()));
    }

    #[test]
    fn session_popup_has_session_commands() {
        let km = Keymap::new();
        let cmds = build(
            &CmdCtx {
                in_session: true,
                ..Default::default()
            },
            &km,
        );
        let n = names(&slash_entries(&cmds, true));
        for want in ["compact", "undo", "timeline", "export", "thinking"] {
            assert!(n.contains(&want.to_string()), "{want} missing");
        }
    }

    #[test]
    fn frontend_wins_a_name_clash_and_backend_extras_are_added() {
        let km = Keymap::new();
        let be = vec![
            SlashCommand {
                name: "compact".into(),
                description: "wizard compact".into(),
                input_hint: String::new(),
            },
            SlashCommand {
                name: "effort".into(),
                description: "set effort".into(),
                input_hint: "<low|high>".into(),
            },
            SlashCommand {
                name: "clear".into(),
                description: "refused".into(),
                input_hint: String::new(),
            },
        ];
        let cmds = build(
            &CmdCtx {
                in_session: true,
                backend: &be,
                ..Default::default()
            },
            &km,
        );
        let compact = find(&cmds, "compact").unwrap();
        assert_eq!(compact.source, Source::Frontend);
        let effort = find(&cmds, "effort").unwrap();
        assert_eq!(effort.source, Source::Backend);
        assert_eq!(effort.description, "set effort <low|high>");
        assert_eq!(find(&cmds, "clear").unwrap().action, Action::NewSession);
    }

    fn titles(cmds: &[Command], cat: Category) -> Vec<String> {
        cmds.iter()
            .filter(|c| c.category == cat && c.palette)
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
        assert!(!cmds.iter().any(|c| c.palette && c.title == "Share session"));
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

    #[test]
    fn init_and_review_are_slash_only_templates() {
        let km = Keymap::new();
        let cmds = build(&CmdCtx::default(), &km);
        for name in ["init", "review"] {
            let c = find(&cmds, name).unwrap();
            assert!(!c.palette);
            assert_eq!(c.source, Source::Frontend);
        }
        assert!(template("review", "main").starts_with("Review main."));
        assert!(template("review", "").contains("uncommitted"));
    }

    #[test]
    fn backend_commands_come_after_the_frontend_ones_in_the_popup() {
        let km = Keymap::new();
        let be = vec![SlashCommand {
            name: "effort".into(),
            description: "set effort".into(),
            input_hint: String::new(),
        }];
        let cmds = build(
            &CmdCtx {
                backend: &be,
                ..Default::default()
            },
            &km,
        );
        let n = names(&slash_entries(&cmds, false));
        assert_eq!(n.last().unwrap(), "effort");
        assert!(n.iter().position(|x| x == "themes") < n.iter().position(|x| x == "effort"));
    }

    #[test]
    fn aliases_resolve() {
        let km = Keymap::new();
        let cmds = build(&CmdCtx::default(), &km);
        assert_eq!(find(&cmds, "quit").unwrap().action, Action::Exit);
        assert_eq!(find(&cmds, "mo").unwrap().action, Action::Models);
    }
}
