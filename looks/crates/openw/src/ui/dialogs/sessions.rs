// OWNER: dialogs
//! `/sessions`: the sessions the backend lists, pinned ones first, then one group per day.
//! Opens at once with what the app already knows and refills when `Event::Sessions` arrives.

use std::collections::HashSet;

use agent_core::{Event, Request, SessionInfo};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::{SelectDialog, LARGE};
use tuikit::select::{SelectItem, SelectState};
use tuikit::width::truncate;
use tuikit::Theme;

use super::clock;
use super::list::{Act, ListDialog, ListEvent};
use super::panel::Prompt;
use super::prefs::Prefs;
use super::{Ctx, Dialog, Effect, Nav, Outcome};

pub struct SessionDialog {
    list: ListDialog<String>,
    sessions: Vec<SessionInfo>,
    prefs: Prefs,
    current: String,
    cwd: String,
    now: i64,
    off: i64,
    to_delete: Option<String>,
    deleted: HashSet<String>,
}

/// What a session is called in the list: a rename wins, then the backend's title.
pub fn display_title(s: &SessionInfo, prefs: &Prefs) -> String {
    let t = prefs
        .titles
        .get(&s.id)
        .map(String::as_str)
        .unwrap_or(&s.title);
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        "New session".into()
    } else {
        t
    }
}

#[allow(clippy::too_many_arguments)]
fn build(
    sessions: &[SessionInfo],
    prefs: &Prefs,
    current: &str,
    cwd: &str,
    now: i64,
    off: i64,
    query: &str,
    to_delete: Option<&str>,
    deleted: &HashSet<String>,
    delete_key: &str,
) -> Vec<SelectItem<String>> {
    let q = query.trim().to_lowercase();
    let mut all: Vec<&SessionInfo> = sessions
        .iter()
        .filter(|s| !deleted.contains(&s.id))
        .filter(|s| q.is_empty() || display_title(s, prefs).to_lowercase().contains(&q))
        .collect();
    all.sort_by_key(|s| std::cmp::Reverse(s.updated));
    let today = clock::date_string(now, off);
    let row = |s: &SessionInfo, group: String| {
        let deleting = to_delete == Some(s.id.as_str());
        let title = if deleting {
            format!("Press {delete_key} again to confirm")
        } else {
            display_title(s, prefs)
        };
        let mut it = SelectItem::new(s.id.clone(), title)
            .group(group)
            .current(s.id == current);
        it = it.danger(deleting);
        if !s.cwd.is_empty() && s.cwd != cwd {
            let base = s
                .cwd
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(&s.cwd);
            it = it.hint(truncate(base, 20));
        }
        if let Some(slot) = prefs.pin_slot(&s.id) {
            it = it.gutter(slot.to_string(), false);
        }
        it
    };
    let mut v = Vec::new();
    for id in &prefs.pinned {
        if let Some(s) = all.iter().find(|s| s.id == *id) {
            v.push(row(s, "Pinned".into()));
        }
    }
    for s in all.iter().filter(|s| !prefs.is_pinned(&s.id)) {
        let day = clock::date_string(s.updated, off);
        let group = if day == today {
            "Today".to_string()
        } else {
            day
        };
        v.push(row(s, group));
    }
    v
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    let mut dlg = SelectDialog::new("Sessions", Vec::new()).width(LARGE);
    dlg.state = SelectState::new(Vec::new()).skip_filter(true);
    let mut d = SessionDialog {
        list: ListDialog::new(dlg).centered(),
        sessions: ctx.sessions.clone(),
        prefs: ctx.prefs.clone(),
        current: ctx.session_id.clone(),
        cwd: ctx.cwd.clone(),
        now: ctx.now,
        off: ctx.utc_offset,
        to_delete: None,
        deleted: HashSet::new(),
    };
    d.rebuild(true);
    Box::new(d)
}

impl SessionDialog {
    fn rebuild(&mut self, select_current: bool) {
        let sel = self
            .list
            .selected_index()
            .map(|i| self.list.items()[i].value.clone());
        let items = build(
            &self.sessions,
            &self.prefs,
            &self.current,
            &self.cwd,
            self.now,
            self.off,
            self.list.query(),
            self.to_delete.as_deref(),
            &self.deleted,
            "ctrl+d",
        );
        self.list.dlg.state.set_items(items);
        // The delete confirmation recolors one row; keep the cursor on it.
        let keep = sel.filter(|_| !select_current);
        match keep {
            Some(id) => {
                self.list.dlg.state.select_where(|v| *v == id);
            }
            None => self.list.dlg.state.select_current(),
        }
        let mut acts = vec![
            Act::new("pin", "pin/unpin", "ctrl+f"),
            Act::new("delete", "delete", "ctrl+d"),
            Act::new("rename", "rename", "ctrl+r"),
        ];
        acts.shrink_to_fit();
        self.list.set_actions(acts);
        if !self.prefs.pinned.is_empty() {
            self.list.hint("switch", "ctrl+x 1-9");
        }
    }

    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Moved | ListEvent::Filtered => {
                let had = self.to_delete.take().is_some();
                if matches!(ev, ListEvent::Filtered) || had {
                    let keep_cursor = matches!(ev, ListEvent::Moved);
                    let sel = self
                        .list
                        .selected_index()
                        .map(|i| self.list.items()[i].value.clone());
                    self.rebuild(false);
                    if keep_cursor {
                        if let Some(id) = sel {
                            self.list.dlg.state.select_where(|v| *v == id);
                        }
                    }
                }
                Outcome::stay()
            }
            ListEvent::Select(i) => {
                let id = self.list.items()[i].value.clone();
                if id == self.current {
                    Outcome::close()
                } else {
                    Outcome::close_with(vec![Effect::Request(Request::LoadSession(id))])
                }
            }
            ListEvent::Action("pin", i) => {
                let id = self.list.items()[i].value.clone();
                self.prefs.toggle_pin(&id);
                self.rebuild(false);
                self.list.dlg.state.select_where(|v| *v == id);
                Outcome::stay().with(Effect::TogglePin(id))
            }
            ListEvent::Action("delete", i) => {
                let id = self.list.items()[i].value.clone();
                if self.to_delete.as_deref() == Some(id.as_str()) {
                    self.to_delete = None;
                    self.deleted.insert(id.clone());
                    self.rebuild(false);
                    Outcome::stay().with(Effect::DeleteSession(id))
                } else {
                    self.to_delete = Some(id.clone());
                    self.rebuild(false);
                    self.list.dlg.state.select_where(|v| *v == id);
                    Outcome::stay()
                }
            }
            ListEvent::Action("rename", i) => {
                let id = self.list.items()[i].value.clone();
                let title = self
                    .sessions
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| display_title(s, &self.prefs))
                    .unwrap_or_default();
                let mut p = Prompt::new("Rename Session", &title, "Enter text", move |t| {
                    Effect::RenameSession {
                        id: id.clone(),
                        title: t,
                    }
                });
                p.editor.move_doc_end();
                Outcome {
                    nav: Nav::Replace(Box::new(p)),
                    effects: Vec::new(),
                }
            }
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for SessionDialog {
    fn title(&self) -> &str {
        "Sessions"
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
    fn on_event(&mut self, ev: &Event) {
        if let Event::Sessions(list) = ev {
            self.sessions = list.clone();
            self.rebuild(self.list.query().is_empty());
        }
    }
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.list.draw(buf, screen, theme)
    }
}

/// Remove a session's file from `dir` (wizard's `sessions` directory). The id is the file name,
/// with or without the `.jsonl` suffix; anything that is not a plain file name is refused.
pub fn delete_session_file(dir: &std::path::Path, id: &str) -> std::io::Result<bool> {
    if id.is_empty() || id.contains(['/', '\\']) || id == "." || id == ".." {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a session id",
        ));
    }
    for name in [format!("{id}.jsonl"), id.to_string()] {
        let p = dir.join(&name);
        if p.is_file() {
            std::fs::remove_file(p)?;
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, title: &str, ago: i64, now: i64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            title: title.into(),
            cwd: "/x".into(),
            updated: now - ago,
        }
    }

    #[test]
    fn groups_pinned_today_and_older() {
        let now = 1_791_200_000;
        let mut prefs = Prefs::default();
        prefs.toggle_pin("b");
        let items = build(
            &[
                s("a", "A", 10, now),
                s("b", "B", 400_000, now),
                s("c", "C", 500_000, now),
            ],
            &prefs,
            "a",
            "/x",
            now,
            0,
            "",
            None,
            &HashSet::new(),
            "ctrl+d",
        );
        let g: Vec<_> = items.iter().map(|i| i.group.clone().unwrap()).collect();
        assert_eq!(g[0], "Pinned");
        assert_eq!(g[1], "Today");
        assert_eq!(g[2], clock::date_string(now - 500_000, 0));
        assert!(items[1].current);
        assert_eq!(items[0].gutter.as_ref().unwrap().text, "1");
    }

    #[test]
    fn search_is_a_substring_of_the_title() {
        let now = 1_791_200_000;
        let items = build(
            &[
                s("a", "Fix the parser", 10, now),
                s("b", "Write docs", 20, now),
            ],
            &Prefs::default(),
            "",
            "/x",
            now,
            0,
            "PARS",
            None,
            &HashSet::new(),
            "ctrl+d",
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "a");
    }

    #[test]
    fn other_directories_show_their_name_in_the_footer() {
        let now = 1_791_200_000;
        let mut x = s("a", "A", 10, now);
        x.cwd = "/work/projperm".into();
        let items = build(
            &[x],
            &Prefs::default(),
            "",
            "/work/proj",
            now,
            0,
            "",
            None,
            &HashSet::new(),
            "ctrl+d",
        );
        assert_eq!(items[0].hint.as_deref(), Some("projperm"));
    }

    #[test]
    fn delete_removes_only_plain_session_files() {
        let dir = std::env::temp_dir().join(format!("openw-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-01-01T00-00-00.jsonl"), "x").unwrap();
        assert!(delete_session_file(&dir, "2026-01-01T00-00-00").unwrap());
        assert!(!delete_session_file(&dir, "2026-01-01T00-00-00").unwrap());
        assert!(delete_session_file(&dir, "../etc").is_err());
        assert!(delete_session_file(&dir, "").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
