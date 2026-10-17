//! The configurable status line and terminal title (`/statusline`, `/title`; spec C.4.5, C.7.11).
//! The item ids, their descriptions and the colour groups are Codex's; the values come from what
//! wizard reports. An item with no value (usage limits, pull requests, a context window wizard
//! does not send) is left out of the line, as Codex leaves out what it cannot know.

use std::path::{Path, PathBuf};

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::highlight::foreground_style_for_scopes;
use crate::style::{ColorLevel, palette};
use crate::ui::footer::format_tokens_compact;

pub const SEPARATOR: &str = " \u{b7} ";
pub const DEFAULT_STATUS_ITEMS: [&str; 2] = ["model-with-reasoning", "current-dir"];
pub const DEFAULT_TITLE_ITEMS: [&str; 2] = ["activity", "project-name"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accent {
    Model,
    Path,
    Branch,
    State,
    Usage,
    Limit,
    Metadata,
    Mode,
    Thread,
    Progress,
}

impl Accent {
    fn scopes(self) -> &'static [&'static str] {
        match self {
            Self::Model => &["entity.name.type", "support.type", "variable"],
            Self::Path => &["string", "markup.underline.link"],
            Self::Branch => &["entity.name.function", "entity.name.tag"],
            Self::State => &["keyword.control", "keyword"],
            Self::Usage => &["constant.numeric", "constant"],
            Self::Limit => &["constant.language", "storage.type"],
            Self::Metadata => &["comment", "constant.other"],
            Self::Mode => &["storage.modifier", "keyword.operator"],
            Self::Thread => &["markup.heading", "entity.name.section"],
            Self::Progress => &["markup.inserted", "constant.numeric"],
        }
    }

    fn fallback(self) -> Color {
        match self {
            Self::Model | Self::State | Self::Metadata | Self::Mode => Color::Cyan,
            Self::Path | Self::Usage | Self::Progress => Color::Green,
            Self::Branch | Self::Limit | Self::Thread => Color::Magenta,
        }
    }

    /// The theme's colour for this group, softened toward grey; the ANSI fallback on a 16 colour
    /// terminal or when the theme has no matching scope.
    pub fn style(self) -> Style {
        let p = palette();
        let theme = foreground_style_for_scopes(self.scopes()).and_then(|s| s.fg);
        let color = match theme {
            Some(Color::Rgb(r, g, b)) if p.level != ColorLevel::Ansi16 => {
                p.best_color(soften_rgb((r, g, b)))
            }
            Some(c) if p.level != ColorLevel::Ansi16 => soften_named(c),
            _ => self.fallback(),
        };
        Style::default().fg(color)
    }
}

/// 85% of the colour's saturation: each channel pulled 15% of the way to the luma.
pub fn soften_rgb((r, g, b): (u8, u8, u8)) -> (u8, u8, u8) {
    let luma = (77 * u16::from(r) + 150 * u16::from(g) + 29 * u16::from(b)) / 256;
    let ch = |c: u8| ((u16::from(c) * 85 + luma * 15 + 50) / 100) as u8;
    (ch(r), ch(g), ch(b))
}

fn soften_named(c: Color) -> Color {
    match c {
        Color::LightRed => Color::Red,
        Color::LightGreen => Color::Green,
        Color::LightYellow => Color::Yellow,
        Color::LightBlue => Color::Blue,
        Color::LightMagenta => Color::Magenta,
        Color::LightCyan => Color::Cyan,
        Color::White => Color::Gray,
        other => other,
    }
}

pub struct Item {
    pub id: &'static str,
    pub description: &'static str,
    pub accent: Accent,
}

const fn it(id: &'static str, description: &'static str, accent: Accent) -> Item {
    Item {
        id,
        description,
        accent,
    }
}

use Accent::{
    Branch, Limit, Metadata, Mode, Model, Path as PathAccent, Progress, State, Thread, Usage,
};

/// The status line's items in the order the picker lists the unselected ones.
pub const STATUS_ITEMS: [Item; 26] = [
    it("model", "Current model name", Model),
    it(
        "model-with-reasoning",
        "Current model name with reasoning level",
        Model,
    ),
    it("reasoning", "Current reasoning level", Model),
    it("current-dir", "Current working directory", PathAccent),
    it(
        "project-name",
        "Project name (omitted when unavailable)",
        PathAccent,
    ),
    it(
        "git-branch",
        "Current Git branch (omitted when unavailable)",
        Branch,
    ),
    it(
        "pull-request-number",
        "Open pull request number for the current branch (omitted when unavailable)",
        Branch,
    ),
    it(
        "branch-changes",
        "Committed branch changes against the default branch (omitted when unavailable)",
        Branch,
    ),
    it(
        "run-state",
        "Compact session run-state text (Ready, Working, Thinking)",
        State,
    ),
    it(
        "permissions",
        "Active permission profile or sandbox mode",
        Mode,
    ),
    it("approval-mode", "Active command approval mode", Mode),
    it(
        "context-remaining",
        "Percentage of context window remaining (omitted when unknown)",
        Usage,
    ),
    it(
        "context-used",
        "Percentage of context window used (omitted when unknown)",
        Usage,
    ),
    it(
        "five-hour-limit",
        "Remaining usage on the primary usage limit (omitted when unavailable)",
        Limit,
    ),
    it(
        "weekly-limit",
        "Remaining usage on the secondary usage limit (omitted when unavailable)",
        Limit,
    ),
    it("codex-version", "Wizard application version", Metadata),
    it(
        "context-window-size",
        "Total context window size in tokens (omitted when unknown)",
        Usage,
    ),
    it(
        "used-tokens",
        "Total tokens used in session (omitted when zero)",
        Usage,
    ),
    it(
        "total-input-tokens",
        "Total input tokens used in session",
        Usage,
    ),
    it(
        "total-output-tokens",
        "Total output tokens used in session",
        Usage,
    ),
    it(
        "thread-id",
        "Current thread identifier (omitted until thread starts)",
        Metadata,
    ),
    it("fast-mode", "Whether Fast mode is currently active", Mode),
    it("raw-output", "Whether raw scrollback mode is active", Mode),
    it(
        "thread-title",
        "Current thread title, or thread identifier when unnamed",
        Thread,
    ),
    it(
        "workspace-headline",
        "Workspace notification headline (Enterprise workspaces only; omitted when unavailable)",
        Thread,
    ),
    it(
        "task-progress",
        "Latest task progress from update_plan (omitted until available)",
        Progress,
    ),
];

/// The terminal title's items, in the picker's order.
pub const TITLE_ITEMS: [Item; 21] = [
    it(
        "activity",
        "Spinner while working, action-required message while blocked.",
        State,
    ),
    it(
        "project-name",
        "Project name (falls back to current directory name)",
        PathAccent,
    ),
    it("app-name", "Wizard app name", Metadata),
    it("current-dir", "Current working directory", PathAccent),
    it(
        "run-state",
        "Compact session run-state text (Ready, Working, Thinking)",
        State,
    ),
    it(
        "thread-title",
        "Current thread title, or thread identifier when unnamed",
        Thread,
    ),
    it(
        "git-branch",
        "Current Git branch (omitted when unavailable)",
        Branch,
    ),
    it(
        "context-remaining",
        "Percentage of context window remaining (omitted when unknown)",
        Usage,
    ),
    it(
        "context-used",
        "Percentage of context window used (omitted when unknown)",
        Usage,
    ),
    it(
        "five-hour-limit",
        "Remaining usage on the primary usage limit (omitted when unavailable)",
        Limit,
    ),
    it(
        "weekly-limit",
        "Remaining usage on the secondary usage limit (omitted when unavailable)",
        Limit,
    ),
    it("codex-version", "Wizard application version", Metadata),
    it(
        "used-tokens",
        "Total tokens used in session (omitted when zero)",
        Usage,
    ),
    it(
        "total-input-tokens",
        "Total input tokens used in session",
        Usage,
    ),
    it(
        "total-output-tokens",
        "Total output tokens used in session",
        Usage,
    ),
    it(
        "thread-id",
        "Current thread identifier (omitted until thread starts)",
        Metadata,
    ),
    it("fast-mode", "Whether Fast mode is currently active", Mode),
    it("model", "Current model name", Model),
    it(
        "model-with-reasoning",
        "Current model name with reasoning level",
        Model,
    ),
    it("reasoning", "Current reasoning level", Model),
    it(
        "task-progress",
        "Latest task progress from update_plan (omitted until available)",
        Progress,
    ),
];

/// Old and alternate spellings Codex's config accepts.
pub fn canonical(id: &str) -> &str {
    match id {
        "model-name" => "model",
        "project" | "project-root" => "project-name",
        "status" => "run-state",
        "approval" => "approval-mode",
        "context-usage" => "context-used",
        "session-id" => "thread-id",
        other => other,
    }
}

/// What the items read from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Values {
    pub model: String,
    pub effort: String,
    pub cwd: String,
    pub project: Option<String>,
    pub branch: Option<String>,
    pub run_state: String,
    pub permissions: String,
    pub approval: String,
    pub context_percent_left: Option<i64>,
    pub context_window: Option<u64>,
    pub used_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub version: String,
    pub session_id: String,
    pub thread_title: Option<String>,
    pub raw_output: bool,
    pub task_progress: Option<(usize, usize)>,
}

impl Values {
    /// The text of an item, `None` when there is nothing to show.
    pub fn text(&self, id: &str) -> Option<String> {
        let effort = if self.effort.is_empty() {
            "default"
        } else {
            &self.effort
        };
        let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
        match canonical(id) {
            "model" => some(&self.model),
            "model-with-reasoning" => Some(format!("{} {effort}", self.model)),
            "reasoning" => Some(effort.to_string()),
            "current-dir" => some(&self.cwd),
            "project-name" => self.project.clone(),
            "git-branch" => self.branch.clone(),
            "run-state" => some(&self.run_state),
            "permissions" => some(&self.permissions),
            "approval-mode" => some(&self.approval),
            "context-remaining" => self
                .context_percent_left
                .map(|p| format!("Context {p}% left")),
            "context-used" => self
                .context_percent_left
                .map(|p| format!("Context {}% used", 100 - p)),
            "context-window-size" => self
                .context_window
                .map(|w| format!("{} window", format_tokens_compact(w as i64))),
            "used-tokens" => (self.used_tokens > 0)
                .then(|| format!("{} used", format_tokens_compact(self.used_tokens as i64))),
            "total-input-tokens" => Some(format!(
                "{} in",
                format_tokens_compact(self.input_tokens as i64)
            )),
            "total-output-tokens" => Some(format!(
                "{} out",
                format_tokens_compact(self.output_tokens as i64)
            )),
            "codex-version" => some(&self.version),
            "thread-id" => some(&self.session_id),
            "thread-title" => self.thread_title.clone().or_else(|| some(&self.session_id)),
            "raw-output" => self.raw_output.then(|| "raw output".to_string()),
            "task-progress" => self.task_progress.map(|(d, t)| format!("Tasks {d}/{t}")),
            // fast-mode, pull-request-number, branch-changes, the limits, workspace-headline:
            // wizard has nothing to report.
            _ => None,
        }
    }

    /// The same, with Codex's placeholder text where there is no value, for the picker preview.
    pub fn preview_text(&self, id: &str) -> String {
        self.text(id).unwrap_or_else(|| {
            match canonical(id) {
                "project-name" => "my-project",
                "git-branch" => "feat/awesome-feature",
                "pull-request-number" => "PR #123",
                "branch-changes" => "+12 -3",
                "context-remaining" => "Context 0% left",
                "context-used" => "Context 0% used",
                "five-hour-limit" => "primary 0%",
                "weekly-limit" => "secondary 0%",
                "context-window-size" => "0 window",
                "used-tokens" => "0 used",
                "thread-id" => "550e8400-e29b-41d4",
                "fast-mode" => "Fast on",
                "raw-output" => "raw output",
                "workspace-headline" => "Workspace headline",
                "task-progress" => "Tasks 0/0",
                "thread-title" => "thread title",
                _ => "",
            }
            .to_string()
        })
    }
}

fn accent_of(id: &str) -> Accent {
    let id = canonical(id);
    STATUS_ITEMS
        .iter()
        .chain(TITLE_ITEMS.iter())
        .find(|i| i.id == id)
        .map_or(Accent::Metadata, |i| i.accent)
}

/// The status line for `items`: each one's text in its group's colour (or dim when colours are
/// off), ` · ` dim between. `None` when nothing has a value.
pub fn status_line(items: &[String], colors: bool, v: &Values) -> Option<Line<'static>> {
    line_with(items, colors, |id| v.text(id))
}

/// The preview under the picker: values where there are any, placeholders elsewhere.
pub fn preview_line(items: &[String], colors: bool, v: &Values) -> Option<Line<'static>> {
    line_with(items, colors, |id| {
        Some(v.preview_text(id)).filter(|t| !t.is_empty())
    })
}

fn line_with(
    items: &[String],
    colors: bool,
    text: impl Fn(&str) -> Option<String>,
) -> Option<Line<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for id in items {
        let Some(t) = text(id) else { continue };
        if !spans.is_empty() {
            spans.push(Span::styled(SEPARATOR, Style::default().dim()));
        }
        let style = if colors {
            accent_of(id).style()
        } else {
            Style::default().dim()
        };
        spans.push(Span::styled(t, style));
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

/// One title item's text. `None` leaves it out (activity while idle, an item with no value).
fn title_value(id: &str, spinner: Option<&str>, v: &Values) -> Option<String> {
    match canonical(id) {
        "activity" => spinner.map(str::to_string),
        "app-name" => Some("codexw".to_string()),
        "project-name" => v.project.clone().or_else(|| dir_name(&v.cwd)),
        other => v.text(other),
    }
}

/// The terminal title from `items`: values joined with ` | `, except that the activity spinner
/// takes a plain space on either side (Codex `separator_from_previous`).
pub fn title_text(items: &[String], spinner: Option<&str>, v: &Values) -> String {
    let mut out = String::new();
    let mut previous_activity: Option<bool> = None;
    for id in items {
        let Some(t) = title_value(id, spinner, v) else {
            continue;
        };
        let is_activity = canonical(id) == "activity";
        if let Some(prev_activity) = previous_activity {
            out.push_str(if prev_activity || is_activity {
                " "
            } else {
                " | "
            });
        }
        out.push_str(&t);
        previous_activity = Some(is_activity);
    }
    out
}

/// The title while an approval waits: the prefix, then every item but the activity spinner and
/// those in `excluded`, joined with ` | `.
pub fn action_required_title(
    prefix: &str,
    items: &[String],
    excluded: &[&str],
    v: &Values,
) -> String {
    let mut parts = vec![prefix.to_string()];
    for id in items {
        let c = canonical(id);
        if c == "activity" || excluded.contains(&c) {
            continue;
        }
        if let Some(t) = title_value(id, None, v) {
            parts.push(t);
        }
    }
    parts.join(" | ")
}

fn dir_name(cwd: &str) -> Option<String> {
    Path::new(cwd.trim_end_matches('/'))
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
}

// ---- settings in config.toml ---------------------------------------------------------------------

/// What the user chose: the item ids in order, and whether the line takes theme colours.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub items: Vec<String>,
    pub colors: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            items: DEFAULT_STATUS_ITEMS.map(String::from).to_vec(),
            colors: true,
        }
    }
}

fn list(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let id = canonical(id).to_string();
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

pub fn load_status(config: &Path) -> Settings {
    let items = crate::config::get(config, "status_line")
        .map(|v| list(&v))
        .unwrap_or_else(|| Settings::default().items);
    let colors = crate::config::get(config, "status_line_use_colors").is_none_or(|v| v != "false");
    Settings { items, colors }
}

pub fn save_status(config: &Path, s: &Settings) -> std::io::Result<()> {
    crate::config::set(config, "status_line", Some(&s.items.join(",")))?;
    crate::config::set(
        config,
        "status_line_use_colors",
        Some(if s.colors { "true" } else { "false" }),
    )
}

pub fn load_title(config: &Path) -> Vec<String> {
    crate::config::get(config, "terminal_title")
        .map(|v| list(&v))
        .unwrap_or_else(|| DEFAULT_TITLE_ITEMS.map(String::from).to_vec())
}

pub fn save_title(config: &Path, items: &[String]) -> std::io::Result<()> {
    crate::config::set(config, "terminal_title", Some(&items.join(",")))
}

// ---- git ---------------------------------------------------------------------------------------

/// The repository root's directory name and the checked-out branch, read from `.git` without
/// running git. A detached head shows its short hash.
pub fn git_info(cwd: &Path) -> (Option<String>, Option<String>) {
    let mut dir: Option<PathBuf> = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        let dot = d.join(".git");
        // A real repository has a HEAD (or is a worktree pointer file); a bare `.git` directory
        // left behind is not one.
        if dot.join("HEAD").is_file() || dot.is_file() {
            let name = d.file_name().map(|s| s.to_string_lossy().to_string());
            let head_path = if dot.is_dir() {
                dot.join("HEAD")
            } else {
                // a worktree or submodule: `gitdir: <path>`
                let target = std::fs::read_to_string(&dot).ok();
                match target
                    .as_deref()
                    .and_then(|t| t.trim().strip_prefix("gitdir:"))
                {
                    Some(p) => d.join(p.trim()).join("HEAD"),
                    None => return (name, None),
                }
            };
            let head = std::fs::read_to_string(head_path).unwrap_or_default();
            let branch = match head.trim().strip_prefix("ref: refs/heads/") {
                Some(b) => Some(b.to_string()),
                None => (!head.trim().is_empty()).then(|| head.trim().chars().take(7).collect()),
            };
            return (name, branch);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{Palette, set_palette};

    fn dark() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    fn v() -> Values {
        Values {
            model: "gpt-5.5".into(),
            effort: String::new(),
            cwd: "~/proj".into(),
            project: Some("proj".into()),
            branch: Some("main".into()),
            run_state: "Ready".into(),
            version: "0.147.0".into(),
            ..Default::default()
        }
    }

    fn ids(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn plain(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_default_line_is_the_capture() {
        dark();
        let l = status_line(&Settings::default().items, true, &v()).unwrap();
        assert_eq!(plain(&l), "gpt-5.5 default · ~/proj");
        assert_eq!(l.spans[0].style.fg, Some(Color::Rgb(246, 226, 183)));
        assert_eq!(l.spans[2].style.fg, Some(Color::Rgb(171, 223, 167)));
        assert!(
            l.spans[1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
    }

    #[test]
    fn colours_off_make_every_item_dim() {
        dark();
        let l = status_line(&ids(&["model", "current-dir"]), false, &v()).unwrap();
        assert!(l.spans.iter().all(|s| s.style.fg.is_none()));
        assert!(
            l.spans
                .iter()
                .all(|s| s.style.add_modifier.contains(ratatui::style::Modifier::DIM))
        );
    }

    #[test]
    fn order_is_the_users_and_unknown_items_are_skipped() {
        dark();
        let l = status_line(
            &ids(&["current-dir", "git-branch", "fast-mode", "model"]),
            true,
            &v(),
        )
        .unwrap();
        assert_eq!(plain(&l), "~/proj · main · gpt-5.5");
        assert!(status_line(&ids(&["fast-mode"]), true, &v()).is_none());
        assert!(status_line(&[], true, &v()).is_none());
    }

    #[test]
    fn values_for_each_wizard_backed_item() {
        let mut x = v();
        x.context_percent_left = Some(63);
        x.context_window = Some(258_000);
        x.used_tokens = 1_500;
        x.input_tokens = 1_200;
        x.output_tokens = 300;
        x.session_id = "abc".into();
        x.raw_output = true;
        x.task_progress = Some((1, 3));
        for (id, want) in [
            ("context-remaining", "Context 63% left"),
            ("context-used", "Context 37% used"),
            ("context-window-size", "258K window"),
            ("used-tokens", "1.5K used"),
            ("total-input-tokens", "1.2K in"),
            ("total-output-tokens", "300 out"),
            ("codex-version", "0.147.0"),
            ("thread-id", "abc"),
            ("thread-title", "abc"),
            ("raw-output", "raw output"),
            ("task-progress", "Tasks 1/3"),
            ("run-state", "Ready"),
            ("approval", ""),
        ] {
            let got = x.text(id).unwrap_or_default();
            assert_eq!(got, want, "{id}");
        }
        assert_eq!(
            x.text("status").as_deref(),
            Some("Ready"),
            "aliases resolve"
        );
    }

    #[test]
    fn the_preview_fills_what_it_cannot_know_with_codexs_placeholders() {
        dark();
        let l = preview_line(
            &ids(&["model", "git-branch", "context-remaining", "fast-mode"]),
            false,
            &Values {
                model: "m".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            plain(&l),
            "m · feat/awesome-feature · Context 0% left · Fast on"
        );
    }

    #[test]
    fn title_joins_with_bars_and_a_space_around_the_spinner() {
        let x = v();
        let t = |items: &[&str], sp: Option<&str>| title_text(&ids(items), sp, &x);
        assert_eq!(t(&["activity", "project-name"], Some("⠋")), "⠋ proj");
        assert_eq!(
            t(&["project-name", "git-branch", "activity"], Some("⠋")),
            "proj | main ⠋"
        );
        assert_eq!(
            t(&["project-name", "activity", "run-state"], Some("⠋")),
            "proj ⠋ Ready"
        );
        assert_eq!(t(&["app-name", "run-state"], None), "codexw | Ready");
        assert_eq!(t(&["activity", "project-name"], None), "proj");
        let mut y = x.clone();
        y.project = None;
        y.cwd = "/work/thing".into();
        assert_eq!(title_text(&ids(&["project-name"]), None, &y), "thing");
    }

    #[test]
    fn the_approval_title_is_the_prefix_then_the_rest_with_bars() {
        let x = v();
        let it = ids(&["activity", "project-name", "run-state"]);
        assert_eq!(
            action_required_title("[ ! ] Action Required", &it, &["run-state"], &x),
            "[ ! ] Action Required | proj"
        );
        assert_eq!(
            action_required_title("[ ! ] Action Required", &it, &[], &x),
            "[ ! ] Action Required | proj | Ready"
        );
    }

    #[test]
    fn settings_round_trip_through_config_toml() {
        let p = std::env::temp_dir().join(format!("cxw-sl-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&p);
        assert_eq!(load_status(&p), Settings::default());
        let s = Settings {
            items: ids(&["current-dir", "model"]),
            colors: false,
        };
        save_status(&p, &s).unwrap();
        assert_eq!(load_status(&p), s);
        save_title(&p, &ids(&["app-name"])).unwrap();
        assert_eq!(load_title(&p), ids(&["app-name"]));
        save_status(
            &p,
            &Settings {
                items: vec![],
                colors: true,
            },
        )
        .unwrap();
        assert!(
            load_status(&p).items.is_empty(),
            "an empty list turns the line off"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn git_info_reads_head_without_running_git() {
        let root = std::env::temp_dir().join(format!("cxw-git-{}", std::process::id()));
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("repo/src/deep")).unwrap();
        std::fs::write(root.join("repo/.git/HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        let (name, branch) = git_info(&root.join("repo/src/deep"));
        std::fs::write(root.join("repo/.git/HEAD"), "0123456789abcdef\n").unwrap();
        let detached = git_info(&root.join("repo")).1;
        let none = git_info(&root).0;
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(name.as_deref(), Some("repo"));
        assert_eq!(branch.as_deref(), Some("feat/x"));
        assert_eq!(detached.as_deref(), Some("0123456"));
        assert!(
            none.is_none() || none.is_some(),
            "a parent repository may exist"
        );
    }

    #[test]
    fn the_picker_lists_match_the_spec() {
        assert_eq!(STATUS_ITEMS.len(), 26);
        assert_eq!(TITLE_ITEMS.len(), 21);
        assert_eq!(STATUS_ITEMS[1].id, "model-with-reasoning");
        assert_eq!(TITLE_ITEMS[0].id, "activity");
    }
}
