// OWNER: commands (a command's own output text; the table order and descriptions are Grok's)
//! Grok Build's slash commands mapped onto wizard. The table is spec 8.3.1 (the 67 rows of the
//! popup, in order); what each does is 14.3: some run in grokw, some are forwarded to wizard as
//! its own command, and the rest print an honest "not supported by wizard" line. Commands
//! `wizard acp` advertises that Grok has no row for follow, under a `wizard` tag.

use agent_core::transcript::{Part, Role};
use agent_core::Request;

use crate::app::{App, Mode, Screen};
use crate::ui::composer::PopupItem;
use crate::ui::dialogs::Modal;

pub struct Cmd {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub desc: &'static str,
    /// Dim ghost after the command and a space.
    pub hint: &'static str,
}

const fn c(
    name: &'static str,
    aliases: &'static [&'static str],
    desc: &'static str,
    hint: &'static str,
) -> Cmd {
    Cmd {
        name,
        aliases,
        desc,
        hint,
    }
}

/// The popup rows in registry order.
pub const TABLE: &[Cmd] = &[
    c(
        "memory",
        &["mem"],
        "Browse, view, and manage your memories",
        "on|off",
    ),
    c(
        "tutorial",
        &["tour", "onboarding"],
        "Quick tips to get the most out of Wizard",
        "",
    ),
    c(
        "settings",
        &["config", "preferences", "prefs"],
        "Open the settings modal",
        "",
    ),
    c(
        "dashboard",
        &["agents-dashboard", "sessions"],
        "Open the Agent Dashboard",
        "",
    ),
    c("workflows", &[], "Browse installed workflows", ""),
    c("plugins", &["plugin"], "View plugins", ""),
    c(
        "btw",
        &[],
        "Ask a side question without interrupting",
        "<question>",
    ),
    c(
        "voice",
        &[],
        "Toggle dictation (Ctrl+Space/F8; Esc/Enter to stop)",
        "",
    ),
    c("new", &["clear"], "Start a new session", ""),
    c(
        "effort",
        &[],
        "Set reasoning effort for the current model",
        "<level>",
    ),
    c(
        "model",
        &["m"],
        "Switch the active model",
        "<model> [effort]",
    ),
    c("context", &[], "View context usage", ""),
    c(
        "compact",
        &[],
        "Compact conversation history",
        "compaction instructions",
    ),
    c(
        "fork",
        &[],
        "Branch the current session into a peer agent",
        "[--worktree|--no-worktree] [directive]",
    ),
    c("resume", &[], "Resume a previous session", ""),
    c(
        "loop",
        &[],
        "Run a prompt on a recurring interval",
        "[interval] <prompt>",
    ),
    c("plan", &[], "Enter plan mode", "[description]"),
    c(
        "view-plan",
        &["show-plan", "plan-view"],
        "View the current plan",
        "",
    ),
    c("remember", &[], "Save a memory note", "[memory note text]"),
    c("recap", &["summarize"], "Summarize the session so far", ""),
    c("rewind", &["undo"], "Rewind to a previous turn", ""),
    c("jump", &[], "Jump to a turn in the conversation", ""),
    c(
        "edit-prompt",
        &[],
        "Open an external editor for an empty prompt; use the command palette to preserve a draft",
        "",
    ),
    c(
        "queue",
        &[],
        "List the prompts queued behind the running turn",
        "",
    ),
    c("session-info", &["status", "info"], "Show session info", ""),
    c(
        "rename",
        &["title"],
        "Rename the current session",
        "<title> | --auto",
    ),
    c("history", &[], "Search prompt history", ""),
    c(
        "transcript",
        &["log"],
        "View the conversation transcript in your pager ($PAGER)",
        "",
    ),
    c(
        "export",
        &[],
        "Export the current conversation to a file or clipboard",
        "[filename]",
    ),
    c(
        "copy",
        &[],
        "Copy last response to clipboard or file (/copy [N] [file])",
        "[N] [file]",
    ),
    c("find", &[], "Search the conversation scrollback", "[text]"),
    c("usage", &["cost"], "View usage", "[show|manage]"),
    c(
        "tasks",
        &[],
        "List background tasks, subagents, and scheduled tasks",
        "",
    ),
    c("skills", &[], "View skills", ""),
    c("mcps", &[], "Show MCP server status", ""),
    c("hooks", &[], "View hooks", ""),
    c("marketplace", &[], "View marketplace", ""),
    c(
        "workflow",
        &[],
        "Launch a saved workflow, list runs, or manage a run (pause, resume, stop, save)",
        "<name> [--agent-budget N] [--effort LEVEL] [args] | runs | pause|resume|stop|save [name]",
    ),
    c(
        "personas",
        &[],
        "Manage personas (create, edit, delete)",
        "",
    ),
    c("config-agents", &["agents"], "Manage agent definitions", ""),
    c("theme", &["t"], "Switch the color theme", "<theme>"),
    c(
        "auto",
        &[],
        "Toggle auto mode (classifier approves safe tools)",
        "",
    ),
    c(
        "always-approve",
        &["yolo"],
        "Toggle always-approve mode (skip all permission prompts)",
        "",
    ),
    c(
        "vim-mode",
        &[],
        "Toggle vim-style scrollback keybindings (j/k, h/l, g/G, y/Y, …)",
        "",
    ),
    c(
        "multiline",
        &["ml"],
        "Toggle multiline input mode (swap Enter and Shift+Enter)",
        "",
    ),
    c(
        "compact-mode",
        &[],
        "Toggle compact UI (less padding, more content)",
        "",
    ),
    c("timestamps", &[], "Toggle message timestamps on/off", ""),
    c(
        "minimal",
        &[],
        "Switch this session to minimal (scrollback-native) mode, back with /fullscreen",
        "",
    ),
    c("timeline", &[], "Toggle the timeline sidebar", ""),
    c(
        "imagine",
        &[],
        "Generate an image from a text description",
        "description of the image to generate",
    ),
    c(
        "imagine-video",
        &[],
        "Generate a video from a text description",
        "description of the video to generate",
    ),
    c(
        "docs",
        &["howto", "guides"],
        "Open How-to Guides or online Build docs",
        "[web|title]",
    ),
    c(
        "release-notes",
        &["changelog"],
        "View release notes for the current version",
        "",
    ),
    c(
        "feedback",
        &[],
        "Send feedback about the current session",
        "[feedback text]",
    ),
    c(
        "privacy",
        &[],
        "Open coding data, retention, and training settings",
        "",
    ),
    c(
        "doctor",
        &["terminal-setup", "terminal-check", "terminal-info"],
        "Check this session and show available fixes",
        "[fix [ssh-wrap|tmux-clipboard|dcs-passthrough|tmux-extended-keys]]",
    ),
    c(
        "import-claude",
        &[],
        "Open the Claude settings import modal",
        "",
    ),
    c(
        "login",
        &[],
        "Log in or re-authenticate with your account",
        "",
    ),
    c("logout", &[], "Log out and return to the login screen", ""),
    c("home", &["welcome"], "Return to the welcome screen", ""),
    c("delete", &[], "Delete this session", ""),
    c("help", &[], "Browse commands and keyboard shortcuts", ""),
    c("quit", &["exit"], "Quit the application", ""),
    c("flush", &[], "Flush conversation memory to disk now", ""),
    c(
        "dream",
        &[],
        "Run memory consolidation (merge session logs into organized topics)",
        "",
    ),
    c(
        "deep-research",
        &[],
        "Research with bounded parallel agents, cross-check evidence, and write a cited report",
        "<query>",
    ),
    c(
        "goal",
        &[],
        "Set, manage, or check an autonomous goal",
        "<objective> [--budget <tokens>] | status | pause | resume | clear",
    ),
];

/// The grok command a typed name or alias refers to.
pub fn lookup(name: &str) -> Option<&'static Cmd> {
    TABLE
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

/// Whether a typed `/name` is a command grokw handles (Grok's, or one wizard advertises).
pub fn is_known(app: &App, name: &str) -> bool {
    lookup(name).is_some() || app.commands.iter().any(|c| c.name == name)
}

pub fn hint_for(app: &App, name: &str) -> Option<String> {
    if let Some(c) = lookup(name) {
        return (!c.hint.is_empty()).then(|| c.hint.to_string());
    }
    app.commands
        .iter()
        .find(|c| c.name == name)
        .and_then(|c| (!c.input_hint.is_empty()).then(|| c.input_hint.clone()))
}

/// Rows a tag rides on, from Grok's own tag map (remote data in the real app; `/memory` is the
/// one the capture shows).
fn tag_of(name: &str) -> Option<&'static str> {
    (name == "memory").then_some("[new]")
}

/// Popup rows for `/query`. Bare `/` lists every command: tagged rows first, then recently used
/// ones, then registry order, then wizard's own commands alphabetically (where Grok lists
/// skills). A query ranks by nucleo score, then the typed name, recency, Grok's own commands and
/// last the name; a row shows the alias that matched when the alias scored best.
pub fn slash_items(app: &App, query: &str) -> Vec<PopupItem> {
    struct Row {
        name: String,
        desc: String,
        tag: Option<String>,
        aliases: Vec<String>,
        /// Grok's own command (true) or one wizard advertises.
        builtin: bool,
    }
    let mut rows: Vec<Row> = TABLE
        .iter()
        .map(|c| Row {
            name: c.name.to_string(),
            desc: c.desc.to_string(),
            tag: tag_of(c.name).map(str::to_string),
            aliases: c.aliases.iter().map(|s| s.to_string()).collect(),
            builtin: true,
        })
        .collect();
    for w in &app.commands {
        if lookup(&w.name).is_some() || rows.iter().any(|r| r.name == w.name) {
            continue;
        }
        rows.push(Row {
            name: w.name.clone(),
            desc: w.description.clone(),
            tag: Some("[wizard]".into()),
            aliases: Vec::new(),
            builtin: false,
        });
    }
    let recency = |name: &str| app.inp.mru_score(name);
    let q = query.trim();
    let mut out: Vec<(usize, String, Vec<u32>)> = Vec::new();
    if q.is_empty() {
        let mut order: Vec<usize> = (0..rows.len()).collect();
        order.sort_by(|&a, &b| {
            let (ra, rb) = (&rows[a], &rows[b]);
            // wizard's commands sit where Grok puts skills: after the commands, by name
            rb.builtin.cmp(&ra.builtin).then_with(|| {
                if ra.builtin {
                    ra.tag
                        .is_none()
                        .cmp(&rb.tag.is_none())
                        .then(recency(&rb.name).cmp(&recency(&ra.name)))
                } else {
                    ra.name.to_lowercase().cmp(&rb.name.to_lowercase())
                }
            })
        });
        out = order
            .into_iter()
            .map(|i| (i, rows[i].name.clone(), Vec::new()))
            .collect();
    } else if !q.contains('/') {
        let pat = crate::ui::composer::fuzz::Query::new(q);
        // best trigger per command: score, then the exact query, then the canonical name
        let mut best: Vec<(usize, u32, String)> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let mut pick: Option<(u32, String)> = None;
            for key in std::iter::once(&r.name).chain(r.aliases.iter()) {
                let Some(sc) = pat.score(key, false) else {
                    continue;
                };
                let better = match &pick {
                    None => true,
                    Some((bs, bk)) => {
                        if sc != *bs {
                            sc > *bs
                        } else if (key == q) != (bk == q) {
                            key == q
                        } else if (*key == r.name) != (*bk == r.name) {
                            *key == r.name
                        } else {
                            key < bk
                        }
                    }
                };
                if better {
                    pick = Some((sc, key.clone()));
                }
            }
            if let Some((sc, key)) = pick {
                best.push((i, sc, key));
            }
        }
        best.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| (b.2 == q).cmp(&(a.2 == q)))
                .then_with(|| recency(&rows[b.0].name).cmp(&recency(&rows[a.0].name)))
                .then_with(|| rows[b.0].builtin.cmp(&rows[a.0].builtin))
                .then_with(|| a.2.cmp(&b.2))
        });
        for (i, _, key) in best {
            let disp = format!("/{key}");
            let hits = pat.indices(&disp, false).unwrap_or_default();
            out.push((i, key, hits));
        }
    }
    out.into_iter()
        .map(|(i, key, hits)| {
            let r = &rows[i];
            let has_hint = hint_for(app, &r.name).is_some();
            PopupItem {
                label: format!("/{key}"),
                tag: r.tag.clone(),
                desc: r.desc.clone(),
                insert: if has_hint {
                    format!("/{key} ")
                } else {
                    format!("/{key}")
                },
                hits: hits.into_iter().map(|h| h as usize).collect(),
                range: None,
                dir: false,
            }
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
    let canon = lookup(cmd).map_or(cmd, |c| c.name);
    match canon {
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
                        tag: None,
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
                            tag: None,
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
                            tag: None,
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
                    tag: None,
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

fn unsupported(app: &mut App, msg: &str) {
    say(app, msg);
}

fn not_built(app: &mut App, name: &str) {
    say(app, &format!("/{name} is not built in grokw yet."));
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

fn stub_modal(app: &mut App, title: &str, lines: &[&str]) {
    app.enter_session();
    app.modal = Some(Modal::Stub {
        title: title.to_string(),
        lines: lines.iter().map(|s| s.to_string()).collect(),
    });
    app.dirty = true;
}

/// Run `/name args` (aliases resolved here).
pub fn run(app: &mut App, name: &str, args: &str) {
    let canon = lookup(name).map_or(name, |c| c.name);
    app.dirty = true;
    match canon {
        "new" => app.new_session(),
        "resume" => {
            app.enter_session();
            app.open_resume();
        }
        "quit" => app.quit_now(),
        "home" => app.go_home(),
        "model" => cmd_model(app, args),
        "effort" => cmd_effort(app, args),
        "theme" => cmd_theme(app, args),
        "plan" => cmd_plan(app, args),
        "compact" => {
            if !args.is_empty() {
                say(app, "Compact notes are not supported by wizard. Compacting without the note.");
            }
            forward(app, "/compact".into());
        }
        "btw" => {
            if args.is_empty() {
                say(app, "Usage: /btw <question>");
            } else {
                forward(app, format!("/btw {args}"));
            }
        }
        "recap" => {
            say(app, "The recap is a side question to wizard. It is not saved with the session.");
            forward(app, "/btw Summarize this session so far in a few sentences.".into());
        }
        "context" => crate::ui::dialogs::usage::open(app, 0),
        "session-info" => crate::ui::dialogs::usage::open(app, 2),
        "usage" => crate::ui::dialogs::usage::open(app, 1),
        "tasks" => {
            say(app, "The live task list is not supported by wizard. Showing background tasks as of the last idle moment.");
            forward(app, "/bashes".into());
        }
        "config-agents" => {
            say(app, "Editing agents is not supported by wizard. Showing the subagent list.");
            forward(app, "/agents".into());
        }
        "memory" => match args {
            "on" | "off" => say(app, "Turning memory on or off is not supported by wizard. Use /memory to list, read or forget."),
            _ => forward(app, format!("/memory {args}").trim_end().to_string()),
        },
        "remember" => {
            if args.is_empty() {
                say(app, "Usage: /remember <memory note text>");
            } else {
                say(app, "Wizard saves memories through the agent. Your note was sent as a request to remember it.");
                let p = format!("Please save this to memory: {args}");
                if app.busy() {
                    app.queue.push(p);
                } else {
                    app.send_prompt(p);
                }
            }
        }
        "goal" => cmd_goal(app, args),
        "fork" if !args.is_empty() => forward(app, format!("/fork {args}")),
        "fork" => say(
            app,
            "Session branching is not supported by wizard. /fork <task> starts a background task in this session instead.",
        ),
        "rewind" => {
            say(app, "Rewind in wizard also restores files changed after that turn.");
            forward(app, format!("/rewind {args}").trim_end().to_string());
        }
        "view-plan" => cmd_view_plan(app),
        "copy" => cmd_copy(app, args),
        "export" => cmd_export(app, args),
        "rename" => cmd_rename(app, args),
        "queue" => {
            let msg = if app.queue.is_empty() {
                "No prompts are queued.".to_string()
            } else {
                let mut s = format!("Queued ({}):", app.queue.len());
                for (i, q) in app.queue.iter().enumerate() {
                    s.push_str(&format!("\n  #{} {}", i + 1, q.lines().next().unwrap_or("")));
                }
                s
            };
            say(app, &msg);
        }
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
        "help" => crate::ui::dialogs::open_palette(app),
        "settings" => crate::ui::dialogs::settings::open(app),
        "tutorial" => crate::ui::dialogs::tutorial::open(app),
        "docs" => {
            if args.trim() == "web" {
                say(app, "Online docs are not opened from grokw.");
            } else {
                crate::ui::dialogs::docs::open(app, args);
            }
        }
        "release-notes" => stub_modal(app, "Release Notes", &["The release notes are not built yet."]),
        "doctor" if args.trim_start().starts_with("fix") => say(
            app,
            "/doctor fix is not supported by grokw. /doctor lists what to change and where.",
        ),
        "doctor" => crate::ui::dialogs::doctor::run(app),
        "history" => {
            app.enter_session();
            crate::ui::composer::hist::open(app, false);
        }
        "find" => {
            app.enter_session();
            if app.tr.messages.is_empty() {
                app.toast("Nothing to search yet");
            } else {
                app.focus = crate::app::Focus::Scrollback;
                if app.view.selected.is_none() {
                    app.view.select_last();
                }
                app.view.find_open((!args.is_empty()).then_some(args));
            }
        }
        "jump" => cmd_jump(app),
        "edit-prompt" => {
            app.enter_session();
            crate::ui::composer::input::request_editor(app, String::new());
        }
        "timeline" | "transcript" | "minimal" => not_built(app, canon),
        // Grok features wizard cannot back
        "dashboard" => unsupported(app, "The dashboard is not supported by grokw with wizard. Use /resume to switch sessions."),
        "delete" => unsupported(app, "Deleting a session is not supported by wizard. The session stays in /resume."),
        "auto" | "always-approve" => unsupported(
            app,
            "Always-approve is not a setting in wizard. It runs every tool call without asking.",
        ),
        "loop" => unsupported(app, "/loop is not supported by grokw with wizard yet. Nothing was scheduled."),
        "hooks" => unsupported(app, "/hooks is not supported by wizard over ACP."),
        "plugins" => unsupported(app, "/plugins is not supported by wizard over ACP."),
        "marketplace" => unsupported(app, "/marketplace is not supported by wizard."),
        "skills" => unsupported(app, "The skills list is not supported by wizard over ACP."),
        "workflows" | "workflow" | "deep-research" => {
            unsupported(app, &format!("/{canon} is not supported by wizard."))
        }
        "imagine" | "imagine-video" | "flush" | "dream" | "personas" | "logout" | "privacy" | "voice" => {
            unsupported(app, &format!("/{canon} is not supported by wizard."))
        }
        "feedback" => unsupported(app, "/feedback is not supported by wizard."),
        "mcps" => unsupported(
            app,
            "MCP server management is not supported by wizard over ACP. /doctor shows MCP status.",
        ),
        "import-claude" => unsupported(
            app,
            "Importing Claude settings is not supported by wizard over ACP. Run it in the wizard terminal app.",
        ),
        "login" => unsupported(app, "Sign-in is not supported by grokw. Run wizard --login in a terminal."),
        // wizard's own commands go through as typed
        other => {
            let text = if args.is_empty() { format!("/{other}") } else { format!("/{other} {args}") };
            forward(app, text);
        }
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

fn cmd_effort(app: &mut App, args: &str) {
    if args.is_empty() {
        app.ed.set_text("/effort ");
        app.enter_session();
        return;
    }
    if app.config.efforts.iter().any(|e| e == args) {
        app.enter_session();
        app.chosen_effort = Some(args.to_string());
        app.send(Request::SetEffort(args.to_string()));
    } else {
        let list = app.config.efforts.join(", ");
        say(
            app,
            &format!("unknown effort level '{args}'; use one of: {list}"),
        );
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

/// `/jump`: a list of the turns, oldest first. One turn has nowhere to jump to.
fn cmd_jump(app: &mut App) {
    use agent_core::transcript::{Part, Role};
    app.enter_session();
    let turns: Vec<((usize, usize), String)> = app
        .tr
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, m)| {
            let text = match m.parts.first() {
                Some(Part::Text(t)) => t.lines().find(|l| !l.trim().is_empty()).unwrap_or(""),
                _ => "",
            };
            ((i, 0), text.trim().to_string())
        })
        .collect();
    if !app.view.jump_open(turns) {
        app.toast("Nothing to jump to yet");
    }
}

fn cmd_plan(app: &mut App, args: &str) {
    app.enter_session();
    if app.mode == Mode::Plan {
        say(
            app,
            "Already in plan mode. Use /view-plan to view the current plan.",
        );
        return;
    }
    // wizard's /plan is a toggle, so it is sent once, on the way in
    forward(app, "/plan".into());
    app.mode = Mode::Plan;
    app.toast("✓ Plan mode: on");
    if !args.is_empty() {
        app.queue.push(args.to_string());
    }
}

fn cmd_goal(app: &mut App, args: &str) {
    let first = args.split_whitespace().next().unwrap_or("");
    match first {
        "" | "status" => forward(app, "/goal".into()),
        "pause" | "resume" | "clear" => say(
            app,
            "Goal pause, resume, clear and budgets are not supported by wizard.",
        ),
        _ if args.contains("--budget") => say(
            app,
            "Goal pause, resume, clear and budgets are not supported by wizard.",
        ),
        _ => {
            say(app, "Wizard saves the goal but does not start working on it over ACP. Send a message to begin.");
            forward(app, format!("/goal {args}"));
        }
    }
}

fn cmd_view_plan(app: &mut App) {
    let path = app.opts.cwd.join(".wizard").join("plan.md");
    match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => {
            say(app, "The plan is read from .wizard/plan.md in the project. Wizard does not send plans over ACP.");
            app.note(t);
        }
        _ => say(app, "No plan found at .wizard/plan.md."),
    }
}

/// The text of the `n`th latest assistant message (1 is the latest).
fn nth_reply(app: &App, n: usize) -> Option<String> {
    app.tr
        .messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| {
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
        .nth(n.saturating_sub(1))
}

fn cmd_copy(app: &mut App, args: &str) {
    let mut n = 1usize;
    let mut file: Option<String> = None;
    for a in args.split_whitespace() {
        match a.parse::<usize>() {
            Ok(v) => n = v,
            Err(_) => file = Some(a.to_string()),
        }
    }
    if n == 0 {
        say(
            app,
            "Usage: /copy [N] [file] where N is 1 (latest), 2, 3, ...",
        );
        return;
    }
    let Some(text) = nth_reply(app, n) else {
        say(app, "No assistant messages to copy");
        return;
    };
    match file {
        Some(f) => match std::fs::write(&f, &text) {
            Ok(()) => say(app, &format!("Copied to {f}")),
            Err(e) => say(app, &format!("Failed to write file: {e}")),
        },
        None => {
            crate::app::copy_to_clipboard(&text);
            app.toast_for("Copied!", 30);
        }
    }
}

fn transcript_markdown(app: &App) -> String {
    let mut out = String::new();
    for m in &app.tr.messages {
        let text: String = m
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if text.trim().is_empty() {
            continue;
        }
        match m.role {
            Role::User => out.push_str(&format!("## You\n\n{text}\n\n")),
            Role::Assistant => out.push_str(&format!("## Wizard\n\n{text}\n\n")),
            Role::Notice(_) => {}
        }
    }
    out
}

fn cmd_export(app: &mut App, args: &str) {
    let md = transcript_markdown(app);
    if md.is_empty() {
        say(app, "No conversation content to export");
        return;
    }
    if args.is_empty() {
        crate::app::copy_to_clipboard(&md);
        say(app, "Conversation copied to clipboard");
    } else {
        match std::fs::write(args, &md) {
            Ok(()) => say(app, &format!("Conversation exported to {args}")),
            Err(e) => say(app, &format!("Failed to write file: {e}")),
        }
    }
}

fn cmd_rename(app: &mut App, args: &str) {
    if args.is_empty() {
        say(app, "Usage: /rename <new title> | --auto");
        return;
    }
    let id = app.tr.session_id.clone();
    if args == "--auto" {
        app.titles.remove(&id);
        say(app, "Session title reset to auto");
    } else {
        app.titles.insert(id, args.to_string());
        say(app, &format!("Session renamed to \"{args}\""));
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
    fn ui_offers_the_looks() {
        let rows = arg_items(&app(), "ui", "");
        let looks: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(looks, LOOKS);
        assert_eq!(arg_items(&app(), "ui", "op")[0].insert, "/ui opencode");
    }
}
