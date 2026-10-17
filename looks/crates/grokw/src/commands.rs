// OWNER: commands (a command's own output text)
//! The `/` menu and who runs a typed `/name`: wizard's advertised commands go to wizard as typed,
//! the shared ones (`agent_core::commands::SHARED`) and grokw's own extras run here.

use agent_core::commands::{help_text, menu, resolve, Command, Shared};
use agent_core::transcript::{Part, Role};
use agent_core::{Request, SlashCommand};

use crate::app::{App, Screen};
use crate::ui::composer::PopupItem;

/// grokw's own commands, on top of wizard's and the shared ones.
const EXTRAS: &[(&str, &str, &str)] = &[
    ("theme", "<theme>", "Switch the color theme"),
    ("timestamps", "", "Toggle message timestamps on/off"),
    (
        "compact-mode",
        "",
        "Toggle compact UI (less padding, more content)",
    ),
    (
        "vim-mode",
        "",
        "Toggle vim-style scrollback keybindings (j/k, h/l, g/G, y/Y, …)",
    ),
    (
        "multiline",
        "",
        "Toggle multiline input mode (swap Enter and Shift+Enter)",
    ),
    ("copy", "", "Copy the last response to the clipboard"),
];

pub fn extras() -> Vec<SlashCommand> {
    agent_core::commands::extras(EXTRAS)
}

/// The rows the popup lists: wizard's commands, the shared ones, then grokw's extras.
pub fn menu_of(app: &App) -> Vec<SlashCommand> {
    menu(&app.commands, &extras())
}

/// Whether a typed `/name` runs here or as wizard's own command; anything else is sent as a prompt.
pub fn is_known(app: &App, name: &str) -> bool {
    resolve(name, &extras()) != Command::Wizard || app.commands.iter().any(|c| c.name == name)
}

pub fn hint_for(app: &App, name: &str) -> Option<String> {
    menu_of(app)
        .into_iter()
        .find(|c| c.name == name)
        .and_then(|c| (!c.input_hint.is_empty()).then_some(c.input_hint))
}

/// Popup rows for `/query`. Bare `/` lists the menu, recently used first. A query ranks by nucleo
/// score, then the typed name, recency and last the name.
pub fn slash_items(app: &App, query: &str) -> Vec<PopupItem> {
    let menu = menu_of(app);
    let recency = |name: &str| app.inp.mru_score(name);
    let q = query.trim();
    let mut rows: Vec<(&SlashCommand, Vec<u32>)> = Vec::new();
    if q.is_empty() {
        rows = menu.iter().map(|c| (c, Vec::new())).collect();
        rows.sort_by_key(|(c, _)| std::cmp::Reverse(recency(&c.name)));
    } else if !q.contains('/') {
        let pat = crate::ui::composer::fuzz::Query::new(q);
        let mut scored: Vec<(&SlashCommand, u32)> = menu
            .iter()
            .filter_map(|c| Some((c, pat.score(&c.name, false)?)))
            .collect();
        scored.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| (b.0.name == q).cmp(&(a.0.name == q)))
                .then_with(|| recency(&b.0.name).cmp(&recency(&a.0.name)))
                .then_with(|| a.0.name.cmp(&b.0.name))
        });
        rows = scored
            .into_iter()
            .map(|(c, _)| {
                let hits = pat
                    .indices(&format!("/{}", c.name), false)
                    .unwrap_or_default();
                (c, hits)
            })
            .collect();
    }
    rows.into_iter()
        .map(|(c, hits)| PopupItem {
            label: format!("/{}", c.name),
            desc: c.description.clone(),
            insert: if c.input_hint.is_empty() {
                format!("/{}", c.name)
            } else {
                format!("/{} ", c.name)
            },
            hits: hits.into_iter().map(|h| h as usize).collect(),
            range: None,
            dir: false,
        })
        .collect()
}

pub const THEMES: [(&str, &str); 6] = [
    ("auto", "auto (follow system)"),
    ("groknight", "groknight"),
    ("grokday", "grokday"),
    ("tokyonight", "tokyonight"),
    ("rosepine-moon", "rosepine-moon"),
    ("oscura-midnight", "oscura-midnight"),
];

/// The names `/theme` offers; `terminal` only once its rollout gate is open.
pub fn theme_names() -> Vec<(&'static str, &'static str)> {
    let mut v = THEMES.to_vec();
    if crate::theme::terminal_gate() {
        v.push(("terminal", "terminal (native colors)"));
    }
    v
}

/// A model the typed text starts with, longest name first: `(id, name, rest)`.
fn split_model<'a>(app: &App, arg: &'a str) -> Option<(String, String, &'a str)> {
    let mut best: Option<(String, String, &str)> = None;
    for m in &app.config.models {
        for cand in [&m.name, &m.id] {
            if cand.is_empty() {
                continue;
            }
            let ok = arg.len() > cand.len()
                && arg[..cand.len()].eq_ignore_ascii_case(cand)
                && arg.as_bytes()[cand.len()] == b' ';
            if ok && best.as_ref().is_none_or(|(_, n, _)| cand.len() > n.len()) {
                best = Some((
                    m.id.clone(),
                    cand.to_string(),
                    arg[cand.len() + 1..].trim_start(),
                ));
            }
        }
    }
    best
}

fn find_model(app: &App, arg: &str) -> Option<String> {
    let a = arg.trim();
    app.config
        .models
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(a) || m.name.eq_ignore_ascii_case(a))
        .or_else(|| {
            app.config.models.iter().find(|m| {
                m.id.rsplit('/')
                    .next()
                    .is_some_and(|t| t.eq_ignore_ascii_case(a))
            })
        })
        .map(|m| m.id.clone())
}

/// `(score, matched char indices)` of `q` against `text`; an empty query matches everything.
fn fz(q: &str, text: &str) -> Option<(u32, Vec<usize>)> {
    let pat = crate::ui::composer::fuzz::Query::new(q);
    if pat.is_empty() {
        return Some((0, Vec::new()));
    }
    let sc = pat.score(text, false)?;
    let hits = pat.indices(text, false).unwrap_or_default();
    Some((sc, hits.into_iter().map(|h| h as usize).collect()))
}

/// Argument rows keep their order for an empty query; a typed one ranks an exact value first
/// (`high` is not `Extra High`), then by score, then name.
fn rank_args(q: &str, v: &mut [(u32, PopupItem)]) {
    let q = q.trim();
    if !q.is_empty() {
        let exact = |i: &PopupItem| {
            i.insert
                .rsplit(' ')
                .next()
                .is_some_and(|v| v.eq_ignore_ascii_case(q))
        };
        v.sort_by(|a, b| {
            exact(&b.1)
                .cmp(&exact(&a.1))
                .then(b.0.cmp(&a.0))
                .then_with(|| a.1.label.cmp(&b.1.label))
        });
    }
}

/// The looks `/ui` switches to, as wizard names them.
const LOOKS: [&str; 7] = [
    "wizard", "codex", "grok", "opencode", "pi", "jcode", "claude",
];

/// Argument rows for `/cmd <arg>`: the model list, the effort list, the themes.
pub fn arg_items(app: &App, cmd: &str, arg: &str) -> Vec<PopupItem> {
    match cmd {
        "model" => {
            if let Some((id, name, rest)) = split_model(app, arg) {
                // effort phase: the model's effort levels
                let _ = id;
                return effort_rows(app, rest, &format!("/model {name} "));
            }
            let q = arg.trim();
            let mut v: Vec<(u32, PopupItem)> = Vec::new();
            for m in &app.config.models {
                let disp = if m.name.is_empty() { &m.id } else { &m.name };
                let Some((score, hits)) = fz(q, disp) else {
                    continue;
                };
                let current = m.id == app.config.model;
                let label = if current {
                    format!("{disp} (current)")
                } else {
                    disp.clone()
                };
                let chain = !app.config.efforts.is_empty();
                v.push((
                    score,
                    PopupItem {
                        label,
                        desc: m.provider.clone(),
                        insert: if chain {
                            format!("/model {disp} ")
                        } else {
                            format!("/model {disp}")
                        },
                        hits,
                        range: None,
                        dir: false,
                    },
                ));
            }
            rank_args(q, &mut v);
            v.into_iter().map(|(_, i)| i).collect()
        }
        "effort" => effort_rows(app, arg, "/effort "),
        "theme" => {
            let q = arg.trim();
            let mut v: Vec<(u32, PopupItem)> = theme_names()
                .iter()
                .filter_map(|(n, d)| {
                    let (score, hits) = fz(q, n)?;
                    Some((
                        score,
                        PopupItem {
                            label: (*n).to_string(),
                            desc: if *n == app.theme.kind.canonical() {
                                format!("{n} (active)")
                            } else {
                                (*d).to_string()
                            },
                            insert: format!("/theme {n}"),
                            hits,
                            range: None,
                            dir: false,
                        },
                    ))
                })
                .collect();
            rank_args(q, &mut v);
            v.into_iter().map(|(_, i)| i).collect()
        }
        "ui" => {
            let q = arg.trim();
            let mut v: Vec<(u32, PopupItem)> = LOOKS
                .iter()
                .filter_map(|n| {
                    let (score, hits) = fz(q, n)?;
                    Some((
                        score,
                        PopupItem {
                            label: (*n).to_string(),
                            desc: if *n == "grok" {
                                "this look".to_string()
                            } else {
                                String::new()
                            },
                            insert: format!("/ui {n}"),
                            hits,
                            range: None,
                            dir: false,
                        },
                    ))
                })
                .collect();
            rank_args(q, &mut v);
            v.into_iter().map(|(_, i)| i).collect()
        }
        _ => Vec::new(),
    }
}

fn effort_rows(app: &App, q: &str, prefix: &str) -> Vec<PopupItem> {
    let q = q.trim();
    let mut v: Vec<(u32, PopupItem)> = app
        .config
        .efforts
        .iter()
        .filter_map(|e| {
            let name = effort_name(e);
            let (score, hits) = fz(q, &name)?;
            let label = if *e == app.config.effort {
                format!("{name} (active)")
            } else {
                name
            };
            Some((
                score,
                PopupItem {
                    label,
                    desc: effort_desc(e).to_string(),
                    insert: format!("{prefix}{e}"),
                    hits,
                    range: None,
                    dir: false,
                },
            ))
        })
        .collect();
    rank_args(q, &mut v);
    v.into_iter().map(|(_, i)| i).collect()
}

/// Display name of an effort level: `xhigh` is `Extra High`.
fn effort_name(e: &str) -> String {
    match e {
        "xhigh" => "Extra High".to_string(),
        "" => String::new(),
        other => {
            let mut cs = other.chars();
            cs.next()
                .map(|f| f.to_uppercase().collect::<String>() + cs.as_str())
                .unwrap_or_default()
        }
    }
}

fn effort_desc(e: &str) -> &'static str {
    match e {
        "xhigh" => "Maximum reasoning for the hardest tasks.",
        "high" => "Thorough reasoning and quality. Recommended.",
        "medium" => "Strong quality with a faster turnaround.",
        "low" => "Fastest responses. Best for simple tasks.",
        "default" => "The model's own default effort.",
        _ => "",
    }
}

fn say(app: &mut App, msg: &str) {
    app.enter_session();
    app.note(msg);
}

/// Wizard's own command, sent without echoing it as a user message.
fn forward(app: &mut App, text: String) {
    app.enter_session();
    if app.busy() {
        app.queue.push(text);
        return;
    }
    app.send(Request::Prompt(text));
}

/// Run `/name args`: the shared commands and grokw's extras here, anything else goes to wizard as
/// typed.
pub fn run(app: &mut App, name: &str, args: &str) {
    app.dirty = true;
    match resolve(name, &extras()) {
        Command::Shared(Shared::Clear) => app.new_session(),
        Command::Shared(Shared::Exit) => app.quit_now(),
        Command::Shared(Shared::Help) => {
            let text = format!(
                "{}\n\nCtrl+. lists the keyboard shortcuts, Ctrl+p opens the command palette.",
                help_text(&menu_of(app))
            );
            say(app, &text);
        }
        Command::Shared(Shared::Model) => cmd_model(app, args),
        Command::Shared(Shared::Resume) if args.is_empty() => {
            app.enter_session();
            app.open_resume();
        }
        Command::Shared(Shared::Resume) => app.resume_session(args.to_string()),
        Command::Extra(extra) => run_extra(app, extra, args),
        // wizard can only answer a bare `/effort` with usage text; the effort list is a picker
        Command::Wizard if name == "effort" && args.is_empty() => {
            app.ed.set_text("/effort ");
            app.enter_session();
        }
        Command::Wizard if args.is_empty() => forward(app, format!("/{name}")),
        Command::Wizard => forward(app, format!("/{name} {args}")),
    }
}

fn run_extra(app: &mut App, name: &str, args: &str) {
    match name {
        "theme" => cmd_theme(app, args),
        "copy" => cmd_copy(app),
        "multiline" => {
            app.multiline = !app.multiline;
            let t = format!("✓ Multiline: {}", on_off(app.multiline));
            app.toast(t);
        }
        "compact-mode" => {
            app.compact_mode = !app.compact_mode;
            let t = format!("✓ Compact mode: {}", on_off(app.compact_mode));
            app.toast(t);
        }
        "timestamps" => {
            app.timestamps = !app.timestamps;
            let t = format!("✓ Timestamps: {}", on_off(app.timestamps));
            app.toast(t);
        }
        "vim-mode" => {
            app.vim_mode = !app.vim_mode;
            let t = format!("Vim mode: {}", on_off(app.vim_mode));
            say(app, &t);
        }
        _ => {}
    }
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

fn cmd_model(app: &mut App, args: &str) {
    if args.is_empty() {
        app.ed.set_text("/model ");
        app.enter_session();
        return;
    }
    let (model_arg, effort) = match split_model(app, &format!("{} ", args.trim_end())) {
        Some((_, name, rest)) if !rest.trim().is_empty() => (name, Some(rest.trim().to_string())),
        _ => (args.to_string(), None),
    };
    match find_model(app, &model_arg) {
        Some(id) => {
            app.enter_session();
            app.chosen_model = Some(id.clone());
            app.send(Request::SetModel(id));
            if let Some(e) = effort {
                app.chosen_effort = Some(e.clone());
                app.send(Request::SetEffort(e));
            }
        }
        None => say(app, &format!("Unknown model: {args}")),
    }
}

/// `/theme` with no argument moves to the next palette; a name or alias switches to it. The choice
/// is kept in the state directory for the next launch.
fn cmd_theme(app: &mut App, args: &str) {
    app.enter_session();
    if app.opts.screen == crate::term::ScreenMode::Minimal {
        say(
            app,
            "/theme is not available in minimal mode: it uses the terminal's own colors.",
        );
        return;
    }
    let gate = crate::theme::terminal_gate();
    let want = args.trim();
    let kind = if want.is_empty() {
        Some(app.theme.next_kind(gate))
    } else {
        crate::theme::Kind::parse(want).filter(|k| *k != crate::theme::Kind::Terminal || gate)
    };
    match kind {
        Some(k) => {
            app.theme = crate::theme::Theme::of(k);
            app.theme.save(app.opts.state_dir.as_deref());
            app.toast(format!("✓ Theme: {}", k.pretty()));
        }
        None if matches!(want.to_ascii_lowercase().as_str(), "auto" | "system") => {
            let k = if crate::theme::appearance_is_dark() {
                crate::theme::Kind::Groknight
            } else {
                crate::theme::Kind::Grokday
            };
            app.theme = crate::theme::Theme::of(k);
            app.toast(format!("✓ Theme: {}", k.pretty()));
        }
        None => {
            let list = theme_names()
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ");
            say(app, &format!("Unknown theme: {want}. Available: {list}"));
        }
    }
    app.dirty = true;
}

/// The text of the latest assistant message.
fn last_reply(app: &App) -> Option<String> {
    app.tr
        .messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .find_map(|m| {
            let t: String = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            (!t.trim().is_empty()).then_some(t)
        })
}

fn cmd_copy(app: &mut App) {
    match last_reply(app) {
        Some(text) => {
            crate::app::copy_to_clipboard(&text);
            app.toast_for("Copied!", 30);
        }
        None => say(app, "No assistant messages to copy"),
    }
}

/// Screen the command ran from, for the palette later.
pub fn from_home(app: &App) -> bool {
    app.screen == Screen::Home
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppOpts;
    use tokio::sync::mpsc::unbounded_channel;

    fn app() -> App {
        let (tx, _) = unbounded_channel();
        let (mtx, _) = unbounded_channel();
        let mut app = App::new(AppOpts::default(), tx, mtx);
        app.config.efforts = vec!["low".into(), "medium".into(), "high".into(), "xhigh".into()];
        app
    }

    #[test]
    fn an_exact_argument_is_the_first_row() {
        let rows = arg_items(&app(), "effort", "high");
        assert_eq!(rows[0].insert, "/effort high");
    }

    #[test]
    fn leaving_omakase_stays_in_plan_mode() {
        let mut app = app();
        let notice = |text: &str| agent_core::Event::Notice {
            level: agent_core::NoticeLevel::Info,
            text: text.into(),
        };
        app.on_event(notice(
            "omakase off — back to plan mode (you review the plan)",
        ));
        assert_eq!(app.mode, crate::app::Mode::Plan);
        app.on_event(notice("plan mode off"));
        assert_eq!(app.mode, crate::app::Mode::Normal);
    }

    #[test]
    fn the_menu_is_wizards_then_shared_then_extras() {
        let mut app = app();
        app.commands = agent_core::mock::commands();
        let names: Vec<String> = slash_items(&app, "").into_iter().map(|i| i.label).collect();
        let want = [
            "/model",
            "/mode",
            "/effort",
            "/plan",
            "/compact",
            "/diff",
            "/todos",
            "/status",
            "/cost",
            "/agents",
            "/help",
            "/clear",
            "/exit",
            "/quit",
            "/resume",
            "/theme",
            "/timestamps",
            "/compact-mode",
            "/vim-mode",
            "/multiline",
            "/copy",
        ];
        assert_eq!(names, want);
    }

    #[test]
    fn only_shared_and_extras_run_here() {
        let mut app = app();
        app.commands = agent_core::mock::commands();
        for name in [
            "clear", "help", "model", "resume", "theme", "copy", "compact", "agents",
        ] {
            assert!(is_known(&app, name), "{name}");
        }
        for name in ["new", "settings", "context", "view-plan", "find", "tmp"] {
            assert!(!is_known(&app, name), "{name}");
        }
    }

    #[test]
    fn ui_offers_the_looks() {
        let rows = arg_items(&app(), "ui", "");
        let looks: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(looks, LOOKS);
        assert_eq!(arg_items(&app(), "ui", "op")[0].insert, "/ui opencode");
    }
}
