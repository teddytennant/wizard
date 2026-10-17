// OWNER: bottom-pane (composer, footer states)
//! The composer band, the popups and the footer under it (spec C.1 to C.5).
//!
//! `handle_key` is the whole input model: shortcut overlay, bang shell mode, Ctrl+R history
//! search, the slash and `@` popups, paste placeholders, submit and queue. It returns what the
//! app has to do; everything else stays here. Ported from Codex's `chat_composer.rs` and its
//! `slash_input`, `history_search` and footer state.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::composer_history::{ComposerHistory, HistoryEntry, SearchDirection, SearchResult};
use super::footer::{
    CollabIndicator, FOOTER_INDENT_COLS, FooterMode, FooterProps, footer_height, render_footer,
};
use super::key_hint::{ctrl, plain, shift};
use super::mention_popup::{FileIndex, MentionPopup, Picked};
use super::paste_burst::{CharDecision, FlushResult, PasteBurst};
use super::skill_popup::SkillPopup;
use super::slash_popup::{SlashItem, SlashPopup, has_command_prefix, wizard_item};
use super::textarea::{TextArea, TextAreaState};
use crate::commands;
use crate::skills::Skill;
use crate::style::palette;

pub const PLACEHOLDERS: [&str; 8] = [
    "Explain this codebase",
    "Summarize recent commits",
    "Implement {feature}",
    "Find and fix a bug in @filename",
    "Write tests for @filename",
    "Improve documentation in @filename",
    "Run /review on my current changes",
    "Use /skills to list available skills",
];

/// More characters than this in one paste become a `[Pasted Content N chars]` element.
pub const LARGE_PASTE_CHAR_THRESHOLD: usize = 1000;
/// The longest message Codex lets through.
pub const MAX_USER_INPUT_TEXT_CHARS: usize = 1 << 20;
/// Columns left of the text: the prompt glyph and one blank.
const LIVE_PREFIX_COLS: u16 = 2;

/// Everything the footer needs from the rest of the app.
#[derive(Clone, Debug)]
pub struct FooterCtx {
    pub model: String,
    /// `default`, `low`, `medium`, `high`, `xhigh`.
    pub effort: String,
    /// Current directory with the home prefix as `~`.
    pub cwd: String,
    pub task_running: bool,
    /// The app primed backtrack: the next Esc opens the transcript, so the hint says `again`.
    pub esc_hint: bool,
    pub plan_mode: bool,
    /// Pre-formatted goal label for the right slot, empty for none.
    pub goal: String,
    /// Side conversation label, empty for none.
    pub side_label: String,
    pub active_agent: String,
    /// Wizard reports no context window, so the status line leaves it out; these only show
    /// when the status line is off.
    pub context_percent: Option<i64>,
    pub context_tokens: Option<i64>,
    /// `/statusline` can turn the configured row off; then the Codex hint row comes back.
    pub status_line_enabled: bool,
    /// The status line's item ids in order, and whether it takes theme colours.
    pub status_items: Vec<String>,
    pub status_colors: bool,
    /// What the items read from.
    pub values: crate::statusline::Values,
    /// Replaces the left side of the footer while set (the external editor is open).
    pub hint_override: Option<String>,
}

impl Default for FooterCtx {
    fn default() -> Self {
        Self {
            model: String::new(),
            effort: String::new(),
            cwd: String::new(),
            task_running: false,
            esc_hint: false,
            plan_mode: false,
            goal: String::new(),
            side_label: String::new(),
            active_agent: String::new(),
            context_percent: None,
            context_tokens: None,
            status_line_enabled: true,
            status_items: crate::statusline::Settings::default().items,
            status_colors: true,
            values: crate::statusline::Values::default(),
            hint_override: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ComposerEvent {
    None,
    /// Enter on a non-empty draft: text with pastes expanded and trimmed.
    Submit(String),
    /// Tab on a non-empty draft while a task runs.
    Queue(String),
    /// A slash command picked from the popup, or typed bare: `/name args`.
    Command(String),
    /// `!cmd` submitted: run `cmd` in the shell now.
    Shell(String),
    /// An info cell the composer wants printed (unknown command); the draft stays.
    Info(String),
    /// An error cell (command disabled while a task runs); the draft stays.
    Error(String),
    /// Ctrl+C with a draft: it was cleared.
    Cleared,
}

/// The draft as the user left it, for restoring after a search preview or a rejected submit.
#[derive(Clone, Debug, Default)]
struct Draft {
    text: String,
    elements: Vec<Range<usize>>,
    pending_pastes: Vec<(String, String)>,
    images: Vec<AttachedImage>,
    bang: bool,
    cursor: usize,
}

/// A local image in the draft: the `[Image #N]` element and the file behind it.
#[derive(Clone, Debug, PartialEq)]
pub struct AttachedImage {
    pub placeholder: String,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchStatus {
    Idle,
    Match,
    NoMatch,
}

#[derive(Clone, Debug)]
struct Search {
    original: Draft,
    query: String,
    status: SearchStatus,
}

#[derive(Debug, Default)]
enum Popup {
    #[default]
    None,
    Command(SlashPopup),
    Mention {
        popup: MentionPopup,
        /// Byte range of the `@token` the popup completes.
        range: Range<usize>,
    },
    /// The `$` skill popup.
    Skill {
        popup: SkillPopup,
        /// Byte range of the `$token` the popup completes.
        range: Range<usize>,
    },
}

pub struct Composer {
    pub editor: TextArea,
    ta_state: TextAreaState,
    /// Width the textarea was last laid out at, for vertical cursor motion.
    last_width: u16,
    pub placeholder: &'static str,
    /// `!` shell mode: the `!` is absorbed into this flag and drawn as the prompt glyph.
    bang: bool,
    pending_pastes: Vec<(String, String)>,
    images: Vec<AttachedImage>,
    /// The images of the message the last submit took, until the app collects them.
    submitted_images: Vec<AttachedImage>,
    popup: Popup,
    dismissed_command_token: Option<String>,
    dismissed_mention: Option<(usize, String)>,
    dismissed_dollar: Option<(usize, String)>,
    skills: Arc<Vec<Skill>>,
    overlay: bool,
    esc_mode: bool,
    task_running: bool,
    history: ComposerHistory,
    search: Option<Search>,
    burst: PasteBurst,
    burst_enabled: bool,
    cwd: PathBuf,
    file_index: Option<Arc<FileIndex>>,
    /// Commands wizard advertised, `(name, description)`: typed, they go to wizard rather than
    /// being unrecognized, and the popup lists the ones codexw has no entry for.
    pub backend_commands: Vec<(String, String)>,
}

impl std::fmt::Debug for Composer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Composer")
            .field("text", &self.editor.text())
            .finish()
    }
}

/// The configured status line (spec C.4.5): the user's items in their theme colours.
pub fn status_line(ctx: &FooterCtx) -> Line<'static> {
    let mut v = ctx.values.clone();
    // The context fields come first; the values fill in only what the app has not set.
    if v.model.is_empty() {
        v.model = ctx.model.clone();
    }
    if v.effort.is_empty() {
        v.effort = ctx.effort.clone();
    }
    if v.cwd.is_empty() {
        v.cwd = ctx.cwd.clone();
    }
    crate::statusline::status_line(&ctx.status_items, ctx.status_colors, &v).unwrap_or_default()
}

/// The text of a pasted-content placeholder for `chars` characters, numbered when the same size
/// is already pending: `[Pasted Content 1500 chars] #2`.
fn next_large_paste_placeholder(pending: &[(String, String)], chars: usize) -> String {
    let base = format!("[Pasted Content {chars} chars]");
    let prefix = format!("{base} #");
    let mut max_suffix = 0usize;
    for (ph, _) in pending {
        if *ph == base {
            max_suffix = max_suffix.max(1);
        } else if let Some(s) = ph
            .strip_prefix(&prefix)
            .and_then(|s| s.parse::<usize>().ok())
        {
            max_suffix = max_suffix.max(s);
        }
    }
    if max_suffix == 0 {
        base
    } else {
        format!("{base} #{}", max_suffix + 1)
    }
}

/// Control characters other than newline and tab are dropped, and CSI escape sequences with them.
pub fn sanitize_user_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.next_if_eq(&'[').is_some() {
            let _ = chars.find(|c| ('@'..='~').contains(c));
        } else if matches!(ch, '\n' | '\t') || !ch.is_control() {
            out.push(ch);
        }
    }
    out
}

/// The `@token` under the cursor: its byte range and the text after the `@`. Tokens inside an
/// element do not count.
fn current_at_token(ta: &TextArea) -> Option<(Range<usize>, String)> {
    let text = ta.text();
    let mut cursor = ta.cursor().min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = &text[..cursor];
    let after = &text[cursor..];
    let start = before
        .char_indices()
        .rfind(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let end = cursor
        + after
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(after.len());
    let token = &text[start..end];
    let query = token.strip_prefix('@')?;
    if ta
        .element_ranges()
        .iter()
        .any(|r| r.start < end && r.end > start)
    {
        return None;
    }
    Some((start..end, query.to_string()))
}

/// How a `$query` reads before the skills are consulted (Codex `dollar_query_kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DollarQuery {
    Completable,
    /// `$1x`, `$-x`: shell syntax unless a loaded skill is spelled that way.
    Ambiguous,
    /// `$HOME`, `$1`, `$_`, a bare name character set that is no name at all.
    Shell,
}

fn dollar_query_kind(query: &str) -> DollarQuery {
    let name_end = query
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':'))
        .count();
    let name = &query[..name_end];
    let upper = name.to_ascii_uppercase();
    let is_env = !name.bytes().any(|b| b.is_ascii_lowercase())
        && matches!(
            upper.as_str(),
            "PATH"
                | "HOME"
                | "USER"
                | "SHELL"
                | "PWD"
                | "TMPDIR"
                | "TEMP"
                | "TMP"
                | "LANG"
                | "TERM"
                | "XDG_CONFIG_HOME"
        );
    let numeric = !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit());
    let first = name.bytes().next();
    if query.is_empty() {
        DollarQuery::Completable
    } else if name_end == 0 || is_env || numeric || matches!(name, "-" | "_") {
        DollarQuery::Shell
    } else if first.is_some_and(|b| b == b'-' || b.is_ascii_digit()) {
        DollarQuery::Ambiguous
    } else {
        DollarQuery::Completable
    }
}

/// The `$token` under the cursor, without its sigil, when it should open the skill popup.
fn current_dollar_token(ta: &TextArea, skills: &[Skill]) -> Option<(Range<usize>, String)> {
    let text = ta.text();
    let mut cursor = ta.cursor().min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let start = text[..cursor]
        .char_indices()
        .rfind(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let end = cursor
        + text[cursor..]
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(text.len() - cursor);
    let query = text[start..end].strip_prefix('$')?;
    if ta
        .element_ranges()
        .iter()
        .any(|r| r.start < end && r.end > start)
    {
        return None;
    }
    let ok = match dollar_query_kind(query) {
        DollarQuery::Completable => true,
        DollarQuery::Ambiguous => skills
            .iter()
            .any(|s| tuikit::fuzzy::score(query, &s.name).is_some()),
        DollarQuery::Shell => false,
    };
    ok.then(|| (start..end, query.to_string()))
}

/// First line of the text and the cursor if it is inside the `/name` token: the name before the
/// cursor and what follows it on the line.
fn command_under_cursor(first_line: &str, cursor: usize) -> Option<(&str, &str)> {
    if !first_line.starts_with('/') {
        return None;
    }
    if cursor > first_line.len() || !first_line.is_char_boundary(cursor) {
        return None;
    }
    let name_start = 1usize;
    let name_end = first_line[name_start..]
        .find(char::is_whitespace)
        .map(|i| name_start + i)
        .unwrap_or(first_line.len());
    let cursor = if cursor <= name_start {
        name_end
    } else {
        cursor
    };
    if cursor > name_end {
        return None;
    }
    Some((&first_line[name_start..cursor], &first_line[cursor..]))
}

/// `/name`, with the typed name up to the cursor, for popup filtering and dismissal.
fn command_popup_filter_text(first_line: &str, cursor: usize) -> Option<String> {
    let (name, _) = command_under_cursor(first_line, cursor)?;
    Some(format!("/{name}"))
}

/// `(name, rest, rest_offset)` for a line starting with `/name`.
fn parse_slash_name(line: &str) -> Option<(&str, &str, usize)> {
    let stripped = line.strip_prefix('/')?;
    let name_end = stripped
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(stripped.len());
    let name = &stripped[..name_end];
    if name.is_empty() {
        return None;
    }
    let rest_untrimmed = &stripped[name_end..];
    let rest = rest_untrimmed.trim_start();
    let rest_start = name_end + (rest_untrimmed.len() - rest.len());
    Some((name, rest, rest_start + 1))
}

/// Commands that take text after their name (spec C.6.2).
fn supports_inline_args(name: &str) -> bool {
    matches!(
        name,
        "review"
            | "rename"
            | "new"
            | "clear"
            | "fork"
            | "plan"
            | "goal"
            | "ide"
            | "keymap"
            | "mcp"
            | "raw"
            | "usage"
            | "pets"
            | "side"
            | "btw"
            | "resume"
    )
}

impl Composer {
    pub fn new(placeholder: &'static str) -> Self {
        Self {
            editor: TextArea::new(),
            ta_state: TextAreaState::default(),
            last_width: 80,
            placeholder,
            bang: false,
            pending_pastes: Vec::new(),
            images: Vec::new(),
            submitted_images: Vec::new(),
            popup: Popup::None,
            dismissed_command_token: None,
            dismissed_mention: None,
            dismissed_dollar: None,
            skills: Arc::new(Vec::new()),
            overlay: false,
            esc_mode: false,
            task_running: false,
            history: ComposerHistory::new(),
            search: None,
            burst: PasteBurst::default(),
            burst_enabled: false,
            cwd: std::env::current_dir().unwrap_or_default(),
            backend_commands: Vec::new(),
            file_index: None,
        }
    }

    // ---- setup ------------------------------------------------------------------------------

    /// Persist submitted prompts under the codexw state directory and load earlier ones.
    pub fn enable_history_persistence(&mut self) {
        self.history.enable_persistence();
    }

    /// Detect pastes that arrive as a stream of key events. Off in the headless harness, where
    /// every key lands at once.
    pub fn set_paste_burst(&mut self, on: bool) {
        self.burst_enabled = on;
    }

    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
        self.file_index = None;
    }

    /// The skills the `$` and `@` popups offer.
    pub fn set_skills(&mut self, skills: Vec<Skill>) {
        self.skills = Arc::new(skills);
    }

    /// Give the `@` popup its files without asking git (tests, and callers that already know).
    pub fn set_file_index(&mut self, index: FileIndex) {
        self.file_index = Some(Arc::new(index));
    }

    fn file_index(&mut self) -> Arc<FileIndex> {
        if self.file_index.is_none() {
            self.file_index = Some(Arc::new(FileIndex::load(&self.cwd)));
        }
        self.file_index.clone().unwrap()
    }

    // ---- reads ------------------------------------------------------------------------------

    /// No draft: nothing typed and not in shell mode.
    pub fn is_empty(&self) -> bool {
        self.editor.is_empty() && !self.bang
    }

    pub fn text(&self) -> &str {
        self.editor.text()
    }

    pub fn is_bang_mode(&self) -> bool {
        self.bang
    }

    /// Ctrl+C belongs to the composer while it has a draft or a history search open.
    pub fn captures_ctrl_c(&self) -> bool {
        self.search.is_some() || !self.is_empty()
    }

    /// Esc belongs to the composer while it closes a popup, a search or shell mode, or leaves
    /// Vim insert mode.
    pub fn captures_escape(&self, key: KeyEvent) -> bool {
        !matches!(self.popup, Popup::None)
            || self.search.is_some()
            || (self.bang && self.editor.is_empty())
            || self.editor.should_handle_vim_insert_escape(key)
            || (self.editor.is_vim_normal_mode() && self.editor.is_vim_operator_pending())
    }

    pub fn popup_active(&self) -> bool {
        !matches!(self.popup, Popup::None)
    }

    pub fn paste_burst_deadline(&self) -> Option<Instant> {
        if self.burst_enabled && self.burst.is_active() {
            Some(Instant::now() + PasteBurst::recommended_flush_delay())
        } else {
            None
        }
    }

    pub fn vim_label(&self) -> Option<&'static str> {
        self.editor.vim_mode_label()
    }

    /// The text the user would submit, pastes expanded.
    pub fn text_with_pending(&self) -> String {
        expand_pending_pastes(
            self.editor.text(),
            &self.editor.element_ranges(),
            &self.pending_pastes,
        )
        .0
    }

    /// Put an `[Image #N]` element in the draft at the cursor and remember the file behind it.
    pub fn attach_image(&mut self, path: PathBuf) {
        let placeholder = format!("[Image #{}]", self.images.len() + 1);
        self.editor.insert_element(&placeholder);
        self.images.push(AttachedImage { placeholder, path });
    }

    /// The images of the message the last submit or queue took, once.
    pub fn take_submitted_images(&mut self) -> Vec<AttachedImage> {
        std::mem::take(&mut self.submitted_images)
    }

    /// Drop attachments whose element was deleted and renumber the rest in draft order, so the
    /// labels never skip (Codex `relabel_local_images`).
    fn reconcile_images(&mut self) {
        if self.images.is_empty() {
            return;
        }
        let text = self.editor.text().to_string();
        let elements = self.editor.element_ranges();
        let mut left = std::mem::take(&mut self.images);
        let mut kept: Vec<AttachedImage> = Vec::new();
        for r in &elements {
            let Some(label) = text.get(r.clone()) else {
                continue;
            };
            if let Some(i) = left.iter().position(|a| a.placeholder == label) {
                kept.push(left.remove(i));
            }
        }
        for (n, img) in kept.iter_mut().enumerate() {
            let want = format!("[Image #{}]", n + 1);
            if img.placeholder != want {
                // Two elements may share a label until the loop gets to the second one.
                self.editor.replace_element_payload(&img.placeholder, &want);
                img.placeholder = want;
            }
        }
        self.images = kept;
    }

    /// The draft as the external editor should see it: pastes expanded, `!` kept.
    pub fn text_for_editor(&self) -> String {
        let t = self.text_with_pending();
        if self.bang { format!("!{t}") } else { t }
    }

    /// Replace the draft with what the external editor returned (trailing whitespace trimmed,
    /// cursor at the end). Pastes were expanded in the seed, so no placeholder survives.
    pub fn apply_external_edit(&mut self, text: &str) {
        self.set_text(text.trim_end());
        self.reset_footer_mode();
    }

    // ---- drafts ---------------------------------------------------------------------------

    fn snapshot_draft(&self) -> Draft {
        Draft {
            text: self.editor.text().to_string(),
            elements: self.editor.element_ranges(),
            pending_pastes: self.pending_pastes.clone(),
            images: self.images.clone(),
            bang: self.bang,
            cursor: self.editor.cursor(),
        }
    }

    fn restore_draft(&mut self, d: Draft) {
        self.editor.set_text_with_elements(&d.text, &d.elements);
        self.editor.set_cursor(d.cursor);
        self.pending_pastes = d.pending_pastes;
        self.images = d.images;
        self.bang = d.bang;
    }

    /// Replace the draft with plain text and put the cursor at its end (edit-last-queued,
    /// backtrack).
    pub fn set_text(&mut self, text: &str) {
        self.search = None;
        self.editor.set_text_clearing_elements(text);
        self.pending_pastes.clear();
        self.images.clear();
        self.bang = false;
        self.editor.set_cursor(text.len());
        self.sync_bash_mode_from_text();
        self.sync_popups();
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    fn apply_history_entry(&mut self, e: HistoryEntry) {
        self.bang = false;
        self.editor.set_text_with_elements(&e.text, &e.elements);
        self.pending_pastes = e.pending_pastes;
        self.images = e
            .images
            .into_iter()
            .map(|(placeholder, path)| AttachedImage { placeholder, path })
            .collect();
        self.editor.set_cursor(e.text.len());
        self.sync_bash_mode_from_text();
    }

    fn sync_bash_mode_from_text(&mut self) {
        if !self.bang && self.editor.text().starts_with('!') {
            self.editor.replace_range(0..1, "");
            self.bang = true;
        }
    }

    /// Insert text at the cursor as if typed, opening whatever popup it completes.
    pub fn insert_text(&mut self, text: &str) {
        self.editor.insert_str(text);
        self.sync_bash_mode_from_text();
        self.sync_popups();
    }

    /// `/mention`: put an `@` in the draft so the file popup opens.
    pub fn insert_at_mention(&mut self) {
        self.editor.insert_str("@");
        self.sync_popups();
    }

    // ---- vim --------------------------------------------------------------------------------

    pub fn set_vim_enabled(&mut self, on: bool) {
        self.editor.set_vim_enabled(on);
    }

    pub fn is_vim_enabled(&self) -> bool {
        self.editor.is_vim_enabled()
    }

    // ---- footer state -----------------------------------------------------------------------

    /// Idle Esc on an empty composer: the footer says how to edit the previous message.
    pub fn on_esc_idle(&mut self) {
        if self.is_empty() && !self.task_running {
            self.esc_mode = true;
        }
    }

    pub fn reset_footer_mode(&mut self) {
        self.overlay = false;
        self.esc_mode = false;
    }

    fn footer_mode(&self, ctx: &FooterCtx) -> FooterMode {
        if self.search.is_some() {
            return FooterMode::HistorySearch;
        }
        if self.overlay {
            return FooterMode::ShortcutOverlay;
        }
        if self.esc_mode && self.is_empty() && !ctx.task_running {
            return FooterMode::EscHint;
        }
        if self.is_empty() {
            FooterMode::ComposerEmpty
        } else {
            FooterMode::ComposerHasDraft
        }
    }

    fn footer_props(&self, ctx: &FooterCtx) -> FooterProps {
        let mode = self.footer_mode(ctx);
        FooterProps {
            mode,
            esc_backtrack_hint: ctx.esc_hint,
            is_task_running: ctx.task_running,
            paste_burst_active: self.burst_enabled && self.burst.is_active(),
            use_shift_enter_hint: false,
            collaboration_modes_enabled: true,
            status_line_enabled: ctx.status_line_enabled,
            status_line: ctx.status_line_enabled.then(|| status_line(ctx)),
            active_agent_label: (!ctx.active_agent.is_empty()).then(|| ctx.active_agent.clone()),
            collab: ctx.plan_mode.then_some(CollabIndicator::Plan),
            goal: (!ctx.goal.is_empty()).then(|| ctx.goal.clone()),
            vim: self.editor.vim_mode_label(),
            shell_mode: self.bang,
            side_label: (!ctx.side_label.is_empty()).then(|| ctx.side_label.clone()),
            context_percent: ctx.context_percent,
            context_tokens: ctx.context_tokens,
            history_search: self.history_search_footer_line(),
            hint_override: ctx.hint_override.clone(),
        }
    }

    // ---- layout -----------------------------------------------------------------------------

    fn popup_height(&self, width: u16) -> Option<u16> {
        match &self.popup {
            Popup::None => None,
            Popup::Command(p) => Some(p.required_height(width)),
            Popup::Mention { popup, .. } => Some(popup.required_height(width)),
            Popup::Skill { popup, .. } => Some(popup.required_height(width)),
        }
    }

    fn text_rows(&self, width: u16) -> u16 {
        self.editor
            .desired_height(width.saturating_sub(LIVE_PREFIX_COLS + 1).max(1))
    }

    pub fn desired_height(&self, width: u16, ctx: &FooterCtx) -> u16 {
        let below = self
            .popup_height(width)
            .unwrap_or_else(|| footer_height(&self.footer_props(ctx)));
        self.text_rows(width) + 2 + below
    }

    /// Split `area` into the band and the slot under it (popup or footer).
    fn layout(&self, area: Rect, ctx: &FooterCtx) -> (Rect, Rect, Rect) {
        let want = self
            .popup_height(area.width)
            .unwrap_or_else(|| footer_height(&self.footer_props(ctx)));
        // The band keeps at least three rows; what the slot cannot have is clipped.
        let slot_h = want.min(area.height.saturating_sub(3));
        let band_h = area.height - slot_h;
        let band = Rect::new(area.x, area.y, area.width, band_h);
        let slot = Rect::new(area.x, area.y + band_h, area.width, slot_h);
        let text = Rect::new(
            area.x + LIVE_PREFIX_COLS,
            area.y + 1,
            area.width.saturating_sub(LIVE_PREFIX_COLS + 1),
            band_h.saturating_sub(2),
        );
        (band, text, slot)
    }

    // ---- drawing ----------------------------------------------------------------------------

    /// Paint the band and the slot under it into `area`; returns the cursor cell.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &FooterCtx) -> Option<Position> {
        if area.height < 2 || area.width < 4 {
            return None;
        }
        self.task_running = ctx.task_running;
        let (band, text_area, slot) = self.layout(area, ctx);
        self.last_width = text_area.width;
        let tint = palette().user_message_style();
        tuikit::paint::set_style(buf, band, tint);

        match &self.popup {
            Popup::Command(p) => p.render(slot, buf),
            Popup::Mention { popup, .. } => popup.render(slot, buf),
            Popup::Skill { popup, .. } => popup.render(slot, buf),
            Popup::None => {
                let props = self.footer_props(ctx);
                render_footer(slot, buf, &props);
            }
        }

        if text_area.is_empty() {
            return None;
        }
        let prompt = if self.bang {
            Span::styled("!", tint.fg(Color::LightRed).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("›", tint.add_modifier(Modifier::BOLD))
        };
        tuikit::paint::put_line(
            buf,
            text_area.x - LIVE_PREFIX_COLS,
            text_area.y,
            &Line::from(prompt),
            band,
        );
        if self.editor.is_empty() && !self.bang {
            let ph = Span::styled(self.placeholder, tint.add_modifier(Modifier::DIM));
            tuikit::paint::put_line(buf, text_area.x, text_area.y, &Line::from(ph), text_area);
        } else {
            let highlights = self.history_search_highlights();
            let hl_style = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
            let hl: Vec<(Range<usize>, Style)> = highlights
                .into_iter()
                .map(|r| (r, tint.patch(hl_style)))
                .collect();
            self.editor
                .render(text_area, buf, &mut self.ta_state, tint, &hl);
        }
        if let Some(pos) = self.history_search_cursor_pos(slot) {
            return Some(pos);
        }
        self.editor
            .cursor_pos_with_state(text_area, self.ta_state)
            .map(|(x, y)| Position::new(x, y))
    }

    // ---- keys -------------------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent, task_running: bool) -> ComposerEvent {
        if key.kind == KeyEventKind::Release {
            return ComposerEvent::None;
        }
        self.task_running = task_running;
        let ev = self.handle_key_inner(key);
        self.reconcile_images();
        self.sync_popups();
        ev
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> ComposerEvent {
        if self.search.is_some() {
            return self.handle_search_key(key);
        }
        if ctrl(KeyCode::Char('r')).is_press(key) && !self.editor.is_vim_operator_pending() {
            self.begin_search();
            return ComposerEvent::None;
        }
        if ctrl(KeyCode::Char('c')).is_press(key) {
            return self.ctrl_c();
        }
        match self.popup {
            Popup::Command(_) => self.handle_command_popup_key(key),
            Popup::Mention { .. } => self.handle_mention_popup_key(key),
            Popup::Skill { .. } => self.handle_skill_popup_key(key),
            Popup::None => self.handle_key_without_popup(key),
        }
    }

    fn ctrl_c(&mut self) -> ComposerEvent {
        if self.is_empty() {
            return ComposerEvent::None;
        }
        let entry = self.history_entry();
        let persist = self.text_with_pending();
        self.history.record(entry, &persist);
        self.clear();
        self.reset_footer_mode();
        ComposerEvent::Cleared
    }

    fn history_entry(&self) -> HistoryEntry {
        let mut text = self.editor.text().to_string();
        let mut elements = self.editor.element_ranges();
        if self.bang {
            text.insert(0, '!');
            for r in &mut elements {
                r.start += 1;
                r.end += 1;
            }
        }
        HistoryEntry {
            text,
            elements,
            pending_pastes: self.pending_pastes.clone(),
            images: self
                .images
                .iter()
                .map(|i| (i.placeholder.clone(), i.path.clone()))
                .collect(),
        }
    }

    fn handle_key_without_popup(&mut self, key: KeyEvent) -> ComposerEvent {
        // `?` on an empty draft shows the shortcut overlay and a second `?` leaves it open.
        let q = plain(KeyCode::Char('?'));
        let sq = shift(KeyCode::Char('?'));
        if (q.is_press(key) || sq.is_press(key)) && self.is_empty() && !self.paste_burst_active() {
            self.overlay = true;
            self.esc_mode = false;
            return ComposerEvent::None;
        }
        if self.bang && key.code == KeyCode::Esc {
            if let Some(p) = self.burst.flush_before_modified_input() {
                self.handle_paste(&p);
            }
            if self.editor.is_empty() {
                self.bang = false;
                return ComposerEvent::None;
            }
        }
        if self.editor.should_handle_vim_insert_escape(key) {
            return self.handle_input_basic(key);
        }
        if self.editor.is_vim_normal_mode() && self.editor.is_vim_operator_pending() {
            return self.handle_input_basic(key);
        }
        let slash = key.code == KeyCode::Char('/') && key.modifiers == KeyModifiers::NONE;
        if self.editor.is_vim_normal_mode() && self.is_empty() && slash {
            self.reset_footer_mode();
            self.editor.set_text_clearing_elements("/");
            self.editor.set_cursor(1);
            self.editor.enter_vim_insert_mode();
            return ComposerEvent::None;
        }
        if self.editor.is_vim_normal_mode()
            && self.is_empty()
            && key.code == KeyCode::Char('!')
            && key.modifiers == KeyModifiers::NONE
        {
            self.reset_footer_mode();
            self.bang = true;
            self.editor.enter_vim_insert_mode();
            return ComposerEvent::None;
        }
        if key.code == KeyCode::Esc {
            if self.is_empty() && !self.task_running {
                self.esc_mode = true;
                return ComposerEvent::None;
            }
        } else {
            self.reset_footer_mode();
        }

        // Tab queues while a task runs; otherwise it submits like Enter, except a shell draft.
        if plain(KeyCode::Tab).is_press(key) && (self.task_running || !self.bang) {
            return self.handle_submission(self.task_running);
        }
        if plain(KeyCode::Enter).is_press(key) {
            return self.handle_submission(false);
        }
        if ctrl(KeyCode::Char('d')).is_press(key) && self.is_empty() {
            return ComposerEvent::None;
        }

        // Up and Down walk history from an empty draft or a recalled entry.
        let (up, down) = if self.editor.is_vim_normal_mode() {
            if self.editor.is_vim_operator_pending() {
                (false, false)
            } else {
                (
                    plain(KeyCode::Char('k')).is_press(key) || plain(KeyCode::Up).is_press(key),
                    plain(KeyCode::Char('j')).is_press(key) || plain(KeyCode::Down).is_press(key),
                )
            }
        } else {
            (
                plain(KeyCode::Up).is_press(key) || ctrl(KeyCode::Char('p')).is_press(key),
                plain(KeyCode::Down).is_press(key) || ctrl(KeyCode::Char('n')).is_press(key),
            )
        };
        if up || down {
            let text = self.history_text();
            if self
                .history
                .should_handle_navigation(&text, self.history_cursor())
            {
                let entry = if up {
                    self.history.navigate_up()
                } else {
                    self.history.navigate_down()
                };
                if let Some(e) = entry {
                    self.apply_history_entry(e);
                    return ComposerEvent::None;
                }
            }
        }
        self.handle_input_basic(key)
    }

    /// The draft as history sees it: a shell draft carries its `!`.
    fn history_text(&self) -> String {
        if self.bang {
            format!("!{}", self.editor.text())
        } else {
            self.editor.text().to_string()
        }
    }

    /// Cursor offset into `history_text`.
    fn history_cursor(&self) -> usize {
        self.editor.cursor() + usize::from(self.bang)
    }

    fn paste_burst_active(&self) -> bool {
        self.burst_enabled && self.burst.is_active()
    }

    // ---- typed input and paste -------------------------------------------------------------

    fn handle_input_basic(&mut self, key: KeyEvent) -> ComposerEvent {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComposerEvent::None;
        }
        let now = Instant::now();
        self.flush_burst_if_due(now);
        if !matches!(key.code, KeyCode::Esc) {
            self.reset_footer_mode();
        }
        if self.burst_enabled {
            if matches!(key.code, KeyCode::Enter)
                && self.burst.is_active()
                && self.burst.append_newline_if_active(now)
            {
                return ComposerEvent::None;
            }
            if let KeyCode::Char(ch) = key.code {
                let ctrl_alt = key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                if !ctrl_alt && self.editor.allows_paste_burst() {
                    if !ch.is_ascii() {
                        return self.handle_non_ascii_char(key, now);
                    }
                    match self.burst.on_plain_char(ch, now) {
                        CharDecision::BufferAppend => {
                            self.burst.append_char_to_buffer(ch, now);
                            return ComposerEvent::None;
                        }
                        CharDecision::BeginBuffer { retro_chars } => {
                            let cur = self.editor.cursor().min(self.editor.text().len());
                            let before = self.editor.text()[..cur].to_string();
                            if let Some(grab) =
                                self.burst
                                    .decide_begin_buffer(now, &before, retro_chars as usize)
                            {
                                if !grab.grabbed.is_empty() {
                                    self.editor.replace_range(grab.start_byte..cur, "");
                                }
                                self.burst.append_char_to_buffer(ch, now);
                                return ComposerEvent::None;
                            }
                        }
                        CharDecision::BeginBufferFromPending => {
                            self.burst.append_char_to_buffer(ch, now);
                            return ComposerEvent::None;
                        }
                        CharDecision::RetainFirstChar => return ComposerEvent::None,
                    }
                }
                if let Some(p) = self.burst.flush_before_modified_input() {
                    self.handle_paste(&p);
                }
            } else if !matches!(key.code, KeyCode::Enter) {
                if let Some(p) = self.burst.flush_before_modified_input() {
                    self.handle_paste(&p);
                }
            }
        }

        let before = if self.pending_pastes.is_empty() {
            None
        } else {
            Some(self.editor.element_payloads())
        };
        if self.bang && key.code == KeyCode::Backspace && self.editor.cursor() == 0 {
            self.bang = false;
            return ComposerEvent::None;
        }
        let width = self.last_width;
        self.editor.input(key, width);
        self.sync_bash_mode_from_text();
        if let Some(before) = before {
            let after = self.editor.element_payloads();
            for removed in before.iter().filter(|p| !after.contains(p)) {
                self.pending_pastes.retain(|(ph, _)| ph != removed);
            }
        }
        if self.burst_enabled {
            match key.code {
                KeyCode::Char(_) => {
                    if key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        self.burst.clear_window_after_non_char();
                    }
                }
                KeyCode::Enter => {}
                _ => self.burst.clear_window_after_non_char(),
            }
        }
        ComposerEvent::None
    }

    fn handle_non_ascii_char(&mut self, key: KeyEvent, now: Instant) -> ComposerEvent {
        let KeyCode::Char(ch) = key.code else {
            return ComposerEvent::None;
        };
        if let Some(d) = self.burst.on_plain_char_no_hold(now) {
            match d {
                CharDecision::BufferAppend => {
                    self.burst.append_char_to_buffer(ch, now);
                    return ComposerEvent::None;
                }
                CharDecision::BeginBuffer { retro_chars } => {
                    let cur = self.editor.cursor().min(self.editor.text().len());
                    let before = self.editor.text()[..cur].to_string();
                    if let Some(grab) =
                        self.burst
                            .decide_begin_buffer(now, &before, retro_chars as usize)
                    {
                        if !grab.grabbed.is_empty() {
                            self.editor.replace_range(grab.start_byte..cur, "");
                        }
                        self.burst.append_char_to_buffer(ch, now);
                        return ComposerEvent::None;
                    }
                }
                _ => {}
            }
        }
        if let Some(p) = self.burst.flush_before_modified_input() {
            self.handle_paste(&p);
        }
        self.editor.insert_str(&ch.to_string());
        self.sync_bash_mode_from_text();
        self.burst.extend_window(now);
        ComposerEvent::None
    }

    fn flush_burst_if_due(&mut self, now: Instant) -> bool {
        match self.burst.flush_if_due(now) {
            FlushResult::Paste(p) => {
                self.handle_paste(&p);
                true
            }
            FlushResult::Typed(ch) => {
                self.editor.insert_str(&ch.to_string());
                self.sync_bash_mode_from_text();
                true
            }
            FlushResult::None => false,
        }
    }

    /// Called from the app's timer: a burst that went quiet is applied as one paste.
    pub fn flush_paste_burst_if_due(&mut self) -> bool {
        if !self.burst_enabled {
            return false;
        }
        let changed = self.flush_burst_if_due(Instant::now());
        if changed {
            self.sync_popups();
        }
        changed
    }

    /// A bracketed paste: normalised, large ones become one element.
    pub fn handle_paste(&mut self, pasted: &str) {
        let pasted = pasted.replace("\r\n", "\n").replace('\r', "\n");
        let pasted = sanitize_user_text(&pasted);
        let chars = pasted.chars().count();
        if chars > LARGE_PASTE_CHAR_THRESHOLD {
            let ph = next_large_paste_placeholder(&self.pending_pastes, chars);
            self.editor.insert_element(&ph);
            self.pending_pastes.push((ph, pasted));
        } else if let Some(path) = (chars > 1)
            .then(|| crate::images::pasted_image(&pasted))
            .flatten()
        {
            self.attach_image(path);
            self.editor.insert_str(" ");
        } else {
            self.editor.insert_str(&pasted);
            self.sync_bash_mode_from_text();
        }
        self.burst.clear_after_explicit_paste();
        self.reset_footer_mode();
        self.sync_popups();
    }

    // ---- submit -----------------------------------------------------------------------------

    fn handle_submission(&mut self, queue: bool) -> ComposerEvent {
        let now = Instant::now();
        let first_line = self.editor.text().lines().next().unwrap_or("");
        let in_slash_context =
            !self.bang && (matches!(self.popup, Popup::Command(_)) || first_line.starts_with('/'));
        if self.burst_enabled && !queue && !in_slash_context {
            if self.burst.is_active() && self.burst.append_newline_if_active(now) {
                return ComposerEvent::None;
            }
            if self.burst.newline_should_insert_instead_of_submit(now) {
                self.editor.insert_str("\n");
                self.burst.extend_window(now);
                return ComposerEvent::None;
            }
        }
        if queue {
            if let Some(p) = self.burst.flush_before_modified_input() {
                self.handle_paste(&p);
            }
        }
        let ev = self.submit_draft(queue);
        if !matches!(ev, ComposerEvent::None) {
            self.editor.enter_vim_normal_mode();
        }
        ev
    }

    /// Dispatch a bare or inline-args slash command, else prepare the draft as a message.
    fn submit_draft(&mut self, queue: bool) -> ComposerEvent {
        let text_now = self.editor.text().to_string();

        // A bare `/name` runs even with the popup closed: Tab leaves `/diff ` behind.
        if !self.bang && !queue {
            if let Some(cmd) = self.slash_command_text(&text_now) {
                return self.dispatch_slash(cmd);
            }
        }

        let (expanded, _) = expand_pending_pastes(
            &text_now,
            &self.editor.element_ranges(),
            &self.pending_pastes,
        );
        let starts_with_space = text_now.starts_with(' ');
        let mut text = expanded.trim().to_string();
        if self.bang {
            // History and the app see the command with its `!`.
            text = format!("!{text}");
            if text == "!" {
                return ComposerEvent::None;
            }
        }
        if text.is_empty() {
            return ComposerEvent::None;
        }
        if !self.bang && !starts_with_space && !queue {
            if let Some((name, _, _)) = parse_slash_name(&text) {
                if !name.contains('/') && !self.knows(name) {
                    return ComposerEvent::Info(format!(
                        "Unrecognized command '/{name}'. Type \"/\" for a list of supported commands."
                    ));
                }
            }
        }
        let chars = text.chars().count();
        if chars > MAX_USER_INPUT_TEXT_CHARS {
            return ComposerEvent::Error(format!(
                "Message exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters ({chars} provided)."
            ));
        }
        let entry = self.history_entry();
        self.history.record(entry, &text);
        self.submitted_images = self
            .images
            .iter()
            .filter(|i| text.contains(&i.placeholder))
            .cloned()
            .collect();
        self.set_text("");
        if queue {
            return ComposerEvent::Queue(text);
        }
        if self.bang_text(&text) {
            return ComposerEvent::Shell(text[1..].trim().to_string());
        }
        ComposerEvent::Submit(text)
    }

    fn bang_text(&self, text: &str) -> bool {
        text.starts_with('!')
    }

    /// A command name codexw has, or one wizard advertised.
    fn knows(&self, name: &str) -> bool {
        commands::lookup(name).is_some() || self.backend_commands.iter().any(|(c, _)| c == name)
    }

    /// wizard's commands codexw has no entry of its own for.
    fn wizard_items(&self) -> Vec<SlashItem> {
        self.backend_commands
            .iter()
            .filter(|(name, _)| commands::lookup(name).is_none())
            .map(|(name, desc)| wizard_item(name, desc))
            .collect()
    }

    /// The draft names, exactly, a wizard command codexw has no entry for (`/mode`), so Enter
    /// runs it instead of the popup's prefix match (`/model`).
    fn typed_backend_only(&self) -> bool {
        let text = self.editor.text();
        let first = text.lines().next().unwrap_or("");
        parse_slash_name(first).is_some_and(|(name, _, _)| {
            commands::lookup(name).is_none() && self.backend_commands.iter().any(|(c, _)| c == name)
        })
    }

    /// The first line as a command to run now: `/name` or `/name args` for a known command.
    fn slash_command_text(&self, text: &str) -> Option<String> {
        if text.starts_with(' ') {
            return None;
        }
        let first_line = text.lines().next().unwrap_or("");
        let (name, _, _) = parse_slash_name(first_line)?;
        if name.contains('/') || !self.knows(name) {
            return None;
        }
        // Text after the name goes along as arguments; the app decides what a command does
        // with it (`/model <id>` picks a model, `/exit now` ignores it).
        let args = self.expanded_rest(text);
        Some(if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        })
    }

    /// Everything after the command name, pastes expanded and trimmed.
    fn expanded_rest(&self, text: &str) -> String {
        let (expanded, _) =
            expand_pending_pastes(text, &self.editor.element_ranges(), &self.pending_pastes);
        match parse_slash_name(&expanded) {
            Some((_, rest, _)) => rest.trim().to_string(),
            None => String::new(),
        }
    }

    fn dispatch_slash(&mut self, cmd_text: String) -> ComposerEvent {
        let name = parse_slash_name(&cmd_text).map(|(n, _, _)| n.to_string());
        if let Some(spec) = name.as_deref().and_then(commands::lookup) {
            if self.task_running && !spec.during_task {
                // The draft stays so the command can be sent once the task ends.
                self.record_slash_history(&cmd_text, spec.name);
                return ComposerEvent::Error(format!(
                    "'/{}' is disabled while a task is in progress.",
                    spec.name
                ));
            }
            self.record_slash_history(&cmd_text, spec.name);
        }
        self.set_text("");
        ComposerEvent::Command(cmd_text)
    }

    fn record_slash_history(&mut self, text: &str, name: &str) {
        // `/clear` is never recalled with Up.
        if name != "clear" {
            self.history.record(HistoryEntry::plain(text), text);
        }
    }

    // ---- slash popup ------------------------------------------------------------------------

    fn handle_command_popup_key(&mut self, key: KeyEvent) -> ComposerEvent {
        if (plain(KeyCode::Char('?')).is_press(key) || shift(KeyCode::Char('?')).is_press(key))
            && self.is_empty()
        {
            self.overlay = true;
            return ComposerEvent::None;
        }
        if key.code == KeyCode::Esc {
            let first_line = self.editor.text().lines().next().unwrap_or("");
            self.dismissed_command_token = command_popup_filter_text(first_line, 0);
            self.popup = Popup::None;
            return ComposerEvent::None;
        }
        self.reset_footer_mode();
        let Popup::Command(popup) = &mut self.popup else {
            return ComposerEvent::None;
        };
        if plain(KeyCode::Up).is_press(key) || ctrl(KeyCode::Char('p')).is_press(key) {
            popup.move_up();
            return ComposerEvent::None;
        }
        if plain(KeyCode::Down).is_press(key) || ctrl(KeyCode::Char('n')).is_press(key) {
            popup.move_down();
            return ComposerEvent::None;
        }
        if plain(KeyCode::Tab).is_press(key) {
            return self.command_popup_tab();
        }
        if key.code == KeyCode::Char('/') && key.modifiers == KeyModifiers::NONE {
            self.command_popup_complete_as_text();
            return ComposerEvent::None;
        }
        if plain(KeyCode::Enter).is_press(key) {
            match popup.selected() {
                Some(sel) if !self.typed_backend_only() => {
                    return self.command_popup_enter(&sel.name);
                }
                _ => return self.handle_key_without_popup(key),
            }
        }
        self.handle_input_basic(key)
    }

    fn sync_popup_filter(&mut self) -> Option<(String, String)> {
        let text = self.editor.text();
        let first_line = text.lines().next().unwrap_or("").to_string();
        let cursor = self.editor.cursor();
        let filter =
            command_popup_filter_text(&first_line, cursor).unwrap_or_else(|| first_line.clone());
        if let Popup::Command(p) = &mut self.popup {
            p.set_filter(filter.trim_start_matches('/'));
        }
        Some((first_line, filter))
    }

    fn command_popup_tab(&mut self) -> ComposerEvent {
        let Some((first_line, _)) = self.sync_popup_filter() else {
            return ComposerEvent::None;
        };
        let Popup::Command(popup) = &self.popup else {
            return ComposerEvent::None;
        };
        if let Some(sel) = popup.selected() {
            if sel.name == "skills" {
                self.record_slash_history("/skills", "skills");
                self.set_text("");
                return ComposerEvent::Command("/skills".into());
            }
            if self.complete_keeping_draft_tail_as_args(&sel.name) {
                return ComposerEvent::None;
            }
            let completed = format!("/{}", sel.name);
            if !first_line.trim_start().starts_with(&completed) {
                self.editor
                    .set_text_clearing_elements(&format!("{completed} "));
                let len = self.editor.text().len();
                self.editor.set_cursor(len);
                return ComposerEvent::None;
            }
        }
        if self.task_running {
            return self.handle_submission(true);
        }
        ComposerEvent::None
    }

    /// `/` while the popup is open accepts the highlighted command as text.
    fn command_popup_complete_as_text(&mut self) {
        let Some((first_line, _)) = self.sync_popup_filter() else {
            return;
        };
        let Popup::Command(popup) = &self.popup else {
            return;
        };
        if let Some(sel) = popup.selected() {
            if self.complete_keeping_draft_tail_as_args(&sel.name) {
                return;
            }
            let completed = format!("/{}", sel.name);
            if !first_line.trim_start().starts_with(&completed) {
                self.editor
                    .set_text_clearing_elements(&format!("{completed} "));
                self.bang = false;
            }
            let len = self.editor.text().len();
            self.editor.set_cursor(len);
        }
    }

    /// `/re` + `view the diff` completes to `/review view the diff` for commands with arguments.
    fn complete_keeping_draft_tail_as_args(&mut self, name: &str) -> bool {
        if !supports_inline_args(name) {
            return false;
        }
        let text = self.editor.text().to_string();
        let first_line_end = text.find('\n').unwrap_or(text.len());
        let cursor = self.editor.cursor();
        if cursor > first_line_end || !text.starts_with('/') || !text.is_char_boundary(cursor) {
            return false;
        }
        let token_end = text[1..first_line_end]
            .find(char::is_whitespace)
            .map(|i| 1 + i)
            .unwrap_or(first_line_end);
        let typed = &text[1..token_end];
        let rest_empty = text[token_end..].trim().is_empty();
        if rest_empty && (cursor <= 1 || cursor >= token_end) {
            return false;
        }
        let replace_end = if cursor <= 1 || (typed == name && rest_empty) {
            token_end
        } else {
            cursor
        };
        let tail = &text[replace_end..];
        let replacement = if tail.chars().next().is_some_and(char::is_whitespace) {
            format!("/{name}")
        } else {
            format!("/{name} ")
        };
        self.editor.replace_range(0..replace_end, &replacement);
        self.bang = false;
        let len = self.editor.text().len();
        self.editor.set_cursor(len);
        true
    }

    fn command_popup_enter(&mut self, name: &str) -> ComposerEvent {
        // Typed text after the name becomes arguments for commands that take them.
        if self.complete_keeping_draft_tail_as_args(name) {
            let text = self.editor.text().to_string();
            if let Some(cmd) = self.slash_command_text(&text) {
                if cmd != format!("/{name}") {
                    return self.dispatch_slash(cmd);
                }
            }
        }
        let cmd = format!("/{name}");
        let spec_name = commands::lookup(name).map_or(name, |s| s.name);
        if let Some(spec) = commands::lookup(name) {
            if self.task_running && !spec.during_task {
                // From the popup the draft is gone; the error says why nothing ran.
                self.record_slash_history(&cmd, spec_name);
                self.set_text("");
                return ComposerEvent::Error(format!(
                    "'/{}' is disabled while a task is in progress.",
                    spec.name
                ));
            }
        }
        self.record_slash_history(&cmd, spec_name);
        self.set_text("");
        ComposerEvent::Command(cmd)
    }

    // ---- @ popup ----------------------------------------------------------------------------

    fn handle_mention_popup_key(&mut self, key: KeyEvent) -> ComposerEvent {
        if key.code == KeyCode::Esc {
            if let Popup::Mention { range, .. } = &self.popup {
                if let Some((r, q)) = current_at_token(&self.editor) {
                    self.dismissed_mention = Some((r.start, q));
                } else {
                    self.dismissed_mention = Some((range.start, String::new()));
                }
            }
            self.popup = Popup::None;
            return ComposerEvent::None;
        }
        self.reset_footer_mode();
        let Popup::Mention { popup, range } = &mut self.popup else {
            return ComposerEvent::None;
        };
        if plain(KeyCode::Up).is_press(key) || ctrl(KeyCode::Char('p')).is_press(key) {
            popup.move_up();
            return ComposerEvent::None;
        }
        if plain(KeyCode::Down).is_press(key) || ctrl(KeyCode::Char('n')).is_press(key) {
            popup.move_down();
            return ComposerEvent::None;
        }
        if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Left {
            popup.previous_search_mode();
            return ComposerEvent::None;
        }
        if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Right {
            popup.next_search_mode();
            return ComposerEvent::None;
        }
        if plain(KeyCode::Tab).is_press(key) || plain(KeyCode::Enter).is_press(key) {
            if let Some(picked) = popup.selected() {
                let range = range.clone();
                match picked {
                    Picked::Path(path) => self.insert_selected_path(range, &path),
                    Picked::Skill(name) => self.insert_skill(range, &name),
                }
                return ComposerEvent::None;
            }
            if plain(KeyCode::Enter).is_press(key) {
                self.popup = Popup::None;
                return self.handle_key_without_popup(key);
            }
            if self.task_running {
                return self.handle_submission(true);
            }
            return ComposerEvent::None;
        }
        self.handle_input_basic(key)
    }

    fn insert_selected_path(&mut self, token: Range<usize>, path: &str) {
        // An image file becomes an attachment, not text.
        let abs = if std::path::Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.cwd.join(path)
        };
        if crate::images::has_image_extension(&abs)
            && crate::images::image_dimensions(&abs).is_some()
        {
            let start = token.start;
            self.editor.replace_range(token, "");
            self.editor.set_cursor(start);
            self.attach_image(abs);
            self.step_past_separator();
            self.popup = Popup::None;
            return;
        }
        let inserted = if path.chars().any(char::is_whitespace) && !path.contains('"') {
            format!("\"{path}\"")
        } else {
            path.to_string()
        };
        let start = token.start;
        self.editor.replace_range(token, &inserted);
        let end = start + inserted.len();
        self.editor.set_cursor(end);
        // One separator space after the path; an existing one is stepped over.
        let text = self.editor.text();
        if text[end..].chars().next().is_some_and(char::is_whitespace) {
            let next = end + text[end..].chars().next().map_or(0, char::len_utf8);
            self.editor.set_cursor(next);
        } else {
            self.editor.insert_str(" ");
        }
        self.dismissed_mention = Some((start, inserted[1.min(inserted.len())..].to_string()));
        self.popup = Popup::None;
    }

    /// Replace the `@` or `$` token with the skill as one atomic `$name` element and step past a
    /// separator space (Codex `insert_selected_mention`).
    fn insert_skill(&mut self, token: Range<usize>, name: &str) {
        let mut start = token.start;
        // Right after another element the two would read as one: put a space between them.
        if self.editor.element_ranges().iter().any(|r| r.end == start) {
            self.editor.replace_range(start..start, " ");
            start += 1;
            self.editor
                .replace_range(start..start + (token.end - token.start), "");
        } else {
            self.editor.replace_range(token, "");
        }
        self.editor.set_cursor(start);
        self.editor.insert_element(&format!("${name}"));
        self.step_past_separator();
        self.popup = Popup::None;
    }

    /// After a completion: step over the space that follows, or add one.
    fn step_past_separator(&mut self) {
        let end = self.editor.cursor();
        let text = self.editor.text();
        if text[end..].chars().next().is_some_and(char::is_whitespace) {
            let next = end + text[end..].chars().next().map_or(0, char::len_utf8);
            self.editor.set_cursor(next);
        } else {
            self.editor.insert_str(" ");
        }
    }

    fn handle_skill_popup_key(&mut self, key: KeyEvent) -> ComposerEvent {
        if key.code == KeyCode::Esc {
            if let Popup::Skill { range, .. } = &self.popup {
                self.dismissed_dollar =
                    Some(match current_dollar_token(&self.editor, &self.skills) {
                        Some((r, q)) => (r.start, q),
                        None => (range.start, String::new()),
                    });
            }
            self.popup = Popup::None;
            return ComposerEvent::None;
        }
        self.reset_footer_mode();
        let Popup::Skill { popup, range } = &mut self.popup else {
            return ComposerEvent::None;
        };
        if plain(KeyCode::Up).is_press(key) || ctrl(KeyCode::Char('p')).is_press(key) {
            popup.move_up();
            return ComposerEvent::None;
        }
        if plain(KeyCode::Down).is_press(key) || ctrl(KeyCode::Char('n')).is_press(key) {
            popup.move_down();
            return ComposerEvent::None;
        }
        if plain(KeyCode::Tab).is_press(key) || plain(KeyCode::Enter).is_press(key) {
            if let Some(skill) = popup.selected() {
                let (range, name) = (range.clone(), skill.name.clone());
                self.insert_skill(range, &name);
                return ComposerEvent::None;
            }
            if plain(KeyCode::Enter).is_press(key) {
                self.popup = Popup::None;
                return self.handle_key_without_popup(key);
            }
            return ComposerEvent::None;
        }
        self.handle_input_basic(key)
    }

    // ---- popup sync -------------------------------------------------------------------------

    fn sync_popups(&mut self) {
        self.sync_slash_command_element();
        if self.search.is_some() {
            self.popup = Popup::None;
            self.dismissed_mention = None;
            return;
        }
        // Browsing history recalls text that must not pop a menu open.
        let text = self.history_text();
        if self
            .history
            .should_handle_navigation(&text, self.history_cursor())
        {
            self.popup = Popup::None;
            return;
        }
        let at = current_at_token(&self.editor);
        let allow_command = !self.bang && at.is_none();
        self.sync_command_popup(allow_command);
        if matches!(self.popup, Popup::Command(_)) {
            self.dismissed_mention = None;
            return;
        }
        match at {
            Some((range, query)) => {
                if self
                    .dismissed_mention
                    .as_ref()
                    .is_some_and(|(s, q)| *s == range.start && *q == query)
                {
                    self.popup = Popup::None;
                    return;
                }
                match &mut self.popup {
                    Popup::Mention { popup, range: r } => {
                        popup.set_query(&query);
                        *r = range;
                    }
                    _ => {
                        let index = self.file_index();
                        self.popup = Popup::Mention {
                            popup: MentionPopup::new(index, self.skills.clone(), &query),
                            range,
                        };
                    }
                }
            }
            None => {
                self.dismissed_mention = None;
                if matches!(self.popup, Popup::Mention { .. }) {
                    self.popup = Popup::None;
                }
                self.sync_skill_popup();
            }
        }
    }

    /// The `$` popup follows a `$token` at the cursor that is not shell syntax.
    fn sync_skill_popup(&mut self) {
        if self.bang || self.skills.is_empty() {
            self.dismissed_dollar = None;
            if matches!(self.popup, Popup::Skill { .. }) {
                self.popup = Popup::None;
            }
            return;
        }
        match current_dollar_token(&self.editor, &self.skills) {
            Some((range, query)) => {
                if self
                    .dismissed_dollar
                    .as_ref()
                    .is_some_and(|(s, q)| *s == range.start && *q == query)
                {
                    self.popup = Popup::None;
                    return;
                }
                match &mut self.popup {
                    Popup::Skill { popup, range: r } => {
                        popup.set_query(&query);
                        *r = range;
                    }
                    _ => {
                        self.popup = Popup::Skill {
                            popup: SkillPopup::new(self.skills.to_vec(), &query),
                            range,
                        };
                    }
                }
            }
            None => {
                self.dismissed_dollar = None;
                if matches!(self.popup, Popup::Skill { .. }) {
                    self.popup = Popup::None;
                }
            }
        }
    }

    fn sync_command_popup(&mut self, allow: bool) {
        let text = self.editor.text();
        let first_line_end = text.find('\n').unwrap_or(text.len());
        let first_line = text[..first_line_end].to_string();
        let cursor = self.editor.cursor();
        let token = command_popup_filter_text(&first_line, 0);
        if let Some(t) = token.as_deref() {
            if self.dismissed_command_token.as_deref() == Some(t) {
                return;
            }
        }
        self.dismissed_command_token = None;
        if !allow {
            if matches!(self.popup, Popup::Command(_)) {
                self.popup = Popup::None;
            }
            return;
        }
        let caret_on_first_line = cursor <= first_line_end;
        let editing_name = caret_on_first_line
            && match command_under_cursor(&first_line, cursor) {
                Some((name, rest)) => {
                    if name.is_empty() {
                        rest.is_empty()
                    } else {
                        has_command_prefix(name, &self.wizard_items())
                    }
                }
                None => false,
            };
        let filter = caret_on_first_line
            .then(|| command_popup_filter_text(&first_line, cursor))
            .flatten();
        match &mut self.popup {
            Popup::Command(p) => {
                if editing_name {
                    if let Some(f) = filter {
                        p.set_filter(f.trim_start_matches('/'));
                    }
                } else {
                    self.popup = Popup::None;
                }
            }
            _ => {
                if editing_name {
                    if let Some(f) = filter {
                        self.popup = Popup::Command(SlashPopup::with_extra(
                            f.trim_start_matches('/'),
                            self.wizard_items(),
                        ));
                    }
                }
            }
        }
    }

    /// A known command name followed by a space becomes one cyan element once the cursor has
    /// left it (spec C.2.3).
    fn sync_slash_command_element(&mut self) {
        let text = self.editor.text();
        let first_line_end = text.find('\n').unwrap_or(text.len());
        let first_line = &text[..first_line_end];
        let cursor = self.editor.cursor();
        let desired = (|| {
            if self.bang {
                return None;
            }
            let (name, _, _) = parse_slash_name(first_line)?;
            if name.contains('/') {
                return None;
            }
            let end = 1 + name.len();
            if cursor <= first_line.len() && (1..end).contains(&cursor) {
                return None;
            }
            let space_after = first_line
                .get(end..)
                .and_then(|t| t.chars().next())
                .is_some_and(char::is_whitespace);
            if !space_after {
                return None;
            }
            commands::lookup(name).map(|_| 0..end)
        })();
        let slash_elements: Vec<Range<usize>> = self
            .editor
            .element_ranges()
            .into_iter()
            .filter(|r| text.get(r.clone()).is_some_and(|t| t.starts_with('/')))
            .collect();
        let mut has_desired = false;
        for r in slash_elements {
            if desired.as_ref() == Some(&r) {
                has_desired = true;
            } else {
                self.editor.remove_element_range(r);
            }
        }
        if let Some(r) = desired {
            if !has_desired {
                self.editor.add_element_range(r);
            }
        }
    }

    // ---- Ctrl+R history search --------------------------------------------------------------

    fn begin_search(&mut self) {
        if let Some(p) = self.burst.flush_before_modified_input() {
            self.handle_paste(&p);
        }
        self.burst.clear_window_after_non_char();
        self.popup = Popup::None;
        self.reset_footer_mode();
        self.search = Some(Search {
            original: self.snapshot_draft(),
            query: String::new(),
            status: SearchStatus::Idle,
        });
        self.history.reset_search();
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> ComposerEvent {
        if ctrl(KeyCode::Char('r')).is_press(key) || key.code == KeyCode::Up {
            self.search_step(SearchDirection::Older);
            return ComposerEvent::None;
        }
        if ctrl(KeyCode::Char('s')).is_press(key) || key.code == KeyCode::Down {
            self.search_step(SearchDirection::Newer);
            return ComposerEvent::None;
        }
        let ctrl_c = ctrl(KeyCode::Char('c')).is_press(key)
            || (key.code == KeyCode::Char('\u{3}') && key.modifiers == KeyModifiers::NONE);
        if key.code == KeyCode::Esc || ctrl_c {
            self.cancel_search();
            return ComposerEvent::None;
        }
        if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
            if self
                .search
                .as_ref()
                .is_some_and(|s| s.status == SearchStatus::Match)
            {
                self.search = None;
                self.history.reset_search();
                let len = self.editor.text().len();
                self.editor.set_cursor(len);
            }
            return ComposerEvent::None;
        }
        let is_bs = key.code == KeyCode::Backspace || ctrl(KeyCode::Char('h')).is_press(key);
        if is_bs {
            if let Some(s) = &self.search {
                let mut q = s.query.clone();
                q.pop();
                self.update_search_query(q);
            }
            return ComposerEvent::None;
        }
        if ctrl(KeyCode::Char('u')).is_press(key) {
            self.update_search_query(String::new());
            return ComposerEvent::None;
        }
        if let KeyCode::Char(ch) = key.code {
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                if let Some(s) = &self.search {
                    let mut q = s.query.clone();
                    q.push(ch);
                    self.update_search_query(q);
                }
            }
        }
        ComposerEvent::None
    }

    fn search_step(&mut self, dir: SearchDirection) {
        let Some((query, original)) = self
            .search
            .as_ref()
            .map(|s| (s.query.clone(), s.original.clone()))
        else {
            return;
        };
        if query.is_empty() {
            self.history.reset_search();
            if let Some(s) = self.search.as_mut() {
                s.status = SearchStatus::Idle;
            }
            self.restore_draft(original);
            return;
        }
        let r = self.history.search(&query, dir, false);
        self.apply_search_result(r);
    }

    fn update_search_query(&mut self, query: String) {
        let Some(original) = self.search.as_ref().map(|s| s.original.clone()) else {
            return;
        };
        if let Some(s) = self.search.as_mut() {
            s.query = query.clone();
        }
        self.restore_draft(original);
        if query.is_empty() {
            self.history.reset_search();
            if let Some(s) = self.search.as_mut() {
                s.status = SearchStatus::Idle;
            }
            return;
        }
        let r = self.history.search(&query, SearchDirection::Older, true);
        self.apply_search_result(r);
    }

    fn apply_search_result(&mut self, r: SearchResult) {
        match r {
            SearchResult::Found(e) => {
                if let Some(s) = self.search.as_mut() {
                    s.status = SearchStatus::Match;
                }
                self.apply_history_entry(e);
            }
            SearchResult::AtBoundary => {
                if let Some(s) = self.search.as_mut() {
                    s.status = SearchStatus::Match;
                }
            }
            SearchResult::NotFound => {
                let original = self.search.as_ref().map(|s| s.original.clone());
                if let Some(s) = self.search.as_mut() {
                    s.status = SearchStatus::NoMatch;
                }
                if let Some(o) = original {
                    self.restore_draft(o);
                }
            }
        }
    }

    fn cancel_search(&mut self) {
        let Some(s) = self.search.take() else {
            return;
        };
        self.history.reset_navigation();
        self.reset_footer_mode();
        self.restore_draft(s.original);
    }

    fn history_search_footer_line(&self) -> Option<Line<'static>> {
        let s = self.search.as_ref()?;
        let mut line = Line::from(vec![
            Span::styled("reverse-i-search: ", Style::default().dim()),
            Span::styled(s.query.clone(), Style::default().fg(Color::Cyan)),
        ]);
        match s.status {
            SearchStatus::Idle => {}
            SearchStatus::Match => {
                let key = Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
                    .remove_modifier(Modifier::DIM);
                line.spans.push(Span::styled("  ", Style::default().dim()));
                line.spans.push(Span::styled("enter", key));
                line.spans
                    .push(Span::styled(" accept", Style::default().dim()));
                line.spans.push(Span::styled(" · ", Style::default().dim()));
                line.spans.push(Span::styled("esc", key));
                line.spans
                    .push(Span::styled(" cancel", Style::default().dim()));
            }
            SearchStatus::NoMatch => line
                .spans
                .push(Span::styled("  no match", Style::default().fg(Color::Red))),
        }
        Some(line)
    }

    /// Every case-insensitive occurrence of the query in the previewed entry.
    fn history_search_highlights(&self) -> Vec<Range<usize>> {
        let Some(s) = self.search.as_ref() else {
            return Vec::new();
        };
        if s.status != SearchStatus::Match || s.query.is_empty() {
            return Vec::new();
        }
        case_insensitive_match_ranges(self.editor.text(), &s.query)
    }

    fn history_search_cursor_pos(&self, slot: Rect) -> Option<Position> {
        let s = self.search.as_ref()?;
        if slot.is_empty() {
            return None;
        }
        let prompt_w = "reverse-i-search: ".len() as u16;
        let query_w = crate::wrap::width_of(&s.query) as u16;
        let x = slot
            .x
            .saturating_add(FOOTER_INDENT_COLS)
            .saturating_add(prompt_w)
            .saturating_add(query_w)
            .min(slot.right().saturating_sub(1));
        Some(Position::new(x, slot.y))
    }
}

fn case_insensitive_match_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    let q: String = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Vec::new();
    }
    let mut folded = String::new();
    let mut spans: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    for (start, ch) in text.char_indices() {
        let orig = start..start + ch.len_utf8();
        for lower in ch.to_lowercase() {
            let fs = folded.len();
            folded.push(lower);
            spans.push((fs..folded.len(), orig.clone()));
        }
    }
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut from = 0;
    while let Some(i) = folded[from..].find(&q) {
        let fs = from + i;
        let fe = fs + q.len();
        let first = spans
            .iter()
            .find(|(f, _)| f.start >= fs)
            .map(|(_, o)| o.start);
        let last = spans
            .iter()
            .rev()
            .find(|(f, _)| f.end <= fe)
            .map(|(_, o)| o.end);
        if let (Some(a), Some(b)) = (first, last) {
            if a < b && out.last().is_none_or(|r| r.end <= a) {
                out.push(a..b);
            }
        }
        from = fe;
    }
    out
}

/// Replace each pasted-content placeholder element with the text it stands for. Returns the new
/// text and the element ranges that remain (none: expanded text has no placeholders).
pub fn expand_pending_pastes(
    text: &str,
    elements: &[Range<usize>],
    pending: &[(String, String)],
) -> (String, Vec<Range<usize>>) {
    if pending.is_empty() {
        return (text.to_string(), elements.to_vec());
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut sorted: Vec<Range<usize>> = elements.to_vec();
    sorted.sort_by_key(|r| r.start);
    for r in sorted {
        let Some(slice) = text.get(r.clone()) else {
            continue;
        };
        if let Some((_, payload)) = pending.iter().find(|(ph, _)| ph == slice) {
            out.push_str(&text[last..r.start]);
            out.push_str(payload);
            last = r.end;
        }
    }
    out.push_str(&text[last..]);
    (out, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};

    fn pal() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    fn comp() -> Composer {
        pal();
        Composer::new("Explain this codebase")
    }

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn type_str(c: &mut Composer, s: &str) {
        for ch in s.chars() {
            c.handle_key(k(KeyCode::Char(ch)), false);
        }
    }

    fn ctx() -> FooterCtx {
        FooterCtx {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            cwd: "~/proj".into(),
            ..Default::default()
        }
    }

    /// The composer drawn at `w` columns with its footer, as text rows (trailing blanks cut).
    fn draw(c: &mut Composer, w: u16, ctx: &FooterCtx) -> Vec<String> {
        let h = c.desired_height(w, ctx);
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        c.render(area, &mut buf, ctx);
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn empty_composer_matches_start_02() {
        let mut c = comp();
        let rows = draw(&mut c, 120, &ctx());
        assert_eq!(
            rows,
            vec![
                "".to_string(),
                "› Explain this codebase".to_string(),
                "".to_string(),
                "  gpt-5.5 default · ~/proj".to_string()
            ]
        );
    }

    #[test]
    fn typing_and_enter_submit() {
        let mut c = comp();
        type_str(&mut c, "hello world");
        assert_eq!(c.text(), "hello world");
        assert_eq!(
            c.handle_key(k(KeyCode::Enter), false),
            ComposerEvent::Submit("hello world".into())
        );
        assert!(c.is_empty());
    }

    #[test]
    fn shift_enter_and_ctrl_j_insert_newlines() {
        let mut c = comp();
        type_str(&mut c, "a");
        c.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), false);
        type_str(&mut c, "b");
        c.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            false,
        );
        type_str(&mut c, "c");
        assert_eq!(c.text(), "a\nb\nc");
    }

    #[test]
    fn tab_queues_while_running_and_submits_when_idle() {
        let mut c = comp();
        type_str(&mut c, "later");
        assert_eq!(
            c.handle_key(k(KeyCode::Tab), true),
            ComposerEvent::Queue("later".into())
        );
        type_str(&mut c, "now");
        assert_eq!(
            c.handle_key(k(KeyCode::Tab), false),
            ComposerEvent::Submit("now".into())
        );
    }

    #[test]
    fn question_mark_opens_the_overlay_and_a_second_one_keeps_it() {
        let mut c = comp();
        c.handle_key(k(KeyCode::Char('?')), false);
        assert!(c.text().is_empty());
        assert_eq!(draw(&mut c, 120, &ctx()).len(), 12);
        c.handle_key(k(KeyCode::Char('?')), false);
        assert!(c.overlay);
        c.handle_key(k(KeyCode::Esc), false);
        assert!(c.overlay);
        c.handle_key(k(KeyCode::Char('x')), false);
        assert!(!c.overlay);
        assert_eq!(c.text(), "x");
    }

    #[test]
    fn a_wizard_command_is_run_as_typed_and_an_unknown_one_is_refused() {
        let mut c = comp();
        c.backend_commands = vec![
            ("mode".into(), "pick or switch personality mode".into()),
            ("help".into(), "show available commands".into()),
        ];
        type_str(&mut c, "/mode");
        assert_eq!(
            c.handle_key(k(KeyCode::Enter), false),
            ComposerEvent::Command("/mode".into())
        );
        type_str(&mut c, "/help");
        assert_eq!(
            c.handle_key(k(KeyCode::Enter), false),
            ComposerEvent::Command("/help".into())
        );
        type_str(&mut c, "/frobnicate");
        assert!(matches!(
            c.handle_key(k(KeyCode::Enter), false),
            ComposerEvent::Info(t) if t.starts_with("Unrecognized command '/frobnicate'")
        ));
    }

    #[test]
    fn the_popup_lists_wizard_commands_after_codexs() {
        let mut c = comp();
        c.backend_commands = vec![
            ("mode".into(), "pick or switch personality mode".into()),
            ("model".into(), "pick or switch the model".into()),
        ];
        type_str(&mut c, "/mo");
        let rows = draw(&mut c, 120, &ctx());
        let model = rows
            .iter()
            .position(|r| r.contains("/model "))
            .expect("/model");
        let mode = rows
            .iter()
            .position(|r| r.contains("/mode "))
            .expect("/mode");
        assert!(model < mode, "{rows:?}");
        assert!(
            rows[mode].contains("pick or switch personality mode"),
            "{rows:?}"
        );
        assert_eq!(rows.iter().filter(|r| r.contains("/model ")).count(), 1);
    }

    #[test]
    fn question_mark_in_a_draft_is_text() {
        let mut c = comp();
        type_str(&mut c, "why?");
        assert_eq!(c.text(), "why?");
    }

    #[test]
    fn bang_is_absorbed_into_shell_mode() {
        let mut c = comp();
        type_str(&mut c, "!ls");
        assert!(c.is_bang_mode());
        assert_eq!(c.text(), "ls");
        let rows = draw(&mut c, 120, &ctx());
        assert_eq!(rows[1], "! ls");
        assert!(rows[3].ends_with("Shell mode"));
        assert_eq!(
            c.handle_key(k(KeyCode::Enter), false),
            ComposerEvent::Shell("ls".into())
        );
        assert!(!c.is_bang_mode());
    }

    #[test]
    fn esc_leaves_shell_mode_only_when_empty() {
        let mut c = comp();
        type_str(&mut c, "!");
        assert!(c.is_bang_mode());
        assert!(c.captures_escape(k(KeyCode::Esc)));
        c.handle_key(k(KeyCode::Esc), false);
        assert!(!c.is_bang_mode());
        type_str(&mut c, "!x");
        c.handle_key(k(KeyCode::Esc), false);
        assert!(c.is_bang_mode());
    }

    #[test]
    fn backspace_on_empty_shell_draft_leaves_shell_mode() {
        let mut c = comp();
        type_str(&mut c, "!");
        c.handle_key(k(KeyCode::Backspace), false);
        assert!(!c.is_bang_mode());
    }

    #[test]
    fn large_paste_becomes_an_element_and_expands_on_submit() {
        let mut c = comp();
        let big = "x".repeat(1500);
        c.handle_paste(&big);
        assert_eq!(c.text(), "[Pasted Content 1500 chars]");
        type_str(&mut c, " and more");
        assert_eq!(c.text(), "[Pasted Content 1500 chars] and more");
        let rows = draw(&mut c, 120, &ctx());
        assert_eq!(rows[1], "› [Pasted Content 1500 chars] and more");
        match c.handle_key(k(KeyCode::Enter), false) {
            ComposerEvent::Submit(t) => assert_eq!(t, format!("{big} and more")),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn second_equal_paste_is_numbered_and_backspace_removes_it_whole() {
        let mut c = comp();
        c.handle_paste(&"a".repeat(1500));
        c.handle_paste(&"b".repeat(1500));
        assert_eq!(
            c.text(),
            "[Pasted Content 1500 chars][Pasted Content 1500 chars] #2"
        );
        c.handle_key(k(KeyCode::Backspace), false);
        assert_eq!(c.text(), "[Pasted Content 1500 chars]");
        assert_eq!(c.pending_pastes.len(), 1);
        // With the first gone too, the next paste reuses the bare label.
        c.handle_key(k(KeyCode::Backspace), false);
        c.handle_paste(&"c".repeat(1500));
        assert_eq!(c.text(), "[Pasted Content 1500 chars]");
    }

    #[test]
    fn small_paste_is_inserted_verbatim_with_crlf_normalised() {
        let mut c = comp();
        c.handle_paste("line one\r\nline two\rline three");
        assert_eq!(c.text(), "line one\nline two\nline three");
        assert_eq!(draw(&mut c, 120, &ctx()).len(), 3 + 2 + 1);
    }

    #[test]
    fn slash_popup_opens_filters_and_enter_dispatches() {
        let mut c = comp();
        type_str(&mut c, "/mo");
        assert!(c.popup_active());
        let rows = draw(&mut c, 120, &ctx());
        assert_eq!(rows[1], "› /mo");
        assert_eq!(
            rows[3],
            "  /model  choose what model and reasoning effort to use"
        );
        assert_eq!(
            c.handle_key(k(KeyCode::Enter), false),
            ComposerEvent::Command("/model".into())
        );
        assert!(c.is_empty());
        assert!(!c.popup_active());
    }

    #[test]
    fn nonsense_command_has_no_popup_and_submit_says_so_and_keeps_the_draft() {
        let mut c = comp();
        type_str(&mut c, "/nonsense");
        assert!(!c.popup_active());
        match c.handle_key(k(KeyCode::Enter), false) {
            ComposerEvent::Info(m) => assert_eq!(
                m,
                "Unrecognized command '/nonsense'. Type \"/\" for a list of supported commands."
            ),
            e => panic!("{e:?}"),
        }
        assert_eq!(c.text(), "/nonsense");
    }

    #[test]
    fn leading_space_disables_slash_parsing() {
        let mut c = comp();
        type_str(&mut c, " /model");
        assert!(!c.popup_active());
        assert_eq!(
            c.handle_key(k(KeyCode::Enter), false),
            ComposerEvent::Submit("/model".into())
        );
    }

    #[test]
    fn tab_completes_to_the_first_prefix_match_and_makes_an_element() {
        let mut c = comp();
        type_str(&mut c, "/re");
        c.handle_key(k(KeyCode::Tab), false);
        assert_eq!(c.text(), "/review ");
        assert!(!c.popup_active());
        assert_eq!(c.editor.element_ranges(), vec![0..7]);
        let area = Rect::new(0, 0, 60, 5);
        let mut buf = Buffer::empty(area);
        c.render(area, &mut buf, &ctx());
        assert_eq!(buf[(2, 1)].fg, Color::Cyan);
        assert!(!buf[(2, 1)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tab_keeps_a_draft_tail_as_arguments() {
        let mut c = comp();
        c.editor.set_text_clearing_elements("/review the diff");
        c.editor.set_cursor(3);
        c.sync_popups();
        assert!(c.popup_active());
        c.handle_key(k(KeyCode::Tab), false);
        assert_eq!(c.text(), "/review view the diff");
    }

    #[test]
    fn esc_dismisses_the_popup_until_the_token_changes() {
        let mut c = comp();
        type_str(&mut c, "/mo");
        assert!(c.captures_escape(k(KeyCode::Esc)));
        c.handle_key(k(KeyCode::Esc), false);
        assert!(!c.popup_active());
        assert_eq!(c.text(), "/mo");
        type_str(&mut c, "d");
        assert!(c.popup_active());
    }

    #[test]
    fn command_disabled_during_a_task_is_an_error_and_keeps_a_typed_draft() {
        let mut c = comp();
        type_str(&mut c, "/new");
        // Dismiss the popup so Enter takes the typed path.
        c.handle_key(k(KeyCode::Esc), true);
        match c.handle_key(k(KeyCode::Enter), true) {
            ComposerEvent::Error(m) => {
                assert_eq!(m, "'/new' is disabled while a task is in progress.")
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(c.text(), "/new");
    }

    #[test]
    fn up_recalls_the_last_prompt_and_down_clears() {
        let mut c = comp();
        type_str(&mut c, "first");
        c.handle_key(k(KeyCode::Enter), false);
        type_str(&mut c, "second");
        c.handle_key(k(KeyCode::Enter), false);
        c.handle_key(k(KeyCode::Up), false);
        assert_eq!(c.text(), "second");
        c.handle_key(k(KeyCode::Up), false);
        assert_eq!(c.text(), "first");
        c.handle_key(k(KeyCode::Down), false);
        c.handle_key(k(KeyCode::Down), false);
        assert_eq!(c.text(), "");
    }

    #[test]
    fn ctrl_c_clears_the_draft_and_up_recalls_it() {
        let mut c = comp();
        type_str(&mut c, "draft text");
        assert!(c.captures_ctrl_c());
        assert_eq!(
            c.handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            ComposerEvent::Cleared
        );
        assert!(c.is_empty());
        c.handle_key(k(KeyCode::Up), false);
        assert_eq!(c.text(), "draft text");
    }

    #[test]
    fn ctrl_r_search_previews_highlights_and_accepts() {
        let mut c = comp();
        for t in ["first prompt fake:text", "second prompt fake:text"] {
            type_str(&mut c, t);
            c.handle_key(k(KeyCode::Enter), false);
        }
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        c.handle_key(ctrl_r, false);
        let rows = draw(&mut c, 120, &ctx());
        assert_eq!(rows[1], "› Explain this codebase");
        assert_eq!(rows[3], "  reverse-i-search:");
        type_str(&mut c, "first");
        let rows = draw(&mut c, 120, &ctx());
        assert_eq!(rows[1], "› first prompt fake:text");
        assert_eq!(
            rows[3],
            "  reverse-i-search: first  enter accept · esc cancel"
        );
        let area = Rect::new(0, 0, 120, 4);
        let mut buf = Buffer::empty(area);
        c.render(area, &mut buf, &ctx());
        let m = buf[(2, 1)].modifier;
        assert!(m.contains(Modifier::REVERSED) && m.contains(Modifier::BOLD));
        assert!(!buf[(9, 1)].modifier.contains(Modifier::REVERSED));
        c.handle_key(k(KeyCode::Enter), false);
        assert_eq!(c.text(), "first prompt fake:text");
        assert!(c.search.is_none());
    }

    #[test]
    fn ctrl_r_no_match_and_esc_restore_the_draft() {
        let mut c = comp();
        type_str(&mut c, "hello");
        c.handle_key(k(KeyCode::Enter), false);
        type_str(&mut c, "keep me");
        c.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            false,
        );
        type_str(&mut c, "zzz");
        let rows = draw(&mut c, 120, &ctx());
        assert_eq!(rows[3], "  reverse-i-search: zzz  no match");
        assert_eq!(c.text(), "keep me");
        c.handle_key(k(KeyCode::Esc), false);
        assert_eq!(c.text(), "keep me");
        assert!(c.search.is_none());
    }

    #[test]
    fn mention_popup_inserts_the_path_and_a_space() {
        let mut c = comp();
        c.set_file_index(FileIndex::from_files(
            ["src/lib.rs", "src/main.rs", "notes.txt"]
                .map(String::from)
                .to_vec(),
        ));
        type_str(&mut c, "see @lib");
        assert!(c.popup_active());
        c.handle_key(k(KeyCode::Enter), false);
        assert_eq!(c.text(), "see src/lib.rs ");
        assert!(!c.popup_active());
        assert_eq!(c.editor.cursor(), c.text().len());
    }

    fn skill(name: &str, desc: &str) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            path: PathBuf::new(),
        }
    }

    fn with_skills() -> Composer {
        let mut c = comp();
        c.set_skills(vec![
            skill("common-sense", "Judgment."),
            skill("wrangler", "Open PRs."),
        ]);
        c.set_file_index(FileIndex::from_files(vec!["wrap.rs".into()]));
        c
    }

    #[test]
    fn dollar_opens_the_skill_popup_and_enter_inserts_one_element() {
        let mut c = with_skills();
        type_str(&mut c, "use $wr");
        assert!(matches!(c.popup, Popup::Skill { .. }));
        c.handle_key(k(KeyCode::Enter), false);
        assert_eq!(c.text(), "use $wrangler ");
        assert!(!c.popup_active());
        assert_eq!(c.editor.element_ranges(), vec![4..13]);
        // the element is atomic: one Backspace removes the whole `$wrangler`
        c.handle_key(k(KeyCode::Backspace), false);
        c.handle_key(k(KeyCode::Backspace), false);
        assert_eq!(c.text(), "use ");
    }

    #[test]
    fn empty_dollar_lists_every_skill_and_esc_dismisses_it() {
        let mut c = with_skills();
        type_str(&mut c, "$");
        assert!(matches!(c.popup, Popup::Skill { .. }));
        c.handle_key(k(KeyCode::Esc), false);
        assert!(!c.popup_active());
        assert_eq!(c.text(), "$");
        // typing on re-opens it for the new token
        type_str(&mut c, "c");
        assert!(matches!(c.popup, Popup::Skill { .. }));
    }

    #[test]
    fn shell_variables_and_parameters_do_not_open_it() {
        for text in ["echo $HOME", "echo $1", "echo $_", "x $PATH"] {
            let mut c = with_skills();
            type_str(&mut c, text);
            assert!(!c.popup_active(), "{text}");
        }
        // ambiguous: a digit-leading name opens it only when a skill is spelled that way
        let mut c = comp();
        c.set_skills(vec![skill("3d-model", "Make meshes.")]);
        type_str(&mut c, "$3d");
        assert!(c.popup_active());
        let mut c = with_skills();
        type_str(&mut c, "$3d");
        assert!(!c.popup_active());
    }

    #[test]
    fn no_skills_means_no_dollar_popup() {
        let mut c = comp();
        type_str(&mut c, "$a");
        assert!(!c.popup_active());
    }

    #[test]
    fn a_skill_picked_from_the_at_popup_becomes_a_dollar_element() {
        let mut c = with_skills();
        type_str(&mut c, "@wran");
        c.handle_key(k(KeyCode::Enter), false);
        assert_eq!(c.text(), "$wrangler ");
        assert_eq!(c.editor.element_ranges(), vec![0..9]);
        // a file still inserts as plain text
        let mut c = with_skills();
        type_str(&mut c, "@wrap");
        c.handle_key(k(KeyCode::Enter), false);
        assert!(
            c.text() == "wrap.rs " || c.text() == "$wrangler ",
            "{}",
            c.text()
        );
    }

    #[test]
    fn two_skills_in_one_draft_are_two_elements() {
        let mut c = with_skills();
        type_str(&mut c, "$wr");
        c.handle_key(k(KeyCode::Enter), false);
        type_str(&mut c, "$co");
        c.handle_key(k(KeyCode::Enter), false);
        assert_eq!(c.text(), "$wrangler $common-sense ");
        assert_eq!(c.editor.element_ranges().len(), 2);
    }

    #[test]
    fn mention_popup_quotes_paths_with_spaces() {
        let mut c = comp();
        c.set_file_index(FileIndex::from_files(vec!["my notes.txt".into()]));
        type_str(&mut c, "@notes");
        c.handle_key(k(KeyCode::Tab), false);
        assert_eq!(c.text(), "\"my notes.txt\" ");
    }

    #[test]
    fn esc_closes_the_mention_popup_and_keeps_the_text() {
        let mut c = comp();
        c.set_file_index(FileIndex::from_files(vec!["a.rs".into()]));
        type_str(&mut c, "@a");
        assert!(c.popup_active());
        c.handle_key(k(KeyCode::Esc), false);
        assert!(!c.popup_active());
        assert_eq!(c.text(), "@a");
    }

    #[test]
    fn plain_text_after_a_typed_mention_stays_plain() {
        let mut c = comp();
        c.set_file_index(FileIndex::from_files(vec!["src/lib.rs".into()]));
        type_str(&mut c, "@src/lib.rs");
        c.handle_key(k(KeyCode::Esc), false);
        let area = Rect::new(0, 0, 60, 4);
        let mut buf = Buffer::empty(area);
        c.render(area, &mut buf, &ctx());
        assert_eq!(buf[(2, 1)].fg, Color::Reset);
    }

    #[test]
    fn vim_slash_in_empty_normal_mode_starts_a_command() {
        let mut c = comp();
        c.set_vim_enabled(true);
        c.handle_key(k(KeyCode::Char('/')), false);
        assert_eq!(c.text(), "/");
        assert_eq!(c.vim_label(), Some("Insert"));
        assert!(c.popup_active());
    }

    #[test]
    fn vim_footer_labels_follow_the_mode() {
        let mut c = comp();
        c.set_vim_enabled(true);
        let rows = draw(&mut c, 120, &ctx());
        assert!(rows[3].trim_end().ends_with("Vim: Normal"));
        c.handle_key(k(KeyCode::Char('i')), false);
        let rows = draw(&mut c, 120, &ctx());
        assert!(rows[3].trim_end().ends_with("Vim: Insert"));
    }

    #[test]
    fn exactly_full_row_gets_a_cursor_row() {
        let mut c = comp();
        c.handle_paste(&"a".repeat(117));
        let rows = draw(&mut c, 120, &ctx());
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn at_narrow_widths_the_footer_status_line_truncates_with_an_ellipsis() {
        let mut c = comp();
        let mut cx = ctx();
        cx.cwd = "~/proj/a-very-long-directory-name/with/several/nested".into();
        let rows = draw(&mut c, 40, &cx);
        assert!(rows[3].ends_with('…'), "{rows:?}");
    }

    #[test]
    fn queue_hint_replaces_the_status_line_while_running() {
        let mut c = comp();
        type_str(&mut c, "draft while running");
        let mut cx = ctx();
        cx.task_running = true;
        let rows = draw(&mut c, 120, &cx);
        assert_eq!(rows[3], "  tab to queue message");
    }

    #[test]
    fn esc_hint_replaces_the_status_line_when_idle_and_empty() {
        let mut c = comp();
        c.on_esc_idle();
        let rows = draw(&mut c, 120, &ctx());
        assert_eq!(rows[3], "  esc esc to edit previous message");
        let mut cx = ctx();
        cx.esc_hint = true;
        let rows = draw(&mut c, 120, &cx);
        assert_eq!(rows[3], "  esc again to edit previous message");
        c.handle_key(k(KeyCode::Char('a')), false);
        let rows = draw(&mut c, 120, &ctx());
        assert!(rows[3].starts_with("  gpt-5.5"));
    }

    #[test]
    fn plan_mode_label_sits_on_the_right() {
        let mut c = comp();
        let mut cx = ctx();
        cx.plan_mode = true;
        let rows = draw(&mut c, 120, &cx);
        assert!(rows[3].starts_with("  gpt-5.5 default · ~/proj"));
        assert!(rows[3].ends_with("Plan mode (shift+tab to cycle)"));
        assert_eq!(rows[3].chars().count(), 118);
    }

    #[test]
    fn paste_burst_makes_fast_typing_one_paste_and_a_held_char_flushes() {
        let mut c = comp();
        c.set_paste_burst(true);
        // Three characters inside the interval start a burst; nothing is shown yet.
        for ch in "abc".chars() {
            c.handle_key(k(KeyCode::Char(ch)), false);
        }
        assert_eq!(c.text(), "");
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(c.flush_paste_burst_if_due());
        assert_eq!(c.text(), "abc");
    }

    #[test]
    fn question_mark_inside_a_burst_is_text() {
        let mut c = comp();
        c.set_paste_burst(true);
        for ch in "ab".chars() {
            c.handle_key(k(KeyCode::Char(ch)), false);
        }
        c.handle_key(k(KeyCode::Char('?')), false);
        std::thread::sleep(std::time::Duration::from_millis(30));
        c.flush_paste_burst_if_due();
        assert_eq!(c.text(), "ab?");
        assert!(!c.overlay);
    }

    #[test]
    fn long_messages_are_refused_with_codexs_wording() {
        let mut c = comp();
        c.handle_paste(&"y".repeat(MAX_USER_INPUT_TEXT_CHARS + 5));
        match c.handle_key(k(KeyCode::Enter), false) {
            ComposerEvent::Error(m) => assert_eq!(
                m,
                format!(
                    "Message exceeds the maximum length of 1048576 characters ({} provided).",
                    MAX_USER_INPUT_TEXT_CHARS + 5
                )
            ),
            e => panic!("{e:?}"),
        }
        assert!(!c.is_empty());
    }
}
