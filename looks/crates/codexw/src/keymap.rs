//! Codex's default key bindings as data (spec C.9.2): what `/keymap` lists and what the keypress
//! inspector names. The bindings are the ones codexw implements; there is no remapping, so this
//! table is read-only and `/keymap` says so when asked to change one.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    Global,
    Chat,
    Composer,
    Editor,
    VimNormal,
    VimOperator,
    VimTextObject,
    Pager,
    List,
    Approval,
}

impl Context {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Chat => "Chat",
            Self::Composer => "Composer",
            Self::Editor => "Editor",
            Self::VimNormal => "Vim normal",
            Self::VimOperator => "Vim operator",
            Self::VimTextObject => "Vim object",
            Self::Pager => "Pager",
            Self::List => "List",
            Self::Approval => "Approval",
        }
    }

    /// The config table name (`tui.keymap.<name>.<action>`).
    pub fn config_name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Chat => "chat",
            Self::Composer => "composer",
            Self::Editor => "editor",
            Self::VimNormal => "vim_normal",
            Self::VimOperator => "vim_operator",
            Self::VimTextObject => "vim_text_object",
            Self::Pager => "pager",
            Self::List => "list",
            Self::Approval => "approval",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Binding {
    pub context: Context,
    pub action: &'static str,
    /// Specs as the config writes them; empty is unbound.
    pub keys: &'static [&'static str],
    pub description: &'static str,
}

const fn b(
    context: Context,
    action: &'static str,
    keys: &'static [&'static str],
    description: &'static str,
) -> Binding {
    Binding {
        context,
        action,
        keys,
        description,
    }
}

use Context::{
    Approval, Chat, Composer, Editor, Global, List, Pager, VimNormal, VimOperator, VimTextObject,
};

pub const BINDINGS: &[Binding] = &[
    b(
        Global,
        "open_transcript",
        &["ctrl-t"],
        "Open the transcript overlay.",
    ),
    b(
        Global,
        "open_external_editor",
        &["ctrl-g"],
        "Open the current draft in an external editor.",
    ),
    b(
        Global,
        "copy",
        &["ctrl-o"],
        "Copy the last agent response to the clipboard.",
    ),
    b(
        Global,
        "clear_terminal",
        &["ctrl-l"],
        "Clear the terminal UI.",
    ),
    b(
        Global,
        "toggle_vim_mode",
        &[],
        "Turn Vim composer mode on or off.",
    ),
    b(
        Global,
        "toggle_raw_output",
        &["alt-r"],
        "Toggle raw scrollback mode.",
    ),
    b(
        Global,
        "toggle_side_conversation",
        &["ctrl-/"],
        "Switch between a side conversation and its parent.",
    ),
    b(
        Chat,
        "interrupt_turn",
        &["esc"],
        "Interrupt the active turn.",
    ),
    b(
        Chat,
        "decrease_reasoning_effort",
        &["alt-,", "shift-down"],
        "Decrease reasoning effort.",
    ),
    b(
        Chat,
        "increase_reasoning_effort",
        &["alt-.", "shift-up"],
        "Increase reasoning effort.",
    ),
    b(
        Chat,
        "edit_queued_message",
        &["alt-up", "shift-left"],
        "Edit the most recently queued message.",
    ),
    b(
        Composer,
        "submit",
        &["enter"],
        "Submit the current composer draft.",
    ),
    b(
        Composer,
        "queue",
        &["tab"],
        "Queue the draft while a task is running.",
    ),
    b(
        Composer,
        "toggle_shortcuts",
        &["?", "shift-?"],
        "Show or hide the composer shortcut overlay.",
    ),
    b(
        Composer,
        "history_search_previous",
        &["ctrl-r"],
        "Open history search or move to the previous match.",
    ),
    b(
        Composer,
        "history_search_next",
        &["ctrl-s"],
        "Move to the next history search match.",
    ),
    b(
        Editor,
        "insert_newline",
        &["ctrl-j", "ctrl-m", "enter", "shift-enter", "alt-enter"],
        "Insert a newline in the draft.",
    ),
    b(
        Editor,
        "move_left",
        &["left", "ctrl-b"],
        "Move the cursor left.",
    ),
    b(
        Editor,
        "move_right",
        &["right", "ctrl-f"],
        "Move the cursor right.",
    ),
    b(Editor, "move_up", &["up", "ctrl-p"], "Move the cursor up."),
    b(
        Editor,
        "move_down",
        &["down", "ctrl-n"],
        "Move the cursor down.",
    ),
    b(
        Editor,
        "move_word_left",
        &["alt-b", "alt-left", "ctrl-left"],
        "Move to the start of the previous word.",
    ),
    b(
        Editor,
        "move_word_right",
        &["alt-f", "alt-right", "ctrl-right"],
        "Move to the end of the next word.",
    ),
    b(
        Editor,
        "move_line_start",
        &["home", "ctrl-a"],
        "Move to the start of the line.",
    ),
    b(
        Editor,
        "move_line_end",
        &["end", "ctrl-e"],
        "Move to the end of the line.",
    ),
    b(
        Editor,
        "delete_backward",
        &["backspace", "shift-backspace", "ctrl-h"],
        "Delete the character before the cursor.",
    ),
    b(
        Editor,
        "delete_forward",
        &["delete", "shift-delete", "ctrl-d"],
        "Delete the character after the cursor.",
    ),
    b(
        Editor,
        "delete_backward_word",
        &[
            "alt-backspace",
            "ctrl-backspace",
            "ctrl-shift-backspace",
            "ctrl-w",
            "ctrl-alt-h",
        ],
        "Delete the word before the cursor.",
    ),
    b(
        Editor,
        "delete_forward_word",
        &["alt-delete", "ctrl-delete", "ctrl-shift-delete", "alt-d"],
        "Delete the word after the cursor.",
    ),
    b(
        Editor,
        "kill_line_start",
        &["ctrl-u"],
        "Delete to the start of the line.",
    ),
    b(Editor, "kill_whole_line", &[], "Delete the whole line."),
    b(
        Editor,
        "kill_line_end",
        &["ctrl-k"],
        "Delete to the end of the line.",
    ),
    b(Editor, "yank", &["ctrl-y"], "Paste the last deleted text."),
    b(
        VimNormal,
        "enter_insert",
        &["i", "insert"],
        "Enter insert mode.",
    ),
    b(
        VimNormal,
        "append_after_cursor",
        &["a"],
        "Append after the cursor.",
    ),
    b(
        VimNormal,
        "append_line_end",
        &["shift-a", "A"],
        "Append at the end of the line.",
    ),
    b(
        VimNormal,
        "insert_line_start",
        &["shift-i", "I"],
        "Insert at the start of the line.",
    ),
    b(VimNormal, "open_line_below", &["o"], "Open a line below."),
    b(
        VimNormal,
        "open_line_above",
        &["shift-o", "O"],
        "Open a line above.",
    ),
    b(VimNormal, "move_left", &["h", "left"], "Move left."),
    b(VimNormal, "move_right", &["l", "right"], "Move right."),
    b(VimNormal, "move_up", &["k", "up"], "Move up."),
    b(VimNormal, "move_down", &["j", "down"], "Move down."),
    b(
        VimNormal,
        "move_word_forward",
        &["w"],
        "Move to the next word.",
    ),
    b(
        VimNormal,
        "move_word_backward",
        &["b"],
        "Move to the previous word.",
    ),
    b(
        VimNormal,
        "move_word_end",
        &["e"],
        "Move to the end of the word.",
    ),
    b(
        VimNormal,
        "move_line_start",
        &["0"],
        "Move to the start of the line.",
    ),
    b(
        VimNormal,
        "move_line_end",
        &["$", "shift-$"],
        "Move to the end of the line.",
    ),
    b(
        VimNormal,
        "delete_char",
        &["x"],
        "Delete the character under the cursor.",
    ),
    b(
        VimNormal,
        "substitute_char",
        &["s"],
        "Replace the character under the cursor.",
    ),
    b(
        VimNormal,
        "delete_to_line_end",
        &["shift-d", "D"],
        "Delete to the end of the line.",
    ),
    b(
        VimNormal,
        "change_to_line_end",
        &["shift-c", "C"],
        "Change to the end of the line.",
    ),
    b(VimNormal, "yank_line", &["shift-y", "Y"], "Yank the line."),
    b(VimNormal, "paste_after", &["p"], "Paste after the cursor."),
    b(
        VimNormal,
        "start_delete_operator",
        &["d"],
        "Start a delete.",
    ),
    b(VimNormal, "start_yank_operator", &["y"], "Start a yank."),
    b(
        VimNormal,
        "start_change_operator",
        &["c"],
        "Start a change.",
    ),
    b(
        VimNormal,
        "cancel_operator",
        &["esc"],
        "Cancel the pending operator.",
    ),
    b(VimOperator, "delete_line", &["d"], "Delete the line."),
    b(VimOperator, "yank_line", &["y"], "Yank the line."),
    b(VimOperator, "motion_left", &["h"], "Operate left."),
    b(VimOperator, "motion_right", &["l"], "Operate right."),
    b(VimOperator, "motion_up", &["k"], "Operate up."),
    b(VimOperator, "motion_down", &["j"], "Operate down."),
    b(
        VimOperator,
        "motion_word_forward",
        &["w"],
        "Operate to the next word.",
    ),
    b(
        VimOperator,
        "motion_word_backward",
        &["b"],
        "Operate to the previous word.",
    ),
    b(
        VimOperator,
        "motion_word_end",
        &["e"],
        "Operate to the end of the word.",
    ),
    b(
        VimOperator,
        "motion_line_start",
        &["0"],
        "Operate to the start of the line.",
    ),
    b(
        VimOperator,
        "motion_line_end",
        &["$", "shift-$"],
        "Operate to the end of the line.",
    ),
    b(
        VimOperator,
        "select_inner_text_object",
        &["i"],
        "Select inside a text object.",
    ),
    b(
        VimOperator,
        "select_around_text_object",
        &["a"],
        "Select around a text object.",
    ),
    b(VimOperator, "cancel", &["esc"], "Cancel the operator."),
    b(VimTextObject, "word", &["w"], "A word."),
    b(VimTextObject, "big_word", &["shift-w", "W"], "A WORD."),
    b(
        VimTextObject,
        "parentheses",
        &["(", ")", "shift-(", "shift-)", "b"],
        "Parentheses.",
    ),
    b(VimTextObject, "brackets", &["[", "]"], "Brackets."),
    b(
        VimTextObject,
        "braces",
        &["{", "}", "shift-{", "shift-}", "shift-b", "B"],
        "Braces.",
    ),
    b(
        VimTextObject,
        "double_quote",
        &["\"", "shift-\""],
        "Double quotes.",
    ),
    b(VimTextObject, "single_quote", &["'"], "Single quotes."),
    b(VimTextObject, "backtick", &["`"], "Backticks."),
    b(VimTextObject, "cancel", &["esc"], "Cancel the text object."),
    b(Pager, "scroll_up", &["up", "k"], "Scroll up one line."),
    b(
        Pager,
        "scroll_down",
        &["down", "j"],
        "Scroll down one line.",
    ),
    b(
        Pager,
        "page_up",
        &["page-up", "shift-space", "ctrl-b"],
        "Scroll up one page.",
    ),
    b(
        Pager,
        "page_down",
        &["page-down", "space", "ctrl-f"],
        "Scroll down one page.",
    ),
    b(Pager, "half_page_up", &["ctrl-u"], "Scroll up half a page."),
    b(
        Pager,
        "half_page_down",
        &["ctrl-d"],
        "Scroll down half a page.",
    ),
    b(Pager, "jump_top", &["home"], "Jump to the top."),
    b(Pager, "jump_bottom", &["end"], "Jump to the bottom."),
    b(Pager, "close", &["q", "ctrl-c"], "Close the pager."),
    b(
        Pager,
        "close_transcript",
        &["ctrl-t"],
        "Close the transcript.",
    ),
    b(
        List,
        "move_up",
        &["up", "ctrl-p", "ctrl-k", "k"],
        "Move up.",
    ),
    b(
        List,
        "move_down",
        &["down", "ctrl-n", "ctrl-j", "j"],
        "Move down.",
    ),
    b(List, "move_left", &["left", "ctrl-h"], "Move left."),
    b(List, "move_right", &["right", "ctrl-l"], "Move right."),
    b(List, "page_up", &["page-up", "ctrl-b"], "Page up."),
    b(List, "page_down", &["page-down", "ctrl-f"], "Page down."),
    b(List, "jump_top", &["home"], "Jump to the top."),
    b(List, "jump_bottom", &["end"], "Jump to the bottom."),
    b(List, "accept", &["enter"], "Accept the highlighted row."),
    b(List, "cancel", &["esc"], "Close the list."),
    b(
        Approval,
        "open_fullscreen",
        &["ctrl-a", "ctrl-shift-a"],
        "Open approval details fullscreen.",
    ),
    b(
        Approval,
        "open_thread",
        &["o"],
        "Open the thread that asked.",
    ),
    b(Approval, "approve", &["y"], "Approve this request."),
    b(
        Approval,
        "approve_for_session",
        &["a"],
        "Approve for the rest of the session.",
    ),
    b(
        Approval,
        "approve_for_prefix",
        &["p"],
        "Approve commands with this prefix from now on.",
    ),
    b(Approval, "deny", &["d"], "Deny this request."),
    b(
        Approval,
        "decline",
        &["esc", "n"],
        "Decline and tell Wizard what to do differently.",
    ),
    b(Approval, "cancel", &["c"], "Cancel the request."),
];

/// `open_transcript` as `Open Transcript`.
pub fn action_label(action: &str) -> String {
    action
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Binding {
    /// The bindings as `/keymap` shows them: `ctrl-t`, `alt-, and shift-down` as `alt-,, shift-down`.
    pub fn summary(&self) -> String {
        if self.keys.is_empty() {
            "unbound".into()
        } else {
            self.keys.join(", ")
        }
    }
}

/// A key as the config spells it: `ctrl-o`, `alt-,`, `shift-down`, `page-up`, `esc`.
pub fn spec_of(key: &KeyEvent) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT)
        && !matches!(key.code, KeyCode::Char(_) | KeyCode::BackTab)
    {
        parts.push("shift");
    }
    let name: String = match key.code {
        KeyCode::Char(' ') => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                parts.push("shift");
            }
            "space".into()
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::SHIFT) && c.is_alphabetic() {
                parts.push("shift");
                c.to_lowercase().collect()
            } else {
                c.to_string()
            }
        }
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => {
            parts.push("shift");
            "tab".into()
        }
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "page-up".into(),
        KeyCode::PageDown => "page-down".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}").to_lowercase(),
    };
    parts.push(&name);
    parts.join("-")
}

/// `ctrl + o`: the spec with `-` between the parts shown as ` + `.
pub fn spaced(spec: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut rest = spec;
    for m in ["ctrl", "alt", "shift"] {
        if let Some(r) = rest.strip_prefix(m).and_then(|r| r.strip_prefix('-')) {
            out.push(m);
            rest = r;
        }
    }
    out.push(rest);
    out.join(" + ")
}

fn mods_text(m: KeyModifiers) -> String {
    let mut p: Vec<&str> = Vec::new();
    if m.contains(KeyModifiers::CONTROL) {
        p.push("ctrl");
    }
    if m.contains(KeyModifiers::ALT) {
        p.push("alt");
    }
    if m.contains(KeyModifiers::SHIFT) {
        p.push("shift");
    }
    if p.is_empty() {
        "none".into()
    } else {
        p.join("+")
    }
}

/// The inspector's description of a key: what was detected, the config spelling, the raw event
/// and the actions it is bound to.
pub fn inspect(key: &KeyEvent) -> Vec<String> {
    let spec = spec_of(key);
    let kind = match key.kind {
        KeyEventKind::Press => "Press",
        KeyEventKind::Repeat => "Repeat",
        KeyEventKind::Release => "Release",
    };
    let mut rows = vec![
        format!("Detected: {}", spaced(&spec)),
        format!("Config key: {spec}"),
        format!(
            "Raw event: code={:?}, modifiers={}, kind={kind}",
            key.code,
            mods_text(key.modifiers)
        ),
        String::new(),
        "Assigned actions:".to_string(),
    ];
    let hits: Vec<&Binding> = BINDINGS
        .iter()
        .filter(|b| b.keys.contains(&spec.as_str()))
        .collect();
    if hits.is_empty() {
        rows.push("  (none)".into());
    }
    for h in hits {
        rows.push(format!(
            "  - {}.{} ({}) - {} [Default]",
            h.context.config_name(),
            h.action,
            action_label(h.action),
            h.description
        ));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn the_table_has_codexs_action_counts() {
        let n = |c: Context| BINDINGS.iter().filter(|b| b.context == c).count();
        assert_eq!(
            n(Global),
            7,
            "eight with fast mode, which wizard does not have"
        );
        assert_eq!(n(Chat), 4);
        assert_eq!(n(Composer), 5);
        assert_eq!(n(Editor), 17);
        assert_eq!(n(VimNormal) + n(VimOperator) + n(VimTextObject), 48);
        assert_eq!(n(Pager) + n(List), 20);
        assert_eq!(n(Approval), 8);
    }

    #[test]
    fn labels_are_title_cased_ids() {
        assert_eq!(action_label("open_transcript"), "Open Transcript");
        assert_eq!(action_label("approve_for_prefix"), "Approve For Prefix");
    }

    #[test]
    fn specs_follow_the_config_grammar() {
        assert_eq!(
            spec_of(&press(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            "ctrl-o"
        );
        assert_eq!(
            spec_of(&press(KeyCode::Char(','), KeyModifiers::ALT)),
            "alt-,"
        );
        assert_eq!(
            spec_of(&press(KeyCode::Down, KeyModifiers::SHIFT)),
            "shift-down"
        );
        assert_eq!(
            spec_of(&press(KeyCode::PageUp, KeyModifiers::NONE)),
            "page-up"
        );
        assert_eq!(
            spec_of(&press(KeyCode::Char(' '), KeyModifiers::SHIFT)),
            "shift-space"
        );
        assert_eq!(
            spec_of(&press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            "shift-tab"
        );
        assert_eq!(
            spec_of(&press(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            "shift-a"
        );
        assert_eq!(spaced("ctrl-alt-v"), "ctrl + alt + v");
    }

    #[test]
    fn the_inspector_matches_the_capture() {
        // reference/codex/120x36/xh-12-keymap-debug-key
        let rows = inspect(&press(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(rows[0], "Detected: ctrl + o");
        assert_eq!(rows[1], "Config key: ctrl-o");
        assert_eq!(
            rows[2],
            "Raw event: code=Char('o'), modifiers=ctrl, kind=Press"
        );
        assert_eq!(rows[4], "Assigned actions:");
        assert_eq!(
            rows[5],
            "  - global.copy (Copy) - Copy the last agent response to the clipboard. [Default]"
        );
    }
}
