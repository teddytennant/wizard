//! Keys, paste and the mouse wheel.

use agent_core::Request;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use tuikit::editor::KeyOutcome;

use crate::app::{App, Overlay};
use crate::commands;
use crate::ui::input;

const PAGE: isize = 10;
const WHEEL: isize = 3;

fn max_scroll(app: &App) -> usize {
    app.view.total.saturating_sub(app.view.height)
}

pub fn scroll_by(app: &mut App, delta: isize) {
    let max = max_scroll(app);
    let cur = app.scroll.unwrap_or(max) as isize;
    let next = (cur + delta).clamp(0, max as isize) as usize;
    app.scroll = (next < max).then_some(next);
    app.dirty = true;
}

/// Ctrl+K and Ctrl+J: put the previous or next user prompt at the top of the view.
pub fn prompt_jump(app: &mut App, dir: isize) {
    let max = max_scroll(app);
    // the prompt goes under the pinned copy of the one before it
    let place = |row: usize| {
        row.saturating_sub(usize::from(
            app.view.prompts.first().is_some_and(|&p| p < row),
        ))
    };
    let cur = app.scroll.unwrap_or(max);
    let target = if dir < 0 {
        match app.scroll {
            None => app.view.prompts.last().copied(),
            Some(_) => app
                .view
                .prompts
                .iter()
                .rev()
                .find(|&&r| place(r) < cur)
                .copied(),
        }
    } else {
        app.view.prompts.iter().find(|&&r| place(r) > cur).copied()
    }
    .map(place);
    match target {
        Some(row) if row < max => app.scroll = Some(row),
        Some(_) => app.scroll = None,
        None if dir > 0 => app.scroll = None,
        None => {}
    }
    app.dirty = true;
}

fn bookmark(app: &mut App) {
    match app.bookmark.take() {
        Some(row) => {
            app.scroll = (row < max_scroll(app)).then_some(row);
            app.set_notice("📌 Returned to bookmark");
        }
        None => {
            app.bookmark = Some(app.scroll.unwrap_or_else(|| max_scroll(app)));
            app.set_notice("📌 Bookmark set - press again to return");
        }
    }
}

fn stash(app: &mut App) {
    let text = app.ed.text().to_string();
    match (app.stash.take(), text.is_empty()) {
        (Some(s), true) => {
            app.ed.set_text(&s);
            app.set_notice("📋 Input restored from stash");
        }
        (Some(s), false) => {
            app.ed.set_text(&s);
            app.stash = Some(text);
            app.set_notice("📋 Swapped input with stash");
        }
        (None, false) => {
            app.ed.clear();
            app.stash = Some(text);
            app.set_notice("📋 Input stashed");
        }
        (None, true) => {}
    }
}

fn cycle_effort(app: &mut App, dir: isize) {
    let list = &app.config.efforts;
    if list.is_empty() {
        return;
    }
    let cur = list
        .iter()
        .position(|e| *e == app.config.effort)
        .unwrap_or(0) as isize;
    let next = (cur + dir).clamp(0, list.len() as isize - 1) as usize;
    let effort = list[next].clone();
    app.chosen_effort = Some(effort.clone());
    app.send(Request::SetEffort(effort.clone()));
    app.set_notice(format!("Effort: {effort}"));
}

/// Ctrl+Up: bring queued prompts back into the composer.
fn retrieve_queued(app: &mut App) -> bool {
    if app.queue.is_empty() {
        return false;
    }
    let n = app.queue.len();
    let mut text = std::mem::take(&mut app.queue).join("\n\n");
    if !app.ed.is_empty() {
        text = format!("{}\n\n{text}", app.ed.text());
    }
    app.ed.set_text(&text);
    app.set_notice(format!(
        "Retrieved {n} pending message{} for editing",
        if n == 1 { "" } else { "s" }
    ));
    true
}

fn help_key(app: &mut App, key: KeyEvent) {
    let Some(Overlay::Help { scroll }) = app.overlay.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = None,
        KeyCode::Down | KeyCode::Char('j') => *scroll += 1,
        KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
        KeyCode::PageDown | KeyCode::Char(' ') => *scroll += 20,
        KeyCode::PageUp => *scroll = scroll.saturating_sub(20),
        KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
        KeyCode::End | KeyCode::Char('G') => *scroll = usize::MAX / 2,
        _ => {}
    }
}

fn sessions_key(app: &mut App, key: KeyEvent) {
    let count = app.sessions.len();
    let Some(Overlay::Sessions { sel, .. }) = app.overlay.as_mut() else {
        return;
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = None,
        KeyCode::Char('c') if ctrl => app.overlay = None,
        KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(count.saturating_sub(1)),
        KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
        KeyCode::Enter => {
            let pick = *sel;
            app.overlay = None;
            if let Some(s) = app.sessions.get(pick) {
                let id = s.id.clone();
                app.send(Request::LoadSession(id));
            }
        }
        _ => {}
    }
}

/// After an edit: selections start over, and a bare `/model` opens its list.
fn edited(app: &mut App) {
    app.sugg_sel = 0;
    app.picker_sel = 0;
    app.tab_base = None;
    app.hist_walk = false;
    if matches!(app.ed.text(), "/model" | "/models") && app.ed.cursor() == app.ed.text().len() {
        app.ed.set_text("/model ");
    }
}

fn enter(app: &mut App) {
    let text = app.ed.text().to_string();
    let backslashes = text.chars().rev().take_while(|c| *c == '\\').count();
    if backslashes % 2 == 1 {
        app.ed.backspace();
        app.ed.insert_newline();
        return;
    }
    if text.trim().is_empty() {
        return;
    }
    app.submit();
}

pub fn on_key(app: &mut App, key: KeyEvent) {
    if key.kind == KeyEventKind::Release {
        return;
    }
    app.dirty = true;
    match app.overlay {
        Some(Overlay::Help { .. }) => return help_key(app, key),
        Some(Overlay::Sessions { .. }) => return sessions_key(app, key),
        None => {}
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let plain = !ctrl && !alt;
    if key.code != KeyCode::Tab {
        app.tab_base = None;
    }
    let listing = input::suggestions_active(app).is_some();
    crate::hotkeys::observe(app, &key, listing);

    if let Some(filter) = commands::model_filter(app.ed.text()) {
        let matches = commands::model_matches(app, filter);
        let last = matches.len().saturating_sub(1);
        match key.code {
            KeyCode::Down => return app.picker_sel = (app.picker_sel + 1).min(last),
            KeyCode::Char('j') if ctrl => return app.picker_sel = (app.picker_sel + 1).min(last),
            KeyCode::Up => return app.picker_sel = app.picker_sel.saturating_sub(1),
            KeyCode::Char('k') if ctrl => return app.picker_sel = app.picker_sel.saturating_sub(1),
            KeyCode::PageDown => return app.picker_sel = (app.picker_sel + 5).min(last),
            KeyCode::PageUp => return app.picker_sel = app.picker_sel.saturating_sub(5),
            KeyCode::Enter if plain => {
                app.ed.clear();
                if let Some(&i) = matches.get(app.picker_sel.min(last)) {
                    commands::pick_model(app, i);
                }
                return;
            }
            KeyCode::Esc => return app.ed.clear(),
            _ => {}
        }
    } else if let Some(list) = input::suggestions_active(app) {
        let n = list.len();
        let sel = app.sugg_sel.min(n - 1);
        match key.code {
            KeyCode::Down if plain => return app.sugg_sel = (sel + 1) % n,
            KeyCode::Char('j') if ctrl => return app.sugg_sel = (sel + 1) % n,
            KeyCode::Up if plain => return app.sugg_sel = (sel + n - 1) % n,
            KeyCode::Char('k') if ctrl => return app.sugg_sel = (sel + n - 1) % n,
            KeyCode::Enter if plain && !shift && list[sel].command != app.ed.text() => {
                app.ed.set_text(&list[sel].command);
                return edited(app);
            }
            KeyCode::Tab => return commands::autocomplete(app),
            _ => {}
        }
    }

    match key.code {
        KeyCode::Enter if shift || alt => {
            app.ed.insert_newline();
            edited(app);
        }
        KeyCode::Enter if ctrl => {
            let text = app.ed.text().trim().to_string();
            if app.busy() && !text.is_empty() {
                app.ed.submit();
                app.history.push(text.clone());
                app.tr.push_user(&text);
                app.send(Request::Steer(text));
            } else {
                enter(app);
            }
        }
        KeyCode::Enter => enter(app),
        KeyCode::Esc => {
            if app.busy() {
                app.interrupt();
            } else {
                app.follow_bottom();
                if !app.ed.is_empty() {
                    app.ed.set_text("");
                    app.set_notice("Input cleared - Ctrl+Z to restore");
                }
                edited(app);
            }
        }
        KeyCode::Char('c') if ctrl => {
            if app.busy() {
                app.interrupt();
            } else {
                app.quit_request();
            }
        }
        KeyCode::Char('d') if ctrl => {
            if !app.ed.is_empty() {
                app.ed.delete_forward();
                edited(app);
            } else if app.busy() {
                app.interrupt();
            } else {
                app.quit_request();
            }
        }
        KeyCode::Char('l') if ctrl => {
            commands::run_local(app, "cls", "");
        }
        KeyCode::Char('u') if ctrl => {
            app.ed.set_cursor(app.ed.text().len());
            while !app.ed.is_empty() && app.ed.cursor() > 0 {
                app.ed.kill_to_line_start();
                if app.ed.cursor() > 0 {
                    app.ed.backspace();
                }
            }
            edited(app);
        }
        KeyCode::Char('k') if ctrl => {
            if app.ed.is_empty() {
                prompt_jump(app, -1);
            } else {
                app.ed.kill_to_line_end();
                edited(app);
            }
        }
        KeyCode::Char('j') if ctrl => prompt_jump(app, 1),
        KeyCode::Char('w') | KeyCode::Char('h') if ctrl => {
            app.ed.delete_word_back();
            edited(app);
        }
        KeyCode::Backspace if ctrl || alt => {
            app.ed.delete_word_back();
            edited(app);
        }
        KeyCode::Delete if alt => {
            app.ed.delete_word_back();
            edited(app);
        }
        KeyCode::Char('a') if ctrl => app.ed.move_doc_start(),
        KeyCode::Char('e') if ctrl => app.ed.move_doc_end(),
        KeyCode::Char('b') if ctrl || alt => app.ed.word_left(),
        KeyCode::Char('f') if ctrl || alt => app.ed.word_right(),
        KeyCode::Left if ctrl => app.ed.word_left(),
        KeyCode::Right if ctrl => app.ed.word_right(),
        KeyCode::Char('z') if ctrl => {
            app.ed.undo();
        }
        KeyCode::Char('s') if ctrl => stash(app),
        KeyCode::Char('g') if ctrl => bookmark(app),
        KeyCode::Up if ctrl && !retrieve_queued(app) && app.ed.history_prev_any() => {
            app.hist_walk = true;
        }
        KeyCode::Down if ctrl => {
            app.ed.history_next();
        }
        KeyCode::Char('u') if alt => scroll_by(app, -PAGE),
        KeyCode::Char('d') if alt => scroll_by(app, PAGE),
        KeyCode::PageUp => scroll_by(app, -PAGE),
        KeyCode::PageDown => scroll_by(app, PAGE),
        KeyCode::Left if alt => cycle_effort(app, -1),
        KeyCode::Right if alt => cycle_effort(app, 1),
        KeyCode::Char('x') if alt => app.todos_pinned = !app.todos_pinned,
        KeyCode::Char('r') if alt => app.open_sessions(),
        KeyCode::Up => {
            if app.ed.move_up() == KeyOutcome::Handled {
                return;
            }
            if (app.ed.is_empty() || app.hist_walk) && app.ed.history_prev() {
                app.hist_walk = true;
            } else {
                scroll_by(app, -1);
            }
        }
        KeyCode::Down => {
            if app.ed.move_down() == KeyOutcome::Handled {
                return;
            }
            if app.hist_walk {
                app.ed.history_next();
                if app.ed.is_empty() {
                    app.hist_walk = false;
                }
            } else {
                scroll_by(app, 1);
            }
        }
        KeyCode::Left => app.ed.move_left(),
        KeyCode::Right => app.ed.move_right(),
        KeyCode::Home => app.ed.move_line_start(),
        KeyCode::End => app.ed.move_line_end(),
        KeyCode::Backspace => {
            app.ed.backspace();
            edited(app);
        }
        KeyCode::Delete => {
            app.ed.delete_forward();
            edited(app);
        }
        KeyCode::Tab => commands::autocomplete(app),
        KeyCode::Char(' ') if plain && commands::model_filter(app.ed.text()) == Some("") => {}
        KeyCode::Char(c) if plain => {
            app.ed.insert_char(c);
            edited(app);
        }
        _ => {}
    }
}

/// Short pastes go in as typed; five lines or more stay text too, since wizard takes the
/// prompt as one string either way.
pub fn on_paste(app: &mut App, text: &str) {
    app.ed
        .paste(&text.replace("\r\n", "\n").replace('\r', "\n"));
    edited(app);
    app.dirty = true;
}

pub fn on_mouse(app: &mut App, ev: MouseEvent) {
    let delta = match ev.kind {
        MouseEventKind::ScrollUp => -WHEEL,
        MouseEventKind::ScrollDown => WHEEL,
        _ => return,
    };
    match app.overlay.as_mut() {
        Some(Overlay::Help { scroll }) => {
            *scroll = (*scroll as isize + delta).max(0) as usize;
            app.dirty = true;
        }
        Some(Overlay::Sessions { .. }) => {}
        None => scroll_by(app, delta),
    }
}
