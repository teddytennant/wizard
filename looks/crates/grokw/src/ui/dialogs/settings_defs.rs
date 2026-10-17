use super::settings::{Cat, Choice, Def, Kind};

// Generated from the settings catalog of Grok Build 1.0.24 (xai-grok-pager/src/settings/defs.rs):
// labels, descriptions, search keywords and choice lists are the shipped strings.
// Only the rows grokw can show are kept; see `settings.rs` for which of them do anything.

pub const CONCRETE_THEME_CHOICES: &[Choice] = &[
    Choice {
        canon: "groknight",
        name: "Grok Night",
        desc: "Neutral dark with magenta accent.",
    },
    Choice {
        canon: "grokday",
        name: "Grok Day",
        desc: "Light theme for bright environments.",
    },
    Choice {
        canon: "tokyonight",
        name: "Tokyo Night",
        desc: "Dark + blue-tinted; needs truecolor.",
    },
    Choice {
        canon: "rosepine-moon",
        name: "Rose Pine Moon",
        desc: "Muted dark with mauve accents; needs truecolor.",
    },
    Choice {
        canon: "oscura-midnight",
        name: "Oscura Midnight",
        desc: "Deep dark with warm accents; needs truecolor.",
    },
    Choice {
        canon: "terminal",
        name: "Terminal",
        desc: "Terminal's own background and text colors.",
    },
];

pub const FOLLOW_UP_BEHAVIOR_CHOICES: &[Choice] = &[
    Choice {
        canon: "queue",
        name: "Queue",
        desc: "Hold follow-ups until the current turn finishes.",
    },
    Choice {
        canon: "steer",
        name: "Steer",
        desc: "Inject follow-ups mid-turn at the next tool or model step.",
    },
];

pub const RENDER_MERMAID_CHOICES: &[Choice] = &[
    Choice {
        canon: "auto",
        name: "Auto",
        desc: "Show diagrams with a clickable row to open/copy the rendered image.",
    },
    Choice {
        canon: "on",
        name: "On",
        desc: "Same as auto: always show the clickable affordance row.",
    },
    Choice {
        canon: "off",
        name: "Off",
        desc: "Always show the raw Mermaid source as a code block.",
    },
];

pub const SCREEN_MODE_CHOICES: &[Choice] = &[
    Choice {
        canon: "fullscreen",
        name: "Fullscreen",
        desc: "Open plain grok in the standard fullscreen TUI. Default when unset.",
    },
    Choice {
        canon: "minimal",
        name: "Minimal",
        desc: "Open plain grok in scrollback-native (minimal) mode.",
    },
];

pub const SCROLL_MODE_CHOICES: &[Choice] = &[
    Choice {
        canon: "auto",
        name: "Auto-detect",
        desc: "Detect wheel vs trackpad per gesture from event timing. Default.",
    },
    Choice {
        canon: "wheel",
        name: "Mouse wheel",
        desc: "Always treat scrolling as wheel notches (fixed lines per tick).",
    },
    Choice {
        canon: "trackpad",
        name: "Trackpad",
        desc: "Always treat scrolling as a trackpad (fractional accumulation).",
    },
];

pub const TEXT_SELECTION_CHOICES: &[Choice] = &[
    Choice { canon: "flash", name: "Flash after copy", desc: "Brief highlight on mouse-up, then clear. Double-click toggles fold. Default." },
    Choice { canon: "hold", name: "Hold until dismissed", desc: "Keep the selection visible until Esc, click, or scroll. Double-click toggles fold." },
    Choice { canon: "word", name: "Word select (terminal-like)", desc: "Double-click selects & copies a word, triple-click a paragraph; selection stays until dismissed." },
];

pub const THEME_CHOICES: &[Choice] = &[
    Choice {
        canon: "auto",
        name: "Auto",
        desc: "Follow system dark/light appearance.",
    },
    Choice {
        canon: "groknight",
        name: "Grok Night",
        desc: "Neutral dark with magenta accent.",
    },
    Choice {
        canon: "grokday",
        name: "Grok Day",
        desc: "Light theme for bright environments.",
    },
    Choice {
        canon: "tokyonight",
        name: "Tokyo Night",
        desc: "Dark + blue-tinted; needs truecolor.",
    },
    Choice {
        canon: "rosepine-moon",
        name: "Rose Pine Moon",
        desc: "Muted dark with mauve accents; needs truecolor.",
    },
    Choice {
        canon: "oscura-midnight",
        name: "Oscura Midnight",
        desc: "Deep dark with warm accents; needs truecolor.",
    },
    Choice {
        canon: "terminal",
        name: "Terminal",
        desc: "Terminal's own background and text colors.",
    },
];

pub const DEFS: &[Def] = &[
    Def { key: "compact_mode", cat: Cat::Appearance, label: "Compact mode", desc: "Reduce padding around messages for more content density. Auto-enabled while the terminal is 20 rows or shorter.", keywords: &["compact", "density", "padding", "tight", "small", "screen", "auto"], kind: Kind::Bool(false), restart: false },
    Def { key: "screen_mode", cat: Cat::Appearance, label: "Default screen mode", desc: "How plain grok opens next time: Fullscreen (default when unset) or Minimal. Writes [ui] screen_mode in config.toml. Restart required. Switch this session only with /minimal or /fullscreen.", keywords: &["screen", "mode", "minimal", "fullscreen", "full", "scrollback", "native", "alt-screen", "render", "default"], kind: Kind::Enum { default: "fullscreen", choices: SCREEN_MODE_CHOICES, preview: false }, restart: true },
    Def { key: "show_timestamps", cat: Cat::Appearance, label: "Show timestamps", desc: "Show clock time next to user messages and agent responses.", keywords: &["timestamps", "time", "clock", "date"], kind: Kind::Bool(true), restart: false },
    Def { key: "show_timeline", cat: Cat::Appearance, label: "Timeline sidebar", desc: "Per-turn tick rail in place of the scrollbar: hover previews a turn, click jumps to it.", keywords: &["timeline", "sidebar", "ticks", "turns", "navigator", "rail"], kind: Kind::Bool(false), restart: false },
    Def { key: "page_flip_on_send", cat: Cat::Appearance, label: "Snap prompt to top on send", desc: "When you send a prompt, scroll it to the top of the screen so the response starts on a fresh page (default). Turn off to leave the scroll position unchanged when you send.", keywords: &["page", "flip", "send", "prompt", "scroll", "top", "jump", "auto", "snap"], kind: Kind::Bool(true), restart: false },
    Def { key: "combine_queued_prompts", cat: Cat::Editor, label: "Combine queued prompts", desc: "Merge consecutive plain follow-ups into one model turn (TUI shows one bubble each). Stops at bash, slash commands, cron, expanded skills, image follow-ups, or a row under edit. Default off; applies on local drain and shell promote.", keywords: &["queue", "combine", "batch", "follow-up", "merge", "pending"], kind: Kind::Bool(false), restart: false },
    Def { key: "follow_up_behavior", cat: Cat::Editor, label: "Follow-up behavior", desc: "What to do with messages you send while a turn is running. Queue waits for the turn to finish; Steer injects them mid-turn at the next tool batch or model step. Default: Queue.", keywords: &["queue", "steer", "interject", "follow-up", "followup", "send", "immediate"], kind: Kind::Enum { default: "queue", choices: FOLLOW_UP_BEHAVIOR_CHOICES, preview: false }, restart: false },
    Def { key: "confirm_before_rewind", cat: Cat::Editor, label: "Confirm before rewind", desc: "Ask before rewinding conversation history. Turn off to rewind immediately when you pick a turn.", keywords: &["rewind", "confirm", "undo", "history", "ask", "prompt"], kind: Kind::Bool(true), restart: false },
    Def { key: "simple_mode", cat: Cat::Appearance, label: "Disable vim input mode", desc: "Use plain readline-style input instead of vim keys in the prompt. Experimental.", keywords: &["simple", "ascii", "minimal", "plain", "vim", "readline", "experimental", "editor", "input", "prompt"], kind: Kind::Bool(true), restart: false },
    Def { key: "vim_mode", cat: Cat::Appearance, label: "Vim scrollback navigation", desc: "Enable vim keys (h/j/k/l, gg/G, /) for navigating the scrollback. Does not affect the input prompt.", keywords: &["vim", "scrollback", "navigation", "hjkl", "keys", "keybindings", "scroll"], kind: Kind::Bool(false), restart: false },
    Def { key: "theme", cat: Cat::Appearance, label: "Theme", desc: "Color theme for the pager UI.", keywords: &["theme", "color", "colour", "palette", "appearance", "dark", "light"], kind: Kind::Enum { default: "groknight", choices: THEME_CHOICES, preview: true }, restart: false },
    Def { key: "auto_dark_theme", cat: Cat::Appearance, label: "Auto dark theme", desc: "Theme to use when the system is in dark mode (only with theme=auto).", keywords: &["auto", "dark", "theme", "system", "appearance", "night"], kind: Kind::Enum { default: "groknight", choices: CONCRETE_THEME_CHOICES, preview: true }, restart: false },
    Def { key: "auto_light_theme", cat: Cat::Appearance, label: "Auto light theme", desc: "Theme to use when the system is in light mode (only with theme=auto).", keywords: &["auto", "light", "theme", "system", "appearance", "day"], kind: Kind::Enum { default: "grokday", choices: CONCRETE_THEME_CHOICES, preview: true }, restart: false },
    Def { key: "render_mermaid", cat: Cat::Appearance, label: "Render Mermaid diagrams", desc: "How ```mermaid code blocks are shown: auto/on add a clickable row to open the rendered diagram; off shows the raw source.", keywords: &["mermaid", "diagram", "diagrams", "render", "flowchart", "graph", "chart"], kind: Kind::Enum { default: "auto", choices: RENDER_MERMAID_CHOICES, preview: false }, restart: false },
    Def { key: "multiline_mode", cat: Cat::Editor, label: "Multiline", desc: "When on, Enter inserts a newline and Shift+Enter sends. Resets each session.", keywords: &["multiline", "newline", "input", "editor", "enter"], kind: Kind::Bool(false), restart: false },
    Def { key: "max_thoughts_width", cat: Cat::Appearance, label: "Max thoughts width", desc: "Column width budget for the agent's thoughts panel (40-500, default 120).", keywords: &["thoughts", "width", "max", "thinking", "panel", "reasoning", "columns"], kind: Kind::Int { default: 120, min: 40, max: 500 }, restart: false },
    Def { key: "show_thinking_blocks", cat: Cat::Appearance, label: "Show thinking blocks", desc: "Show agent thinking/reasoning blocks in the scrollback while streaming.", keywords: &["thinking", "reasoning", "thoughts", "blocks", "show", "hide"], kind: Kind::Bool(true), restart: false },
    Def { key: "prompt_suggestions", cat: Cat::Editor, label: "Prompt suggestions", desc: "After each turn, predict your likely next prompt and show it as ghost text in the input (Tab to accept). Uses a small model call per turn.", keywords: &["prompt", "suggestion", "suggestions", "autocomplete", "ghost", "tab", "predict", "next"], kind: Kind::Bool(true), restart: false },
    Def { key: "respect_manual_folds", cat: Cat::Appearance, label: "Respect manual folds", desc: "Keep manually folded blocks as-is while streaming and stop auto-scroll when expanding a block. Experimental.", keywords: &["fold", "pin", "collapse", "expand", "thinking", "follow", "scroll"], kind: Kind::Bool(false), restart: false },
    Def { key: "group_tool_verbs", cat: Cat::Appearance, label: "Group tool calls", desc: "Fold consecutive read/search/list tool calls and subagent rows into one summary row; finished thoughts fold into the group too.", keywords: &["group", "tool", "verbs", "fold", "collapse", "read", "search", "summary", "thinking", "subagent"], kind: Kind::Bool(true), restart: false },
    Def { key: "collapsed_edit_blocks", cat: Cat::Appearance, label: "Collapsed edit blocks", desc: "Show edits as one-line +N/-M diffstat summaries and merge back-to-back edits to the same file into one block; expand a row to see the diffs.", keywords: &["edit", "edits", "diff", "diffstat", "collapse", "collapsed", "summary", "expand", "one-line", "merge", "coalesce"], kind: Kind::Bool(false), restart: false },
    Def { key: "display_refresh_auto_cadence", cat: Cat::Appearance, label: "Match display refresh rate", desc: "On high-refresh displays, the TUI will stream/scroll faster to match the display. Off keeps the classic ~60 Hz cadence. Restart required.", keywords: &["display", "refresh", "rate", "hz", "cadence", "fps", "smooth", "scroll", "stream", "high", "120", "144"], kind: Kind::Bool(true), restart: true },
    Def { key: "scroll_speed", cat: Cat::Mouse, label: "Scroll speed", desc: "Mouse-wheel and trackpad scroll speed multiplier (1-100). Higher = faster.", keywords: &["scroll", "speed", "mouse", "wheel", "trackpad", "fast", "slow"], kind: Kind::Int { default: 50, min: 1, max: 100 }, restart: false },
    Def { key: "scroll_mode", cat: Cat::Mouse, label: "Scroll input", desc: "Force wheel or trackpad scroll behavior when auto-detection misreads your device.", keywords: &["scroll", "mode", "wheel", "trackpad", "mouse", "detect", "force", "input"], kind: Kind::Enum { default: "auto", choices: SCROLL_MODE_CHOICES, preview: false }, restart: false },
    Def { key: "scroll_lines", cat: Cat::Mouse, label: "Scroll lines", desc: "Lines per scroll tick for both wheel and trackpad (1-10). Until set, each terminal's own profile applies.", keywords: &["scroll", "lines", "tick", "notch", "wheel", "trackpad", "mouse"], kind: Kind::Int { default: 3, min: 1, max: 10 }, restart: false },
    Def { key: "invert_scroll", cat: Cat::Mouse, label: "Invert scroll", desc: "Reverse vertical scroll direction (natural scrolling).", keywords: &["invert", "scroll", "natural", "direction", "reverse", "mouse", "trackpad"], kind: Kind::Bool(false), restart: false },
    Def { key: "keep_text_selection", cat: Cat::Mouse, label: "Text selection", desc: "How long in-app selection stays on screen and what double-click does (fold vs. select & copy a word). For your terminal or multiplexer's own selection, hold Shift while dragging (native copy).", keywords: &["selection", "drag", "copy", "flash", "hold", "shift", "native", "mouse", "tmux", "double", "double-click", "word", "terminal"], kind: Kind::Enum { default: "flash", choices: TEXT_SELECTION_CHOICES, preview: false }, restart: false },
    Def { key: "show_tips", cat: Cat::Advanced, label: "Show tips", desc: "Show the tip-of-the-day banner on startup. Restart required.", keywords: &["tips", "tip", "show", "banner", "welcome", "startup", "launch"], kind: Kind::Bool(true), restart: true },
    Def { key: "contextual_hints", cat: Cat::Advanced, label: "Show contextual hints", desc: "Show brief, in-context keyboard hints as you work; toggle each one individually.", keywords: &["contextual", "hints", "tips", "undo", "plan", "nudge", "image", "clipboard", "ephemeral", "send", "interject", "queue", "ctrl+z", "draft", "wipe", "mode", "shift+tab", "paste", "input", "enter", "follow-up", "small", "screen", "compact", "ssh", "wrap", "remote"], kind: Kind::Group(HINT_CHILDREN), restart: false },
];

/// Rows inside the `Show contextual hints` sheet.
pub const HINT_CHILDREN: &[Def] = &[
    Def { key: "contextual_hints.undo", cat: Cat::Advanced, label: "Undo", desc: "Remind you that Ctrl+Z restores the prompt after you clear it.", keywords: &["undo", "ctrl+z", "draft", "wipe", "hint"], kind: Kind::Bool(true), restart: false },
    Def { key: "contextual_hints.plan_mode", cat: Cat::Advanced, label: "Plan mode", desc: "Suggest plan mode (Shift+Tab) when your prompt looks like a planning request.", keywords: &["plan", "mode", "nudge", "shift+tab", "hint"], kind: Kind::Bool(true), restart: false },
    Def { key: "contextual_hints.image_input", cat: Cat::Advanced, label: "Image input", desc: "Offer to paste an image when one is on the clipboard and the model accepts images.", keywords: &["image", "clipboard", "paste", "input", "hint"], kind: Kind::Bool(true), restart: false },
    Def { key: "contextual_hints.send_now", cat: Cat::Advanced, label: "Send now", desc: "After you queue a follow-up mid-turn, remind you that Enter on an empty prompt sends the top queued item now.", keywords: &["send", "now", "interject", "queue", "follow-up", "enter", "empty", "hint"], kind: Kind::Bool(true), restart: false },
    Def { key: "contextual_hints.small_screen", cat: Cat::Advanced, label: "Small screen", desc: "Suggest /compact-mode once per run when the terminal is short on rows.", keywords: &["small", "screen", "compact", "space", "rows", "hint"], kind: Kind::Bool(true), restart: false },
    Def { key: "contextual_hints.word_select", cat: Cat::Advanced, label: "Word select", desc: "After double-clicking conversation text while Text selection is fold/nav, remind you that Word select lives in Settings.", keywords: &["word", "select", "double", "double-click", "click", "fold", "selection", "settings", "hint"], kind: Kind::Bool(true), restart: false },
    Def { key: "contextual_hints.export_copy", cat: Cat::Advanced, label: "Copy and export", desc: "After three nearby drag-copies of conversation text, remind you that /copy and /export exist.", keywords: &["copy", "export", "transcript", "clipboard", "hint"], kind: Kind::Bool(true), restart: false },
    Def { key: "contextual_hints.ssh_wrap", cat: Cat::Advanced, label: "SSH wrap", desc: "Show a `/doctor` tip when an SSH session is not using `grok wrap`.", keywords: &["ssh", "wrap", "remote", "clipboard", "restore", "startup", "hint"], kind: Kind::Bool(true), restart: false },
];
