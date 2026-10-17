// OWNER: keys (routing order is spec 9.3; the rewind and palette keys are not built)
//! Key and mouse routing. First owner that takes the key wins: a pending confirmation, a modal
//! or picker, the home screen, then the agent screen's global chords, then whichever pane has
//! focus (the composer, or the scrollback). The composer's own editing keys live in
//! `tuikit::editor`.

use std::time::Duration;

use agent_core::Request;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::app::{App, Focus, Mode, Pending, PendingAction, Screen};
use crate::ui::composer::{self, hist, input, PopupKind};
use crate::ui::dialogs::{self, Modal};
use crate::ui::{welcome, Layout};

const CONFIRM: Duration = Duration::from_millis(1000);
const ESC_DOUBLE: Duration = Duration::from_millis(800);

fn ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char(k) if k.eq_ignore_ascii_case(&c))
}

fn plain(key: &KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

pub fn on_key(app: &mut App, key: KeyEvent) {
    if key.kind == KeyEventKind::Release {
        return;
    }
    app.dirty = true;
    // 0. a pending confirmation: the same chord fires it, anything else drops it
    if let Some(p) = app.pending.clone() {
        app.pending = None;
        if (key.code, key.modifiers) == p.chord && app.now() < p.until {
            fire(app, p.action);
            return;
        }
    }
    // 1. modals and pickers
    if app.modal.is_some() {
        modal_key(app, key);
        return;
    }
    if app.screen == Screen::Home && app.home.picker.is_some() {
        dialogs::picker_key(app, key);
        return;
    }
    match app.screen {
        Screen::Home => home_key(app, key),
        Screen::Session => session_key(app, key),
    }
}

fn fire(app: &mut App, a: PendingAction) {
    match a {
        PendingAction::Quit => app.quit_now(),
        PendingAction::NewSession => app.new_session(),
    }
}

fn arm(app: &mut App, action: PendingAction, key: &KeyEvent, label: &str) {
    app.pending = Some(Pending {
        action,
        chord: (key.code, key.modifiers),
        until: app.now() + CONFIRM,
        label: label.to_string(),
    });
}

fn arm_quit(app: &mut App, key: &KeyEvent) {
    let name = match key.code {
        KeyCode::Char(c) => format!("Ctrl+{}", c.to_ascii_lowercase()),
        _ => "Ctrl+q".to_string(),
    };
    arm(
        app,
        PendingAction::Quit,
        key,
        &format!("{name}:press again to quit"),
    );
}

fn modal_key(app: &mut App, key: KeyEvent) {
    match app.modal {
        Some(Modal::Resume(_)) => {
            dialogs::picker_key(app, key);
        }
        Some(Modal::Palette(_)) => dialogs::palette_key(app, key),
        Some(Modal::Shortcuts(_)) => dialogs::shortcuts::key(app, key),
        Some(Modal::Usage(_)) => dialogs::usage::key(app, key),
        Some(Modal::Settings(_)) => dialogs::settings::key(app, key),
        Some(Modal::Docs(_)) => dialogs::docs::key(app, key),
        Some(Modal::Tutorial(_)) => dialogs::tutorial::key(app, key),
        Some(Modal::Stub { .. }) => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) || ctrl(&key, 'c') {
                app.modal = None;
            }
            if ctrl(&key, 'q') {
                arm_quit(app, &key);
            }
        }
        None => {}
    }
}

// ---- home -----------------------------------------------------------------------------------

fn home_key(app: &mut App, key: KeyEvent) {
    if app.home.worktree.is_some() {
        worktree_key(app, key);
        return;
    }
    if ctrl(&key, 'q') || ctrl(&key, 'c') || ctrl(&key, 'd') {
        arm_quit(app, &key);
        return;
    }
    if ctrl(&key, 'r') {
        app.open_resume();
        return;
    }
    if ctrl(&key, 'n') {
        app.new_session();
        return;
    }
    if ctrl(&key, 'w') {
        app.open_worktree();
        return;
    }
    if app.home.unfocused {
        match key.code {
            KeyCode::Down => {
                let n = welcome::MENU.len();
                app.home.menu_sel = Some(app.home.menu_sel.map_or(0, |i| (i + 1) % n));
            }
            KeyCode::Up => {
                let n = welcome::MENU.len();
                app.home.menu_sel = Some(app.home.menu_sel.map_or(n - 1, |i| (i + n - 1) % n));
            }
            KeyCode::Enter => {
                if let Some(i) = app.home.menu_sel {
                    activate_menu(app, i);
                }
            }
            KeyCode::Esc => {}
            // any other key brings the composer back and goes into it
            _ => {
                app.home.unfocused = false;
                app.home.menu_sel = None;
                leave_home(app, key);
            }
        }
        return;
    }
    match key.code {
        KeyCode::Esc => {
            app.home.unfocused = true;
            app.home.menu_sel = None;
        }
        _ => leave_home(app, key),
    }
}

/// The New Worktree dialog: type a name, `Enter` creates, `Esc` leaves.
fn worktree_key(app: &mut App, key: KeyEvent) {
    if ctrl(&key, 'q') || ctrl(&key, 'c') {
        arm_quit(app, &key);
        return;
    }
    match key.code {
        KeyCode::Esc => app.home.worktree = None,
        KeyCode::Enter => app.create_worktree(),
        KeyCode::Backspace => {
            if let Some(d) = app.home.worktree.as_mut() {
                d.label.pop();
            }
        }
        KeyCode::Char('u') if ctrl(&key, 'u') => {
            if let Some(d) = app.home.worktree.as_mut() {
                d.label.clear();
            }
        }
        KeyCode::Char(c) if plain(&key) || key.modifiers == KeyModifiers::SHIFT => {
            if let Some(d) = app.home.worktree.as_mut() {
                d.label.push(c);
            }
        }
        _ => {}
    }
}

/// The first key of a draft takes the user to the agent screen and goes into the composer.
fn leave_home(app: &mut App, key: KeyEvent) {
    app.enter_session();
    session_key(app, key);
}

pub fn activate_menu(app: &mut App, i: usize) {
    match i {
        0 => app.open_worktree(),
        1 => app.open_resume(),
        2 => {
            app.modal = Some(Modal::Stub {
                title: "Release Notes".into(),
                lines: vec!["The release notes are not built yet.".into()],
            })
        }
        _ => app.quit_now(),
    }
}

// ---- agent screen ---------------------------------------------------------------------------

fn session_key(app: &mut App, key: KeyEvent) {
    // chords that work in either pane
    if ctrl(&key, 'q') {
        arm_quit(app, &key);
        return;
    }
    if ctrl(&key, 'c') {
        ctrl_c(app, &key);
        return;
    }
    if app.view.nav.viewer.is_some() {
        viewer_key(app, key);
        return;
    }
    if app.view.nav.jump.is_some() {
        jump_key(app, key);
        return;
    }
    if ctrl(&key, 'r') {
        app.open_resume();
        return;
    }
    if ctrl(&key, 'n')
        && app.inp.hist.is_none()
        && (app.focus == Focus::Scrollback || app.ed.is_empty())
    {
        arm(
            app,
            PendingAction::NewSession,
            &key,
            "Ctrl+n:press again to new",
        );
        return;
    }
    if ctrl(&key, 'x') || (ctrl(&key, '.')) {
        dialogs::shortcuts::open(app);
        return;
    }
    if ctrl(&key, 'p') && app.popup_open_state().is_none() && app.inp.hist.is_none() {
        dialogs::open_palette(app);
        return;
    }
    if key.code == KeyCode::F(2) {
        crate::commands::run(app, "settings", "");
        return;
    }
    if ctrl(&key, 't') {
        toggle_todo(app);
        return;
    }
    if ctrl(&key, 'o') {
        app.toast("Always-approve is not a setting in wizard");
        return;
    }
    if ctrl(&key, 'b') {
        app.toast("Backgrounding a running command is not supported by wizard");
        return;
    }
    // a dropdown or the history panel owns the page keys
    let owned = app.inp.hist.is_some() || app.popup_open_state().is_some();
    match key.code {
        KeyCode::PageUp if !owned => {
            let n = app.view.view_rows.saturating_sub(3).max(1);
            app.view.scroll_up(n);
            if app.focus == Focus::Scrollback {
                app.view.select_in_view(true);
            }
            return;
        }
        KeyCode::PageDown if !owned => {
            let n = app.view.view_rows.saturating_sub(3).max(1);
            app.view.scroll_down(n);
            if app.focus == Focus::Scrollback {
                app.view.select_in_view(false);
            }
            return;
        }
        _ => {}
    }
    match app.focus {
        Focus::Prompt => prompt_key(app, key),
        Focus::Scrollback => scrollback_key(app, key),
        Focus::Todo => todo_key(app, key),
    }
}

/// The block viewer: scroll, copy, quote, close.
fn viewer_key(app: &mut App, key: KeyEvent) {
    let page = app
        .view
        .nav
        .viewer
        .as_ref()
        .map_or(1, |v| v.visible.saturating_sub(1).max(1)) as isize;
    let half = (page / 2).max(1);
    let by = match key.code {
        KeyCode::Esc | KeyCode::Char('q') if plain(&key) => {
            app.viewer_close();
            return;
        }
        KeyCode::Char('f') if ctrl(&key, 'f') => {
            app.viewer_close();
            return;
        }
        KeyCode::Enter => {
            app.viewer_quote();
            return;
        }
        KeyCode::Char('y') if plain(&key) => {
            crate::ui::transcript_nav::copy_selected(app, false);
            return;
        }
        KeyCode::Char('Y') => {
            crate::ui::transcript_nav::copy_selected(app, true);
            return;
        }
        KeyCode::Down | KeyCode::Char('j') if plain(&key) => 1,
        KeyCode::Up | KeyCode::Char('k') if plain(&key) => -1,
        KeyCode::Char('n') | KeyCode::Char('j') if ctrl_or_alt(&key) => 1,
        KeyCode::Char('p') | KeyCode::Char('k') if ctrl_or_alt(&key) => -1,
        KeyCode::Char('d') if ctrl(&key, 'd') => half,
        KeyCode::Char('u') if ctrl(&key, 'u') => -half,
        KeyCode::PageDown => page,
        KeyCode::PageUp => -page,
        KeyCode::Home | KeyCode::Char('g') => isize::MIN / 2,
        KeyCode::End | KeyCode::Char('G') => isize::MAX / 2,
        _ => return,
    };
    if let Some(v) = app.view.nav.viewer.as_mut() {
        v.scroll(by);
    }
}

/// `/jump`: the list owns the keys until `Enter` or `Esc`.
fn jump_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Down | KeyCode::Char('j') if plain(&key) => app.view.jump_move(1),
        KeyCode::Up | KeyCode::Char('k') if plain(&key) => app.view.jump_move(-1),
        KeyCode::Enter => {
            app.view.jump_commit();
            app.focus = Focus::Scrollback;
        }
        KeyCode::Esc => app.view.jump_cancel(),
        _ => {}
    }
}

/// `/find`: type a query, `Enter` accepts it, `Down`/`Up` (or `n`/`N` after accepting) step.
fn find_key(app: &mut App, key: KeyEvent) {
    let composing = app.view.nav.find.as_ref().is_some_and(|f| f.composing);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Esc => app.view.find_close(),
        KeyCode::Down if plain(&key) => app.view.find_step(true),
        KeyCode::Up if plain(&key) => app.view.find_step(false),
        KeyCode::Enter if composing => {
            if app
                .view
                .nav
                .find
                .as_ref()
                .is_some_and(|f| f.query.is_empty())
            {
                app.view.find_close();
            } else if let Some(f) = app.view.nav.find.as_mut() {
                f.composing = false;
                app.view.find_reveal();
            }
        }
        KeyCode::Char('n') if !composing && plain(&key) && !shift => app.view.find_step(true),
        KeyCode::Char('N') if !composing && plain(&key) => app.view.find_step(false),
        KeyCode::Backspace if composing => app.view.find_edit(|q| {
            q.pop();
        }),
        KeyCode::Char('u') if composing && ctrl(&key, 'u') => app.view.find_edit(|q| q.clear()),
        KeyCode::Char('w') if composing && ctrl(&key, 'w') => app.view.find_edit(|q| {
            let t = q.trim_end().len();
            q.truncate(q.trim_end().rfind(' ').map_or(0, |i| i + 1).min(t));
        }),
        KeyCode::Char(c) if composing && plain(&key) => app.view.find_edit(|q| q.push(c)),
        _ => {}
    }
}

fn ctrl_c(app: &mut App, key: &KeyEvent) {
    if app.inp.hist.is_some() {
        hist::key(app, *key);
        return;
    }
    if !app.ed.is_empty() {
        app.ed.clear();
        app.inp.chips.clear();
        app.shell_mode = false;
        return;
    }
    if app.busy() {
        cancel_turn(app);
        return;
    }
    arm_quit(app, key);
}

pub fn cancel_turn(app: &mut App) {
    app.send(Request::Cancel);
    app.queue.clear();
    app.turn.cancelling = true;
}

fn shift_tab(app: &mut App) {
    if app.busy() {
        app.toast("Wait for the turn to finish before switching modes");
        return;
    }
    match app.mode {
        Mode::Normal => {
            app.send(Request::Prompt("/plan".into()));
            app.mode = Mode::Plan;
            app.mode_banner("  Switched to mode: Plan");
        }
        Mode::Plan => {
            app.send(Request::Prompt("/plan".into()));
            app.mode = Mode::Normal;
            app.mode_banner("  Switched to mode: Normal");
        }
    }
}

fn esc(app: &mut App) {
    if app.busy() {
        app.toast("Press Ctrl+c to cancel the turn");
        return;
    }
    let now = app.now();
    if !app.ed.is_empty() {
        if app.esc_armed.is_some_and(|t| now < t) {
            app.esc_armed = None;
            stash(app, false);
        } else {
            app.esc_armed = Some(now + ESC_DOUBLE);
            app.pending = Some(Pending {
                action: PendingAction::Quit,
                chord: (KeyCode::Null, KeyModifiers::NONE),
                until: now + ESC_DOUBLE,
                label: "Esc:press again to clear".into(),
            });
        }
        return;
    }
    if app
        .tr
        .messages
        .iter()
        .any(|m| m.role == agent_core::transcript::Role::User)
    {
        if app.esc_armed.is_some_and(|t| now < t) {
            app.esc_armed = None;
            crate::commands::run(app, "rewind", "");
        } else {
            app.esc_armed = Some(now + ESC_DOUBLE);
        }
    }
}

fn prompt_key(app: &mut App, key: KeyEvent) {
    input::reconcile(app);
    app.inp.prev_cursor = app.ed.cursor();
    // the history panel takes every key while it is open
    if app.inp.hist.is_some() {
        hist::key(app, key);
        sync_popup(app);
        return;
    }
    let popup = composer::popup(app);
    if let Some(p) = &popup {
        if popup_key(app, p, &key) {
            return;
        }
    }
    match key.code {
        KeyCode::Esc => {
            if app.shell_mode && app.ed.is_empty() {
                app.shell_mode = false;
            } else {
                esc(app);
            }
        }
        KeyCode::BackTab => shift_tab(app),
        KeyCode::Tab => {
            // focus the scrollback and select the last entry
            app.focus = Focus::Scrollback;
            app.view_select_last();
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => send_now_chord(app),
        KeyCode::Enter => {
            // on a paste chip Enter inlines it instead of sending
            if let Some((i, _)) = input::chip_at(app) {
                if !app.inp.chips[i].image {
                    input::expand(app, i);
                    sync_popup(app);
                    return;
                }
            }
            let alt = key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::SHIFT);
            let newline = if app.multiline { !alt } else { alt };
            enter(app, newline);
        }
        KeyCode::Char('s')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT) =>
        {
            stash(app, true);
        }
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input::paste_clipboard(app);
        }
        // Up on an empty composer browses the history; with text it moves a row
        KeyCode::Up if app.ed.is_empty() && plain(&key) && !app.shell_mode => {
            hist::open(app, true);
        }
        _ => edit_key(app, key),
    }
    sync_popup(app);
}

/// The editing keys of the composer: what a key does when no popup or panel wants it.
pub fn edit_key(app: &mut App, key: KeyEvent) {
    let old = app.ed.cursor();
    let ctrl_m = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt_m = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('!') if plain(&key) && app.ed.is_empty() && !app.shell_mode => {
            app.shell_mode = true;
        }
        KeyCode::Backspace if app.shell_mode && app.ed.is_empty() => app.shell_mode = false,
        KeyCode::Char('w' | 'u') if ctrl_m && app.shell_mode && app.ed.is_empty() => {
            app.shell_mode = false;
        }
        KeyCode::Backspace if !ctrl_m && !alt_m && input::delete_whole(app, false) => {}
        KeyCode::Delete if !ctrl_m && !alt_m && input::delete_whole(app, true) => {}
        KeyCode::Char('z') if alt_m && !ctrl_m => {
            app.ed.redo();
        }
        _ => {
            use tuikit::editor::KeyOutcome;
            if let KeyOutcome::Ignored = app.ed.apply_key(key) {
                if ctrl(&key, 'd') {
                    arm_quit(app, &key);
                }
            }
        }
    }
    input::snap_cursor(app, old);
}

/// Stash the draft, or bring a stashed one back onto an empty composer. A chord stash returns by
/// itself after the next send; a double-Esc stash only when the chord is pressed.
fn stash(app: &mut App, chord: bool) {
    if !app.ed.is_empty() {
        let mut text = app.ed.text().to_string();
        if app.shell_mode {
            text = format!("! {text}");
        }
        if let Some(old) = app.stash.take() {
            // a second stash replaces the first; the first goes to history
            app.ed.history_push(&old);
        }
        app.stash = Some(text);
        app.inp.stash_chips = std::mem::take(&mut app.inp.chips);
        app.inp.stash_auto = chord;
        app.ed.clear();
        if chord {
            app.shell_mode = false;
        }
    } else if app.stash.is_some() {
        pop_stash(app);
    }
}

fn pop_stash(app: &mut App) {
    let Some(s) = app.stash.take() else { return };
    match s.strip_prefix("! ") {
        Some(cmd) => {
            app.shell_mode = true;
            app.ed.set_text(cmd);
        }
        None => app.ed.set_text(&s),
    }
    app.inp.chips = std::mem::take(&mut app.inp.stash_chips);
    app.inp.stash_auto = false;
}

/// After a send: a chord stash comes back if the composer is idle and empty.
fn auto_restore_stash(app: &mut App) {
    if app.stash.is_some() && app.inp.stash_auto && app.ed.is_empty() && !app.shell_mode {
        pop_stash(app);
    }
}

/// Ctrl+Enter: cancel the turn and send the draft as the next one; with an empty draft, send the
/// top queued row. Nothing to do when idle.
fn send_now_chord(app: &mut App) {
    if !app.busy() {
        return;
    }
    let text = app.ed.text().trim().to_string();
    if text.is_empty() {
        if !app.queue.is_empty() {
            let q = app.queue.remove(0);
            send_now(app, q);
        }
        return;
    }
    let full = input::expand_all(app, &text);
    app.ed.submit();
    app.inp.chips.clear();
    send_now(app, full);
}

/// Keys while a popup is open. True when the popup took the key.
fn popup_key(app: &mut App, p: &composer::Popup, key: &KeyEvent) -> bool {
    let n = p.items.len();
    let c = key.modifiers.contains(KeyModifiers::CONTROL);
    let at = p.kind == PopupKind::At;
    let up = matches!(key.code, KeyCode::Up)
        || (c && matches!(key.code, KeyCode::Char('p')))
        || (at && c && matches!(key.code, KeyCode::Char('k')));
    let down = matches!(key.code, KeyCode::Down)
        || (c && matches!(key.code, KeyCode::Char('n')))
        || (at && c && matches!(key.code, KeyCode::Char('j')));
    // the slash list wraps; the file picker stops at its ends
    if up {
        app.popup_sel = if !at {
            (app.popup_sel + n - 1) % n
        } else {
            app.popup_sel.saturating_sub(1)
        };
    } else if down {
        app.popup_sel = if !at {
            (app.popup_sel + 1) % n
        } else {
            (app.popup_sel + 1).min(n - 1)
        };
    } else if matches!(key.code, KeyCode::PageDown)
        || (at && c && matches!(key.code, KeyCode::Char('d')))
    {
        app.popup_sel = (app.popup_sel + if at { 4 } else { 8 }).min(n - 1);
    } else if matches!(key.code, KeyCode::PageUp)
        || (at && c && matches!(key.code, KeyCode::Char('u')))
    {
        app.popup_sel = app.popup_sel.saturating_sub(if at { 4 } else { 8 });
    } else {
        return popup_accept_key(app, p, key);
    }
    ensure_at_visible(app, p);
    true
}

fn popup_accept_key(app: &mut App, p: &composer::Popup, key: &KeyEvent) -> bool {
    let at = p.kind == PopupKind::At;
    match key.code {
        KeyCode::Tab => {
            accept_popup(app, p, false);
            true
        }
        KeyCode::Enter if plain(key) && !key.modifiers.contains(KeyModifiers::SHIFT) => {
            accept_popup(app, p, true);
            true
        }
        KeyCode::Right if at && plain(key) => {
            accept_at(app, p, AtAccept::Right);
            true
        }
        KeyCode::Char(':') if at && plain(key) => {
            // a file: take it and leave the cursor after a colon for the line range
            let sel = app.popup_sel.min(p.items.len() - 1);
            if p.items[sel].dir {
                return false;
            }
            accept_at(app, p, AtAccept::Colon);
            true
        }
        KeyCode::Char('l') if at && key.modifiers.contains(KeyModifiers::CONTROL) => {
            let sel = app.popup_sel.min(p.items.len() - 1);
            if p.items[sel].dir {
                return false;
            }
            accept_at(app, p, AtAccept::Colon);
            true
        }
        KeyCode::Esc => {
            app.popup_closed = true;
            true
        }
        _ => false,
    }
}

/// Keep the `@` picker's selection inside its eight-row window.
fn ensure_at_visible(app: &mut App, p: &composer::Popup) {
    if p.kind != PopupKind::At {
        return;
    }
    let sel = app.popup_sel.min(p.items.len().saturating_sub(1));
    let vis = p.items.len().min(8);
    let s = &mut app.inp.at_scroll;
    if sel < *s {
        *s = sel;
    } else if sel >= *s + vis {
        *s = sel + 1 - vis;
    }
}

fn enter(app: &mut App, newline: bool) {
    let text = app.ed.text().to_string();
    if newline || text.ends_with('\\') {
        if text.ends_with('\\') && !newline {
            // a trailing backslash is replaced by the newline
            app.ed.backspace();
        }
        app.ed.insert_newline();
        return;
    }
    if text.trim().is_empty() {
        // an empty composer with a queued row and a running turn sends the top row now
        if app.busy() && !app.queue.is_empty() {
            let q = app.queue.remove(0);
            send_now(app, q);
        }
        return;
    }
    // chips stand for their content from here on
    let full = input::expand_all(app, &text);
    if full != text {
        app.ed.set_text(&full);
    }
    app.inp.chips.clear();
    if app.shell_mode {
        // history keeps the `! ` so a recalled command comes back in shell mode
        app.ed.set_text(&format!("! {}", app.ed.text()));
        let cmd = app.ed.submit();
        let cmd = cmd.strip_prefix("! ").unwrap_or(&cmd).to_string();
        app.shell_mode = false;
        run_shell(app, cmd);
        auto_restore_stash(app);
        return;
    }
    app.submit();
    auto_restore_stash(app);
}

fn send_now(app: &mut App, text: String) {
    // wizard takes nothing mid-turn: cancel, then send when the turn has ended
    app.queue.insert(0, text);
    cancel_turn_keep_queue(app);
}

fn cancel_turn_keep_queue(app: &mut App) {
    let q = app.queue.clone();
    cancel_turn(app);
    app.queue = q;
}

/// `!cmd`: run in the session directory off the UI thread. The transcript shows a `Run (user)`
/// block (spec 5.8) that fills in when the command ends.
fn run_shell(app: &mut App, cmd: String) {
    app.enter_session();
    let id = app.begin_shell(&cmd);
    let cwd = app.opts.cwd.clone();
    let tx = app.msg_tx.clone();
    std::thread::spawn(move || {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(cwd)
            .output();
        let (output, ok) = match out {
            Ok(o) => {
                let mut s = String::from_utf8_lossy(&o.stdout).to_string();
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                (s, o.status.success())
            }
            Err(e) => (format!("Could not run the command: {e}"), false),
        };
        let _ = tx.send(crate::app::Msg::Shell {
            id,
            cmd,
            output,
            ok,
        });
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AtAccept {
    Tab,
    Right,
    Colon,
}

fn accept_popup(app: &mut App, p: &composer::Popup, enter: bool) {
    if p.kind == PopupKind::At {
        accept_at(app, p, AtAccept::Tab);
        return;
    }
    let sel = app.popup_sel.min(p.items.len() - 1);
    let item = &p.items[sel];
    let typed = app.ed.text().to_string();
    if enter
        && typed == item.insert.trim_end()
        && p.kind == PopupKind::Slash
        && !item.insert.ends_with(' ')
    {
        // the typed text already is the command: send it
        app.submit();
        return;
    }
    app.ed.set_text(&item.insert);
    let chain = item.insert.ends_with(' ');
    if enter && !chain {
        app.submit();
        return;
    }
    sync_popup(app);
}

/// Replace `range` of the composer text and put the cursor after the replacement.
fn replace_range(app: &mut App, range: std::ops::Range<usize>, with: &str) {
    let t = app.ed.text().to_string();
    let new = format!("{}{}{}", &t[..range.start], with, &t[range.end..]);
    app.ed.set_text(&new);
    app.ed.set_cursor(range.start + with.len());
}

/// Accept the highlighted `@` row. A file becomes `@path ` and a directory `@path` without the
/// space; in directory mode (the query ends in `/`) a directory is entered instead, the row
/// becoming `path/` with the picker still open. Right on a directory fills its path and keeps
/// searching inside it.
fn accept_at(app: &mut App, p: &composer::Popup, how: AtAccept) {
    let sel = app.popup_sel.min(p.items.len() - 1);
    let item = p.items[sel].clone();
    let text = app.ed.text().to_string();
    let Some(ctx) = composer::at::detect(&text, app.ed.cursor()) else {
        return;
    };
    let path = item.insert.clone();
    let pr = ctx.path_range();
    if item.dir && (p.dir_mode || how == AtAccept::Right) {
        let with = if p.dir_mode { format!("{path}/") } else { path };
        replace_range(app, pr, &with);
        app.popup_closed = false;
    } else {
        let tail = match how {
            AtAccept::Colon => ":".to_string(),
            _ if item.dir => String::new(),
            _ => " ".to_string(),
        };
        // `!` stays out of the reference: it only asked for hidden files
        replace_range(app, ctx.range.clone(), &format!("@{path}{tail}"));
    }
    app.popup_sel = 0;
    app.inp.at_scroll = 0;
    app.popup_for = app.ed.text().to_string();
}

/// Reset the popup selection when the text under it changed.
fn sync_popup(app: &mut App) {
    let t = app.ed.text();
    if t != app.popup_for {
        app.popup_for = t.to_string();
        app.popup_sel = 0;
        app.inp.at_scroll = 0;
        app.popup_closed = false;
    }
    if let Some(p) = composer::popup(app) {
        ensure_at_visible(app, &p);
    }
}

/// Ctrl+T: hidden, then shown and focused, then hidden again.
fn toggle_todo(app: &mut App) {
    if app.tr.todos.is_empty() {
        app.toast("No todo items.");
        return;
    }
    app.todo.open = !app.todo.open;
    app.focus = if app.todo.open {
        Focus::Todo
    } else {
        Focus::Prompt
    };
    app.todo.sel = 0;
    app.todo.top = 0;
}

fn todo_key(app: &mut App, key: KeyEvent) {
    let n = crate::ui::footer::todo_visible(app).len();
    match key.code {
        KeyCode::Esc | KeyCode::Tab | KeyCode::Char(' ') => {
            app.focus = Focus::Prompt;
        }
        KeyCode::Char('h') if plain(&key) => {
            app.todo.hide_done = !app.todo.hide_done;
            app.todo.sel = 0;
            app.todo.top = 0;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.todo.sel = (app.todo.sel + 1).min(n.saturating_sub(1))
        }
        KeyCode::Up | KeyCode::Char('k') => app.todo.sel = app.todo.sel.saturating_sub(1),
        _ => return,
    }
    // keep the selected row in the pane
    let rows = ((app.size.1 as f32 * 0.15) as usize)
        .clamp(1, 10)
        .min(n.max(1));
    if app.todo.sel < app.todo.top {
        app.todo.top = app.todo.sel;
    } else if app.todo.sel >= app.todo.top + rows {
        app.todo.top = app.todo.sel + 1 - rows;
    }
}

fn scrollback_key(app: &mut App, key: KeyEvent) {
    if app.view.nav.find.is_some() {
        find_key(app, key);
        return;
    }
    if app.vim_mode && vim_key(app, key) {
        return;
    }
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // a selection made with the mouse is gone once a key is used
    app.view.nav.drag = None;
    match key.code {
        KeyCode::Right if shift && !ctrl_or_alt(&key) => app.view_prompt(true),
        KeyCode::Left if shift && !ctrl_or_alt(&key) => app.view_prompt(false),
        KeyCode::Char('f') if ctrl(&key, 'f') => app.view_open_selected(),
        KeyCode::Tab | KeyCode::Char(' ') | KeyCode::Esc if plain(&key) => {
            if key.code == KeyCode::Esc && app.busy() {
                app.toast("Press Ctrl+c to cancel the turn");
            } else {
                app.focus = Focus::Prompt;
                app.view.selected = None;
            }
        }
        KeyCode::Up if plain(&key) => app.view_select(-1),
        KeyCode::Down if plain(&key) => app.view_select(1),
        KeyCode::Right if !shift => app.view_fold(Some(true)),
        KeyCode::Left if !shift => app.view_fold(Some(false)),
        KeyCode::Enter => app.view_open_selected(),
        KeyCode::Home => app.view.to_top(),
        KeyCode::End => app.view.to_bottom(),
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.view.think_open = !app.view.think_open;
            app.view.folds.retain(|_, _| true);
        }
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.view.scroll_up(1)
        }
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.view.scroll_down(1)
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let n = (app.view.view_rows / 2).max(1);
            app.view.scroll_up(n);
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let n = (app.view.view_rows / 2).max(1);
            app.view.scroll_down(n);
        }
        // in simple mode a letter or `/` goes to the composer
        KeyCode::Char(c) if plain(&key) && (c.is_ascii_alphabetic() || c == '/') => {
            app.focus = Focus::Prompt;
            app.view.selected = None;
            prompt_key(app, key);
        }
        _ => {}
    }
}

fn ctrl_or_alt(key: &KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Vim keys in the scrollback (`/vim-mode`). Returns whether the key was one of them.
fn vim_key(app: &mut App, key: KeyEvent) -> bool {
    if ctrl_or_alt(&key) {
        return false;
    }
    let KeyCode::Char(c) = key.code else {
        return false;
    };
    app.view.nav.drag = None;
    match c {
        'j' => app.view_select(1),
        'k' => app.view_select(-1),
        'J' => app.view_turn(true),
        'K' => app.view_turn(false),
        'L' => app.view_prompt(true),
        'H' => app.view_prompt(false),
        'g' => {
            app.view.to_top();
            app.view.selected = app
                .view
                .doc
                .entries
                .iter()
                .find(|e| e.selectable)
                .map(|e| e.key);
        }
        'G' => {
            app.view.to_bottom();
            app.view.select_last();
        }
        'h' => app.view_fold(Some(false)),
        'l' => app.view_fold(Some(true)),
        'e' => app.view_fold(None),
        'E' => app.view_fold_all(),
        'r' => app.view_raw(),
        'y' => crate::ui::transcript_nav::copy_selected(app, false),
        'Y' => crate::ui::transcript_nav::copy_selected(app, true),
        'i' | ' ' => {
            app.focus = Focus::Prompt;
            app.view.selected = None;
        }
        '/' => app.view.find_open(None),
        _ => return false,
    }
    true
}

pub fn on_paste(app: &mut App, text: &str) {
    app.dirty = true;
    if app.view.nav.find.as_ref().is_some_and(|f| f.composing) {
        let one: String = text.lines().next().unwrap_or("").to_string();
        app.view.find_edit(|q| q.push_str(&one));
        return;
    }
    if app.modal.is_some() {
        return;
    }
    if app.screen == Screen::Home {
        app.home.picker = None;
        app.enter_session();
    }
    if app.focus == Focus::Scrollback {
        app.focus = Focus::Prompt;
    }
    // a paste detaches a browsed history entry like any other edit
    if app.inp.hist.as_ref().is_some_and(|p| p.browse) {
        app.inp.hist = None;
    }
    input::paste(app, text);
    sync_popup(app);
    if app.inp.hist.is_some() {
        hist::refilter(app);
    }
}

pub fn on_mouse(app: &mut App, ev: MouseEvent) {
    let lay = Layout::compute(app);
    if app.screen == Screen::Session && app.modal.is_none() {
        if viewer_mouse(app, &ev) {
            return;
        }
        if composer_mouse(app, &lay, &ev) {
            app.dirty = true;
            return;
        }
        if scrollback_mouse(app, &lay, &ev) {
            app.dirty = true;
            return;
        }
    }
    match ev.kind {
        MouseEventKind::ScrollUp if app.screen == Screen::Session => {
            app.view.scroll_up(3);
            app.dirty = true;
        }
        MouseEventKind::ScrollDown if app.screen == Screen::Session => {
            app.view.scroll_down(3);
            app.dirty = true;
        }
        MouseEventKind::Moved if app.screen == Screen::Home && app.home.picker.is_none() => {
            let hit = welcome::menu_hit(app, &lay, ev.column, ev.row);
            if hit != app.home.hover {
                app.home.hover = hit;
                app.dirty = true;
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if app.screen == Screen::Home && app.home.picker.is_none() && app.modal.is_none() {
                if let Some(i) = welcome::menu_hit(app, &lay, ev.column, ev.row) {
                    activate_menu(app, i);
                    app.dirty = true;
                }
            } else if app.screen == Screen::Session && app.busy() {
                // the [stop] button at the end of the turn row
                if Some(ev.row) == lay.turn_y && ev.column + 8 >= lay.w.saturating_sub(lay.hpad) {
                    cancel_turn(app);
                }
            }
        }
        _ => {}
    }
}

/// The pointer over the composer: the row under it in an open popup, a click that puts the
/// cursor in the text, and a double click that expands a paste chip. True when it took the event.
fn composer_mouse(app: &mut App, lay: &Layout, ev: &MouseEvent) -> bool {
    if app.focus != Focus::Prompt || app.inp.hist.is_some() {
        return false;
    }
    let hit = composer::popup_hit(app, lay, ev.column, ev.row);
    match ev.kind {
        MouseEventKind::Moved => {
            if hit != app.inp.popup_hover {
                app.inp.popup_hover = hit;
                return true;
            }
            return false;
        }
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return false,
    }
    // a click on a popup row selects it and takes it, as Enter would
    if let Some(i) = hit {
        if let Some(p) = composer::popup(app) {
            app.popup_sel = i;
            accept_popup(app, &p, true);
            return true;
        }
    }
    let r = lay.composer;
    let tx = r.x + 4;
    let tw = composer::text_width(r.width);
    let area = ratatui::layout::Rect::new(tx, r.y + 1, tw, r.height.saturating_sub(2));
    if ev.column < tx || ev.column >= tx + tw || ev.row < area.y || ev.row >= area.bottom() {
        return false;
    }
    app.ed.click(area, ev.column, ev.row);
    input::snap_cursor(app, app.ed.cursor());
    // a second click on the same cell inside 500 ms expands the chip under it
    let now = app.now();
    let double = app.inp.last_click.is_some_and(|(t, x, y)| {
        (x, y) == (ev.column, ev.row) && now.saturating_sub(t) < Duration::from_millis(500)
    });
    app.inp.last_click = Some((now, ev.column, ev.row));
    if double {
        if let Some((i, _)) = input::chip_at(app) {
            input::expand(app, i);
        }
        app.inp.last_click = None;
    }
    sync_popup(app);
    true
}

/// Wheel and `[x]` while the block viewer is open. Everything else is swallowed.
fn viewer_mouse(app: &mut App, ev: &MouseEvent) -> bool {
    let Some(v) = app.view.nav.viewer.as_mut() else {
        return false;
    };
    match ev.kind {
        MouseEventKind::ScrollUp => v.scroll(-3),
        MouseEventKind::ScrollDown => v.scroll(3),
        MouseEventKind::Down(MouseButton::Left) => {
            let hit = |r: ratatui::layout::Rect| {
                ev.row == r.y && ev.column >= r.x && ev.column < r.x + r.width
            };
            if v.close.is_some_and(hit) {
                app.viewer_close();
            }
        }
        _ => {}
    }
    app.dirty = true;
    true
}

/// Clicks, drags and the scrollbar over the transcript. Returns whether the event was used.
fn scrollback_mouse(app: &mut App, lay: &Layout, ev: &MouseEvent) -> bool {
    if app.view.nav.jump.is_some() {
        return false;
    }
    let in_view = ev.row >= lay.view.y && ev.row < lay.view.y + lay.view.height;
    match ev.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // the arrow under the viewport brings the end back
            if ev.row == lay.host_y && ev.column == lay.w / 2 && !app.view.following() {
                app.view.to_bottom();
                return true;
            }
            if !in_view {
                return false;
            }
            // the scrollbar jumps to the row it points at
            if ev.column == lay.w.saturating_sub(1) {
                let total = app.view.extent();
                let vh = lay.view.height as usize;
                if total > vh && vh > 0 {
                    let frac = (ev.row - lay.view.y) as f64 / vh as f64;
                    let to = ((total - vh) as f64 * frac).round() as usize;
                    app.view.offset = Some(to.min(app.view.max_offset()));
                    app.view.flip = None;
                }
                return true;
            }
            let Some(r) = app.view.row_at(lay, ev.row) else {
                app.view.nav.drag = None;
                return false;
            };
            app.view.nav.drag = Some(crate::ui::transcript_nav::Drag {
                from: (r, ev.column),
                to: (r, ev.column),
            });
            app.view.nav.pressed = true;
            if let Some((ei, _)) = app.view.doc.locate(r) {
                let e = &app.view.doc.entries[ei];
                if e.selectable {
                    let key = e.key;
                    let again = app.view.nav.click.is_some_and(|(i, t)| {
                        i == ei && app.now().saturating_sub(t) < DOUBLE_CLICK
                    });
                    app.view.selected = Some(key);
                    app.focus = Focus::Scrollback;
                    app.view.nav.click = Some((ei, app.now()));
                    if again {
                        // a double click folds or unfolds the block
                        app.view.fold(None);
                        app.view.nav.click = None;
                        app.view.nav.drag = None;
                        app.view.nav.pressed = false;
                    }
                }
            }
            true
        }
        MouseEventKind::Drag(MouseButton::Left) if app.view.nav.pressed => {
            // near an edge the transcript follows the pointer
            if ev.row < lay.view.y {
                app.view.scroll_up(1);
            } else if ev.row >= lay.view.y + lay.view.height {
                app.view.scroll_down(1);
            }
            let row = ev
                .row
                .clamp(lay.view.y, (lay.view.y + lay.view.height).saturating_sub(1));
            if let (Some(r), Some(d)) = (app.view.row_at(lay, row), app.view.nav.drag.as_mut()) {
                d.to = (r, ev.column);
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) if app.view.nav.pressed => {
            app.view.nav.pressed = false;
            match app.view.nav.drag {
                Some(d) if d.from != d.to => {
                    let text = app.view.selected_text(d);
                    if !text.is_empty() {
                        crate::app::copy_to_clipboard(&text);
                        app.toast_for("Copied!", 30);
                    }
                }
                _ => app.view.nav.drag = None,
            }
            true
        }
        _ => false,
    }
}

const DOUBLE_CLICK: Duration = Duration::from_millis(300);
