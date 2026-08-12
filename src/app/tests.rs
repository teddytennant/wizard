use super::*;

use crate::agent::{ImageSource, InterviewQuestion};
use crate::commands::{FusionAction, ServerAction};
use crate::images::ImageRef;

use super::command::{git_diff_text, is_wizard_state_path};
use super::transcript::{collapse_long, scroll_step};
use crate::transcript::TranscriptItem;

/// Replay `messages` into `app` the way `/resume` does — through session
/// records, which is the only shape the replay door takes.
fn replay(app: &mut App, messages: Vec<crate::llm::ChatMessage>) {
    app.load_transcript(&records(messages));
}

/// Stored session records for `messages`, with the timestamps a real file
/// would have carried.
fn records(messages: Vec<crate::llm::ChatMessage>) -> Vec<crate::agent::session::SessionEntry> {
    messages
        .into_iter()
        .map(|message| {
            crate::agent::session::SessionEntry::Message(crate::agent::session::SessionRecord {
                timestamp: chrono::Utc::now(),
                message,
                system_note: false,
            })
        })
        .collect()
}

fn app() -> App {
    App::new(Config::default())
}

fn press(app: &mut App, code: KeyCode) -> Option<AppAction> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        .expect("key handled")
}

fn press_ctrl(app: &mut App, c: char) -> Option<AppAction> {
    app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
        .expect("key handled")
}

/// An app with `n` subagent runs on the rail, all still running.
fn app_with_panes(n: u64) -> App {
    let mut app = app();
    for i in 0..n {
        app.handle_agent_event(AgentEvent::SubagentRunStarted {
            run: i,
            bg: Some(i as u32),
            name: format!("agent{i}"),
            task: format!("task {i}"),
        });
    }
    app
}

fn press_mod(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Option<AppAction> {
    app.handle_key(KeyEvent::new(code, mods))
        .expect("key handled")
}

fn type_str(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

/// Untracked (new) files are invisible to plain `git diff`, so `/diff`
/// must surface them itself — otherwise a tree whose only change is a new
/// file reads as "(working tree clean)".
#[tokio::test]
async fn diff_text_includes_untracked_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
    };
    run(&["init"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(root.join("brand_new.txt"), "fresh content\n").expect("write");

    let text = git_diff_text(root).await.expect("diff text");
    assert!(
        text.contains("brand_new.txt") && text.contains("fresh content"),
        "untracked file missing from /diff output:\n{text}"
    );
    assert!(text.contains("# --- untracked ---"));
}

/// Wizard's own `.wizard/` session state (checkpoints, snapshots) is an
/// implementation detail — it must never show up in `/diff`, or the
/// sidebar fills with internal noise and looks broken.
#[tokio::test]
async fn diff_text_omits_wizard_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
    };
    run(&["init"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::create_dir_all(root.join(".wizard/checkpoints/1")).expect("mkdir");
    std::fs::write(root.join(".wizard/checkpoints/1/0.snap"), "internal\n").expect("write");
    std::fs::write(root.join("real_change.txt"), "user content\n").expect("write");

    let text = git_diff_text(root).await.expect("diff text");
    assert!(
        text.contains("real_change.txt"),
        "real untracked change missing:\n{text}"
    );
    assert!(
        !text.contains(".wizard/checkpoints"),
        "wizard internal state leaked into /diff:\n{text}"
    );
}

#[test]
fn is_wizard_state_path_matches_state_dir_only() {
    assert!(is_wizard_state_path(".wizard/checkpoints/1/0.snap"));
    assert!(is_wizard_state_path("sub/.wizard/x"));
    assert!(is_wizard_state_path(".wizard"));
    assert!(!is_wizard_state_path("src/wizard.rs"));
    assert!(!is_wizard_state_path(".wizardrc"));
}

/// The diff sidebar paginates with PgUp/PgDn (offset from the top) and Esc
/// closes it — without this a diff taller than the pane is unreadable.
#[test]
fn diff_sidebar_pages_and_closes() {
    let mut app = app();
    app.diff = Some(DiffPane::default());
    let scroll = |app: &App| app.diff.as_ref().expect("the sidebar is open").scroll;
    assert_eq!(scroll(&app), 0);

    press(&mut app, KeyCode::PageDown);
    assert_eq!(scroll(&app), 10, "PgDn scrolls the diff down");
    press(&mut app, KeyCode::PageUp);
    assert_eq!(scroll(&app), 0, "PgUp scrolls back up");
    // PgUp at the top stays clamped (no underflow).
    press(&mut app, KeyCode::PageUp);
    assert_eq!(scroll(&app), 0);

    // While the diff owns paging, the transcript scroll is untouched.
    assert_eq!(app.transcript.scroll, 0);

    app.diff.as_mut().expect("open").scroll = 30;
    press(&mut app, KeyCode::Esc);
    assert!(app.diff.is_none(), "Esc closes the diff sidebar");
}

#[test]
fn welcome_stays_up_for_empty_and_notice_only_transcripts() {
    let mut app = app();
    // Fresh launch: nothing typed, welcome screen.
    assert!(!app.has_conversation());

    // Early system notices (provider health, partial MCP failure) land
    // before the first message; they alone must not dismiss the welcome.
    app.notice("error: 1 of 2 MCP servers failed to connect (see logs)");
    app.notice("just a status line");
    assert!(
        !app.has_conversation(),
        "notices alone should not count as conversation"
    );
}

#[test]
fn slash_command_dismisses_the_welcome_screen() {
    let mut app = app();
    assert!(app.welcome_visible());

    // Startup notices land before anything is submitted; they alone must
    // leave the welcome screen up.
    app.notice("error: 1 of 2 MCP servers failed to connect (see logs)");
    assert!(app.welcome_visible());

    // A slash command dispatches without adding transcript entries, but
    // it still begins the session.
    type_str(&mut app, "/effort high");
    press(&mut app, KeyCode::Enter);
    assert!(
        !app.welcome_visible(),
        "a slash command dismisses the welcome screen"
    );
}

#[test]
fn welcome_dismisses_once_real_entries_appear() {
    for event in [
        AgentEvent::TextDelta("hello".to_string()),
        AgentEvent::ToolStarted {
            name: "read".to_string(),
            args: serde_json::json!({}),
        },
    ] {
        let mut app = app();
        app.handle_agent_event(event);
        app.transcript.commit();
        assert!(
            app.has_conversation(),
            "a reply or a tool call begins the conversation"
        );
    }
    // And so does a prompt.
    let mut app = app();
    app.transcript.user("hi".to_string(), Vec::new());
    assert!(app.has_conversation());
}

#[test]
fn spinner_verb_starts_from_the_default_list() {
    let app = app();
    assert!(crate::config::UiConfig::DEFAULT_SPINNER_VERBS.contains(&app.spinner_verb.as_str()));
}

#[test]
fn spinner_verb_is_deterministic_and_stable_within_a_busy_period() {
    let config = Config {
        ui: crate::config::UiConfig {
            spinner_verbs: vec![
                "Pondering".to_string(),
                "Musing".to_string(),
                "Noodling".to_string(),
            ],
            vim: false,
            skin: None,
        },
        ..Config::default()
    };
    let mut a = App::new(config.clone());
    let mut b = App::new(config);
    a.tick = 17;
    b.tick = 17;
    a.roll_spinner_verb();
    b.roll_spinner_verb();
    // Same tick and roll count -> same verb.
    assert_eq!(a.spinner_verb, b.spinner_verb);
    // Ticks advancing mid-turn must not change the verb until a re-roll.
    let during = a.spinner_verb.clone();
    a.tick += 5;
    assert_eq!(a.spinner_verb, during);
}

#[test]
fn spinner_verb_rerolls_across_busy_periods() {
    let mut app = app();
    let mut seen = std::collections::HashSet::new();
    for turn in 0..40u64 {
        app.tick = turn * 13;
        app.roll_spinner_verb();
        seen.insert(app.spinner_verb.clone());
    }
    assert!(seen.len() > 1, "verb never varied across busy periods");
}

#[test]
fn slash_filters_suggestions_by_prefix() {
    let mut app = app();
    type_str(&mut app, "/mo");
    let names: Vec<&str> = app.suggestions.iter().map(|s| s.name.as_str()).collect();
    // Prefix matches first, then substring matches ("me*mo*ry").
    assert_eq!(names, ["model", "mode", "memory"]);
    assert_eq!(app.input_mode, InputMode::Command);
}

#[test]
fn suggestions_hide_once_args_are_typed() {
    let mut app = app();
    type_str(&mut app, "/evolve add");
    assert!(app.suggestions.is_empty());
}

#[test]
fn arrow_keys_cycle_suggestions_with_wraparound() {
    let mut app = app();
    type_str(&mut app, "/mo");
    assert_eq!(app.suggestion_index, 0);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.suggestion_index, 1);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.suggestion_index, 2);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.suggestion_index, 0);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.suggestion_index, 2);
}

#[test]
fn tab_completes_the_selected_suggestion() {
    let mut app = app();
    // "/re" would be ambiguous between /rewind and /reload.
    type_str(&mut app, "/rel");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "/reload");
    assert_eq!(app.cursor, "/reload".chars().count());
}

#[test]
fn tab_completion_appends_space_for_commands_taking_args() {
    let mut app = app();
    type_str(&mut app, "/ev");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "/evolve ");
}

#[test]
fn enter_completes_and_runs_argless_commands() {
    let mut app = app();
    type_str(&mut app, "/d");
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Diff))
    ));
    assert!(app.input.is_empty());
}

#[test]
fn enter_on_partial_arg_command_completes_and_waits() {
    let mut app = app();
    type_str(&mut app, "/ev");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert_eq!(app.input, "/evolve ");
}

#[test]
fn exactly_typed_command_wins_over_longer_completion() {
    // "model" prefix-matches the typed "mode"; Enter must still run
    // /mode itself, not complete to /model.
    let mut app = app();
    type_str(&mut app, "/mode");
    assert_eq!(app.suggestions[0].name, "mode");
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Mode(None)))
    ));
}

fn custom(name: &str, template: &str, description: Option<&str>) -> CustomCommand {
    CustomCommand {
        name: name.to_string(),
        description: description.map(str::to_string),
        template: template.to_string(),
        path: PathBuf::new(),
    }
}

#[test]
fn custom_commands_appear_in_suggestions_after_builtins() {
    let mut app = app();
    app.custom_commands = vec![custom(
        "models-report",
        "Report on $ARGUMENTS",
        Some("report"),
    )];
    type_str(&mut app, "/mo");
    let names: Vec<&str> = app.suggestions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["model", "mode", "models-report", "memory"]);
    let spec = &app.suggestions[2];
    assert_eq!(spec.description, "report");
    assert!(spec.takes_args);
}

#[test]
fn typed_custom_command_submits_the_expanded_prompt() {
    let mut app = app();
    app.custom_commands = vec![custom("review", "Review $1 with care.", None)];
    type_str(&mut app, "/review src/app.rs");
    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Submit(prepared)) = action else {
        panic!("expected a submit, got {action:?}");
    };
    assert_eq!(prepared.text, "Review src/app.rs with care.");
    // The transcript shows what the user actually typed.
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::User { text, .. }) if text == "/review src/app.rs"
    ));
}

#[test]
fn unknown_slash_command_passes_through_as_a_prompt() {
    let mut app = app();
    type_str(&mut app, "/frobnicate the build");
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Submit(prepared)) if prepared.text == "/frobnicate the build"
    ));
}

#[test]
fn builtin_command_with_bad_args_keeps_its_usage_notice() {
    let mut app = app();
    type_str(&mut app, "/mode warlock");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Notice(text)) if text.contains("unknown mode")
    ));
}

#[test]
fn submit_expands_at_file_references() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("ctx.txt"), "the context\n").unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();
    type_str(&mut app, "use @ctx.txt here");
    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Submit(prepared)) = action else {
        panic!("expected a submit, got {action:?}");
    };
    assert!(
        prepared.text.contains("the context"),
        "got: {}",
        prepared.text
    );
    // The transcript keeps the compact form.
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::User { text, .. }) if text == "use @ctx.txt here"
    ));
}

#[test]
fn submit_attaches_image_at_refs() {
    let tmp = tempfile::tempdir().unwrap();
    // Minimal 1x1 PNG.
    let png = [
        0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xfe, 0xd4, 0xef, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    std::fs::write(tmp.path().join("shot.png"), png).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();
    type_str(&mut app, "look at @shot.png");
    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Submit(prepared)) = action else {
        panic!("expected a submit, got {action:?}");
    };
    assert!(
        prepared.text.contains("[image: shot.png]"),
        "got: {}",
        prepared.text
    );
    assert_eq!(prepared.images.len(), 1);
    assert!(prepared.images[0].ends_with("shot.png"));
}

/// A 1x1 PNG for tests that only need a real image file on disk.
const MINI_PNG: [u8; 70] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xfe, 0xd4, 0xef, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn pasting_image_paths_shows_numbered_indicators() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    std::fs::write(tmp.path().join("b.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    app.handle_paste(&tmp.path().join("b.png").display().to_string());

    assert!(app.input.contains("[Image #1]"), "input: {}", app.input);
    assert!(app.input.contains("[Image #2]"), "input: {}", app.input);
    assert_eq!(app.pending_images.len(), 2);
}

#[test]
fn pasting_the_same_image_twice_stages_it_once() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    let token = tmp.path().join("a.png").display().to_string();
    app.handle_paste(&token);
    app.handle_paste(&token);

    assert_eq!(app.pending_images.len(), 1);
    assert!(!app.input.contains("[Image #2]"), "input: {}", app.input);
}

#[test]
fn clearing_the_composer_drops_staged_images() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    assert_eq!(app.pending_images.len(), 1);

    app.clear_input();
    assert!(app.pending_images.is_empty());
    assert!(app.input.is_empty());
}

#[test]
fn backspace_deletes_a_pasted_image_token_in_one_press() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    assert_eq!(app.input, "[Image #1]");
    assert_eq!(app.pending_images.len(), 1);
    assert_eq!(app.cursor, app.input.chars().count());

    // One Backspace at the end of the token removes the whole attachment.
    press(&mut app, KeyCode::Backspace);
    assert!(app.input.is_empty(), "input: {}", app.input);
    assert!(app.pending_images.is_empty());
    assert_eq!(app.cursor, 0);
}

#[test]
fn delete_removes_image_token_under_the_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    type_str(&mut app, "see ");
    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    assert!(app.input.ends_with("[Image #1]"), "input: {}", app.input);

    // Park the cursor on the '[' of the token, then Delete.
    app.cursor = "see ".chars().count();
    press(&mut app, KeyCode::Delete);
    assert_eq!(app.input, "see ");
    assert!(app.pending_images.is_empty());
    assert_eq!(app.cursor, "see ".chars().count());
}

#[test]
fn deleting_one_image_renumbers_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    std::fs::write(tmp.path().join("b.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    app.handle_paste(&tmp.path().join("b.png").display().to_string());
    assert_eq!(app.input, "[Image #1] [Image #2]");
    let first = app.pending_images[0].clone();
    let second = app.pending_images[1].clone();

    // Cursor after token #1; Backspace drops #1 and renumbers #2 → #1.
    app.cursor = "[Image #1]".chars().count();
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.input, " [Image #1]");
    assert_eq!(app.pending_images, vec![second]);
    assert!(!app.pending_images.contains(&first));
}

#[test]
fn sniff_identifies_supported_image_formats() {
    assert_eq!(sniff_image_ext(&MINI_PNG), Some("png"));
    assert_eq!(sniff_image_ext(&[0xff, 0xd8, 0xff, 0xe0]), Some("jpg"));
    assert_eq!(sniff_image_ext(b"GIF89a\x01\x00"), Some("gif"));
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&[0x1a, 0x00, 0x00, 0x00]);
    webp.extend_from_slice(b"WEBPVP8 ");
    assert_eq!(sniff_image_ext(&webp), Some("webp"));
    assert_eq!(sniff_image_ext(b"not an image at all"), None);
    assert_eq!(sniff_image_ext(&[]), None);
}

#[test]
fn tab_completes_at_paths_from_the_directory_listing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("readme.md"), "x").unwrap();
    std::fs::create_dir(tmp.path().join("reach")).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    // Common prefix of readme.md / reach.
    type_str(&mut app, "see @re");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "see @rea");

    // Unique file completes fully.
    type_str(&mut app, "d");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "see @readme.md");
}

#[test]
fn tab_completes_unique_directory_with_a_trailing_slash() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sources")).unwrap();
    std::fs::write(tmp.path().join("sources").join("inner.rs"), "x").unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();
    type_str(&mut app, "@so");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "@sources/");
    type_str(&mut app, "in");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "@sources/inner.rs");
}

#[test]
fn genie_and_sovereign_parse_as_mode_switches() {
    assert_eq!(
        SlashCommand::parse("/genie"),
        Some(Ok(SlashCommand::Mode(Some(Mode::Genie))))
    );
    assert_eq!(
        SlashCommand::parse("/sovereign"),
        Some(Ok(SlashCommand::Mode(Some(Mode::Sovereign))))
    );
}

#[test]
fn effort_parses_levels_default_and_bare() {
    assert_eq!(
        SlashCommand::parse("/effort"),
        Some(Ok(SlashCommand::Effort(None))),
        "bare /effort opens the picker"
    );
    assert_eq!(
        SlashCommand::parse("/effort low"),
        Some(Ok(SlashCommand::Effort(Some(Some(ReasoningEffort::Low)))))
    );
    assert_eq!(
        SlashCommand::parse("/effort HIGH"),
        Some(Ok(SlashCommand::Effort(Some(Some(ReasoningEffort::High))))),
        "level is case-insensitive"
    );
    assert_eq!(
        SlashCommand::parse("/effort default"),
        Some(Ok(SlashCommand::Effort(Some(None)))),
        "default clears back to the provider default"
    );
    assert!(
        matches!(SlashCommand::parse("/effort turbo"), Some(Err(_))),
        "unknown level is an error"
    );
}

#[test]
fn goal_parses_show_and_set() {
    assert_eq!(
        SlashCommand::parse("/goal"),
        Some(Ok(SlashCommand::Goal(None)))
    );
    assert_eq!(
        SlashCommand::parse("/goal ship the thing"),
        Some(Ok(SlashCommand::Goal(Some("ship the thing".into()))))
    );
}

#[test]
fn server_subcommands_parse() {
    assert_eq!(
        SlashCommand::parse("/server"),
        Some(Ok(SlashCommand::Server(ServerAction::Status)))
    );
    assert_eq!(
        SlashCommand::parse("/server status"),
        Some(Ok(SlashCommand::Server(ServerAction::Status)))
    );
    assert_eq!(
        SlashCommand::parse("/server start"),
        Some(Ok(SlashCommand::Server(ServerAction::Start)))
    );
    assert_eq!(
        SlashCommand::parse("/server stop"),
        Some(Ok(SlashCommand::Server(ServerAction::Stop)))
    );
    let parsed = SlashCommand::parse("/server restart").expect("is a slash command");
    let message = parsed.expect_err("unknown subcommand");
    assert!(message.contains("status|start|stop"), "got: {message}");
}

#[test]
fn provider_add_accepts_xai_kinds() {
    let parsed =
        SlashCommand::parse("/provider add xai xai https://api.x.ai/v1 grok-4.3 XAI_API_KEY")
            .expect("is a slash command")
            .expect("parses");
    assert_eq!(
        parsed,
        SlashCommand::Provider(ProviderAction::Add {
            name: "xai".to_string(),
            kind: ProviderKind::Xai,
            base_url: "https://api.x.ai/v1".to_string(),
            model: "grok-4.3".to_string(),
            api_key_env: Some("XAI_API_KEY".to_string()),
        })
    );

    let parsed = SlashCommand::parse("/provider add grok xaioauth https://api.x.ai/v1 grok-4.3")
        .expect("is a slash command")
        .expect("parses");
    assert_eq!(
        parsed,
        SlashCommand::Provider(ProviderAction::Add {
            name: "grok".to_string(),
            kind: ProviderKind::XaiOauth,
            base_url: "https://api.x.ai/v1".to_string(),
            model: "grok-4.3".to_string(),
            api_key_env: None,
        })
    );

    // The error for an unknown kind names the xai kinds too.
    let parsed =
        SlashCommand::parse("/provider add x bogus https://e.com m").expect("is a slash command");
    let message = parsed.expect_err("unknown kind");
    assert!(message.contains("xai|xaioauth"), "got: {message}");
}

#[test]
fn provider_add_accepts_openrouter_kind() {
    let parsed = SlashCommand::parse(
            "/provider add openrouter openrouter https://openrouter.ai/api/v1 openrouter/auto OPENROUTER_API_KEY",
        )
        .expect("is a slash command")
        .expect("parses");
    assert_eq!(
        parsed,
        SlashCommand::Provider(ProviderAction::Add {
            name: "openrouter".to_string(),
            kind: ProviderKind::OpenRouter,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            model: "openrouter/auto".to_string(),
            api_key_env: Some("OPENROUTER_API_KEY".to_string()),
        })
    );

    // The error for an unknown kind names openrouter too.
    let parsed =
        SlashCommand::parse("/provider add x bogus https://e.com m").expect("is a slash command");
    let message = parsed.expect_err("unknown kind");
    assert!(message.contains("openrouter"), "got: {message}");
}

#[test]
fn provider_add_accepts_cloudflare_kind() {
    let parsed = SlashCommand::parse(
            "/provider add cf cloudflare https://api.cloudflare.com/client/v4/accounts/acc/ai/v1 @cf/zai-org/glm-5.2 CLOUDFLARE_API_TOKEN",
        )
        .expect("is a slash command")
        .expect("parses");
    assert_eq!(
        parsed,
        SlashCommand::Provider(ProviderAction::Add {
            name: "cf".to_string(),
            kind: ProviderKind::Cloudflare,
            base_url: "https://api.cloudflare.com/client/v4/accounts/acc/ai/v1".to_string(),
            model: "@cf/zai-org/glm-5.2".to_string(),
            api_key_env: Some("CLOUDFLARE_API_TOKEN".to_string()),
        })
    );
}

#[test]
fn provider_no_args_opens_the_menu_and_list_still_lists() {
    // Bare `/provider` opens the interactive picker; `/provider list` keeps
    // the scripting/text behavior.
    assert_eq!(
        SlashCommand::parse("/provider"),
        Some(Ok(SlashCommand::Provider(ProviderAction::Menu)))
    );
    assert_eq!(
        SlashCommand::parse("/provider list"),
        Some(Ok(SlashCommand::Provider(ProviderAction::List)))
    );
}

#[test]
fn login_parses_with_a_provider_argument() {
    assert_eq!(
        SlashCommand::parse("/login xai"),
        Some(Ok(SlashCommand::Login("xai".to_string())))
    );
    let parsed = SlashCommand::parse("/login").expect("is a slash command");
    let message = parsed.expect_err("missing provider");
    assert!(message.contains("/login xai"), "got: {message}");
}

#[test]
fn fusion_parses_toggle_config_and_rejects_unknown() {
    assert_eq!(
        SlashCommand::parse("/fusion"),
        Some(Ok(SlashCommand::Fusion(FusionAction::Toggle)))
    );
    assert_eq!(
        SlashCommand::parse("/fusion config"),
        Some(Ok(SlashCommand::Fusion(FusionAction::Config)))
    );
    assert!(matches!(SlashCommand::parse("/fusion bogus"), Some(Err(_))));
}

#[test]
fn ultra_parses_toggle_config_and_rejects_unknown() {
    assert_eq!(
        SlashCommand::parse("/ultra"),
        Some(Ok(SlashCommand::Ultra(UltraAction::Toggle)))
    );
    assert_eq!(
        SlashCommand::parse("/ultra config"),
        Some(Ok(SlashCommand::Ultra(UltraAction::Config)))
    );
    assert!(matches!(SlashCommand::parse("/ultra bogus"), Some(Err(_))));
}

/// The `/ultra config` picker offers every lens in the catalog plus a trailing
/// judge row, pre-toggled to the configured roster, and Enter turns exactly the
/// toggled rows into the roster to save. The lens rows are compared as a set:
/// a user `~/.wizard/subagents/` entry that shadows a built-in moves it to the
/// end of the catalog, which is a legitimate reordering, not a failure.
#[test]
fn ultra_picker_saves_the_toggled_lenses_and_the_judge_row() {
    let mut app = app();
    app.open_ultra_picker();
    let picker = app.picker.as_ref().expect("the ultra picker is open");
    assert_eq!(picker.kind, PickerKind::UltraLenses);
    let (judge, lenses) = picker.items.split_last().expect("rows");
    assert_eq!(judge.value, ULTRA_JUDGE_ROW);
    assert!(judge.current, "the default roster runs one judge");
    for name in ultra::DEFAULT_LENSES {
        let row = lenses
            .iter()
            .find(|item| item.value == *name)
            .unwrap_or_else(|| panic!("{name} has a row"));
        assert!(row.current, "{name} is in the default roster");
    }

    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Command(SlashCommand::Ultra(UltraAction::Apply(saved)))) = action else {
        panic!("Enter saves the roster, got {action:?}");
    };
    let mut got = saved.lenses;
    got.sort();
    let mut want = UltraConfig::default().lenses;
    want.sort();
    assert_eq!(got, want);
    assert_eq!(saved.judges, 1, "the judge row was left on");
    assert!(app.picker.is_none(), "the picker closed");
}

/// Untoggling the judge row is how the compare phase is turned off — it must
/// reach `[ultra]` as `judges = 0`, not be silently floored back to one.
#[test]
fn ultra_picker_untoggling_the_judge_row_drops_the_compare_phase() {
    let mut app = app();
    app.open_ultra_picker();
    let picker = app.picker.as_mut().expect("the ultra picker is open");
    picker.selected = picker.items.len() - 1;
    press(&mut app, KeyCode::Char(' '));

    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Command(SlashCommand::Ultra(UltraAction::Apply(saved)))) = action else {
        panic!("Enter saves the roster, got {action:?}");
    };
    assert_eq!(saved.judges, 0);
    assert!(!saved.lenses.is_empty(), "the lens roster is untouched");
}

/// An empty roster has nothing to fan out over, so Enter refuses it rather
/// than persisting an `[ultra]` that `UltraEngine::build` would then reject
/// at the next toggle.
#[test]
fn ultra_picker_refuses_an_empty_roster() {
    let mut app = app();
    app.open_ultra_picker();
    let picker = app.picker.as_mut().expect("the ultra picker is open");
    for item in &mut picker.items {
        item.current = false;
    }

    assert!(press(&mut app, KeyCode::Enter).is_none(), "nothing to save");
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Notice(text)) if text.contains("at least one lens")
    ));
}

#[test]
fn rewind_parses_with_and_without_a_turn() {
    assert_eq!(
        SlashCommand::parse("/rewind"),
        Some(Ok(SlashCommand::Rewind(None)))
    );
    assert_eq!(
        SlashCommand::parse("/rewind 7"),
        Some(Ok(SlashCommand::Rewind(Some(7))))
    );
    let parsed = SlashCommand::parse("/rewind soon").expect("is a slash command");
    let message = parsed.expect_err("non-numeric turn");
    assert!(message.contains("/rewind [turn]"), "got: {message}");
}

#[test]
fn resume_parses_with_and_without_an_id() {
    assert_eq!(
        SlashCommand::parse("/resume"),
        Some(Ok(SlashCommand::Resume(None)))
    );
    assert_eq!(
        SlashCommand::parse("/resume 2026-06-09T09-30-00"),
        Some(Ok(SlashCommand::Resume(Some(
            "2026-06-09T09-30-00".to_string()
        ))))
    );
}

#[test]
fn resume_picker_selection_becomes_a_resume_command() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Resume,
        title: " resume session ".to_string(),
        items: vec![PickerItem {
            value: "2026-06-09T09-30-00".to_string(),
            detail: "add resume · 4 msgs".to_string(),
            current: false,
        }],
        selected: 0,
    });
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Resume(Some(id)))) if id == "2026-06-09T09-30-00"
    ));
    assert!(app.picker.is_none(), "the picker closed");
}

#[test]
fn resume_claude_parses_with_and_without_an_id() {
    assert_eq!(
        SlashCommand::parse("/resume-claude"),
        Some(Ok(SlashCommand::ResumeClaude(None)))
    );
    assert_eq!(
        SlashCommand::parse("/resume-claude 008a53cb"),
        Some(Ok(SlashCommand::ResumeClaude(Some("008a53cb".to_string()))))
    );
}

/// Enter on a Claude Code row must produce a
/// [`SlashCommand::ResumeClaude`] and never a [`SlashCommand::Resume`].
///
/// The two pickers are the same widget over ids that look alike, and the
/// commands behind them do different things — one reopens a Wizard session,
/// the other reads another program's file and writes a new session from it.
/// Routing a Claude row to `/resume` would look up a Wizard session that does
/// not exist and report the import as a missing session.
#[test]
fn resume_claude_picker_selection_becomes_a_resume_claude_command() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::ResumeClaude,
        title: " continue from claude code ".to_string(),
        items: vec![PickerItem {
            value: "008a53cb-01e1-4443-9daf-b2d7311f4f35".to_string(),
            detail: "Create private GitHub repo · 2h · 13 branch points".to_string(),
            current: false,
        }],
        selected: 0,
    });
    let action = press(&mut app, KeyCode::Enter);
    assert!(
        matches!(
            &action,
            Some(AppAction::Command(SlashCommand::ResumeClaude(Some(id))))
                if id == "008a53cb-01e1-4443-9daf-b2d7311f4f35"
        ),
        "got: {action:?}"
    );
    assert!(app.picker.is_none(), "the picker closed");
}

/// With no Claude Code history for this directory the picker does not open at
/// all, and the notice says *why* it is empty.
///
/// "No sessions" on its own sends people looking for a bug in the import.
/// Claude Code files history under a slug of the working directory, so the
/// overwhelmingly common cause is that it was run somewhere else.
#[test]
fn an_empty_claude_history_explains_itself_instead_of_opening_a_picker() {
    let mut app = app();
    // A directory Claude Code has certainly never been run in. `app()` inherits
    // the real project root, and on the machine this is being written on that
    // root *does* have Claude Code history — a test that passes or fails on
    // whose checkout it runs in is worse than no test.
    let empty = tempfile::tempdir().expect("workspace");
    app.project_root = empty.path().to_path_buf();
    app.open_resume_claude_picker();
    assert!(app.picker.is_none(), "nothing to pick from");
    let notice = app
        .transcript
        .iter()
        .filter_map(|item| match item {
            crate::transcript::TranscriptItem::Notice(text) => Some(text.as_str()),
            _ => None,
        })
        .next_back()
        .expect("a notice");
    assert!(
        notice.contains("slug of the working directory"),
        "got: {notice}"
    );
}

#[test]
fn load_transcript_replays_messages_and_pairs_tool_results() {
    use crate::llm::{ChatMessage, ToolCall};
    let mut app = app();
    let mut assistant = ChatMessage::assistant("reading it");
    assistant.push_tool_call(ToolCall::new(
        "read_file".to_string(),
        serde_json::json!({ "path": "x.rs" }),
    ));
    replay(
        &mut app,
        vec![
            ChatMessage::user("read x.rs"),
            assistant,
            ChatMessage::tool_result("call_read_file", "read_file", "fn main() {}"),
        ],
    );
    assert!(matches!(
        app.transcript.get(0),
        Some(TranscriptItem::User { text, .. }) if text == "read x.rs"
    ));
    assert!(matches!(
        app.transcript.get(1),
        Some(TranscriptItem::Text(text)) if text == "reading it"
    ));
    assert!(matches!(
        app.transcript.get(2),
        Some(TranscriptItem::Tool(tool))
            if tool.name == "read_file"
                && tool.output.as_ref().expect("answered").content == "fn main() {}"
    ));
    assert_eq!(app.transcript.len(), 3);
}

/// System messages are the reports the agent injects mid-conversation:
/// background tasks finishing, subagents reporting back, hooks firing. The
/// live turn shows each one as a notice, so a reload that dropped them (which
/// is what this surface used to do, while the GUI showed them) lost events the
/// user had already watched go by.
#[test]
fn load_transcript_replays_the_agents_system_notes_as_notices() {
    use crate::llm::ChatMessage;
    let mut app = app();
    replay(
        &mut app,
        vec![
            ChatMessage::user("go"),
            ChatMessage::system("[note] background task #1 finished"),
            // Hook *context* stays dropped: it is a payload written for the model,
            // and the hook already reported itself in one line.
            ChatMessage::system(format!(
                "{}\nrepo context written for the model",
                crate::agent::SESSION_START_HOOK_NOTE
            )),
        ],
    );
    assert_eq!(app.transcript.len(), 2, "{:?}", app.transcript);
    assert!(matches!(
        app.transcript.get(1),
        Some(TranscriptItem::Notice(text)) if text.starts_with("[note]")
    ));
}

/// The session file does not record whether a tool call failed, so a reloaded
/// card reads it back out of the dispatcher's own phrasings. Hardcoding
/// success (what this surface used to do) turned every ✗ in a resumed
/// conversation into a ✓, which is exactly the thing you scroll back to find.
#[test]
fn load_transcript_flags_a_failed_call_as_failed() {
    use crate::llm::{ChatMessage, ToolCall};
    let mut app = app();
    let mut assistant = ChatMessage::assistant("");
    assistant.push_tool_call(ToolCall::new("execute", serde_json::json!({})));
    replay(
        &mut app,
        vec![
            assistant,
            ChatMessage::tool_result(
                "call_execute",
                "execute",
                "invalid arguments for 'execute': missing field `command`",
            ),
        ],
    );
    assert!(matches!(
        app.transcript.get(0),
        Some(TranscriptItem::Tool(tool))
            if tool.output.as_ref().expect("answered").is_error
    ));
}

/// A batch's results all arrive on one `tool`-role message now, so the replay
/// has to fill one card per result block. Reading only the first would leave
/// every call after the first of every batch showing as unanswered.
#[test]
fn load_transcript_fills_one_card_per_result_of_a_batch() {
    use crate::llm::{ChatMessage, ToolCall};
    let mut app = app();
    let mut assistant = ChatMessage::assistant("on it");
    assistant.push_tool_call(ToolCall::new(
        "read_file",
        serde_json::json!({ "path": "a" }),
    ));
    assistant.push_tool_call(ToolCall::new(
        "read_file",
        serde_json::json!({ "path": "b" }),
    ));
    let mut results = ChatMessage::tool_result("call_a", "read_file", "body a");
    results.push_tool_result("call_b", "read_file", "body b");
    replay(
        &mut app,
        vec![ChatMessage::user("read both"), assistant, results],
    );

    // user + assistant + two filled cards.
    assert_eq!(app.transcript.len(), 4);
    let filled: Vec<&str> = app
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Tool(tool) => Some(tool.output.as_ref()?.content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(filled, ["body a", "body b"], "both cards were filled");
}

/// The acceptance bar for this workstream, at the surface it is about.
///
/// `transcript::a_live_turn_and_its_replay_agree` pins the *model*; this pins
/// that the TUI actually goes through it. One turn is driven the way the event
/// loop drives it — `App::handle_agent_event`, prompt and all — and the same
/// turn is replayed the way `/resume` replays it, and the two conversations
/// have to be the same conversation.
///
/// It would have failed at any point in this surface's history before now:
/// the live path folded events into a `Vec<TranscriptEntry>` of its own, and
/// that reducer dropped the system note, reported the failed call as a
/// success, and paired a batch's results by name.
#[test]
fn the_tuis_live_turn_and_its_replay_agree() {
    use crate::llm::{ChatMessage, FunctionCall, ToolCall};

    let call = |name: &str, id: &str, args: serde_json::Value| ToolCall {
        id: id.to_string(),
        function: FunctionCall {
            name: name.to_string(),
            arguments: args,
        },
    };
    let mut assistant = ChatMessage::assistant("reading both, then drawing");
    assistant.push_tool_call(call(
        "read_file",
        "toolu_a",
        serde_json::json!({"path": "a.rs"}),
    ));
    assistant.push_tool_call(call(
        "read_file",
        "toolu_b",
        serde_json::json!({"path": "b.rs"}),
    ));
    assistant.push_tool_call(call(
        "render",
        "toolu_c",
        serde_json::json!({"shape": "hat"}),
    ));
    // Answered out of call order, which is the case only the id gets right.
    let mut results = ChatMessage::tool_result("toolu_b", "read_file", "body b");
    results.push_tool_result("toolu_a", "read_file", "body a");
    results.push_tool_result("toolu_c", "render", "Error: the canvas is empty");

    let mut replayed = app();
    replay(
        &mut replayed,
        vec![
            ChatMessage::user("read both and draw"),
            assistant,
            results,
            ChatMessage::user_with_images(
                "Image(s) returned by `render`:",
                vec![
                    crate::llm::Image::new("aGk=", "image/png")
                        .at_path(PathBuf::from("/img/hat.png")),
                ],
            ),
            ChatMessage::system("[note] background task #1 finished"),
        ],
    );

    let mut live = app();
    live.transcript
        .user("read both and draw".to_string(), Vec::new());
    for delta in ["reading both, ", "then drawing"] {
        live.handle_agent_event(AgentEvent::TextDelta(delta.to_string()));
    }
    for (name, args, output) in [
        (
            "read_file",
            serde_json::json!({"path": "a.rs"}),
            crate::tools::ToolOutput::ok("body a"),
        ),
        (
            "read_file",
            serde_json::json!({"path": "b.rs"}),
            crate::tools::ToolOutput::ok("body b"),
        ),
        (
            "render",
            serde_json::json!({"shape": "hat"}),
            crate::tools::ToolOutput::error("Error: the canvas is empty"),
        ),
    ] {
        live.handle_agent_event(AgentEvent::ToolStarted {
            name: name.to_string(),
            args,
        });
        live.handle_agent_event(AgentEvent::ToolFinished {
            name: name.to_string(),
            output,
        });
    }
    live.handle_agent_event(AgentEvent::Images {
        source: ImageSource::Tool("render".to_string()),
        images: vec![ImageRef {
            path: PathBuf::from("/img/hat.png"),
            mime: "image/png".to_string(),
            bytes: 2,
        }],
    });
    live.handle_agent_event(AgentEvent::Notice(
        "[note] background task #1 finished".to_string(),
    ));
    live.handle_agent_event(AgentEvent::Done {
        reason: DoneReason::Completed,
    });

    // The call ids are the one thing only the stored session has; comparing
    // them would be comparing the two sources rather than the two readings.
    let strip = |app: &App| -> Vec<TranscriptItem> {
        app.transcript
            .iter()
            .cloned()
            .map(|item| match item {
                TranscriptItem::Tool(tool) => TranscriptItem::Tool(crate::transcript::ToolItem {
                    call_id: String::new(),
                    ..tool
                }),
                other => other,
            })
            .collect()
    };
    assert_eq!(strip(&replayed), strip(&live));
    // Not vacuous: the turn really did produce all of it, including the
    // failure that replay has to sniff back out of the text.
    assert_eq!(live.transcript.len(), 7, "{:?}", live.transcript.items());
    assert!(
        live.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Tool(tool)
                if tool.output.as_ref().is_some_and(|out| out.is_error)
        )),
        "the failed call is in there"
    );
    // And the fold flags agree too, which is the half of the screen the model
    // does not carry: the failed `render` card is shut on both paths, the
    // short reads are open on both.
    let folds = |app: &App| -> Vec<bool> {
        (0..app.transcript.len())
            .map(|index| app.transcript.folded(index))
            .collect()
    };
    assert_eq!(folds(&replayed), folds(&live));
    assert!(folds(&live).contains(&true), "something is folded");
    assert!(folds(&live).contains(&false), "and something is not");
}

/// The hazard from `transcript::insert_reports_a_mid_vector_change`, seen from
/// the consumer that has to survive it.
///
/// A tool's images are spliced in behind the row that produced them, so every
/// row below moves down one — and the fold flags are keyed by index. If the
/// view treated that as an append, the flags below the splice would stay put
/// while the items moved, and the wrong card would be drawn folded.
#[test]
fn a_tools_images_shift_the_fold_flags_with_the_rows() {
    use crate::llm::{ChatMessage, Image, ToolCall};

    let mut assistant = ChatMessage::assistant("");
    assistant.push_tool_call(ToolCall::new("render", serde_json::json!({})));
    assistant.push_tool_call(ToolCall::new("execute", serde_json::json!({})));
    let mut results = ChatMessage::tool_result("", "render", "drew it");
    // Long enough to fold, so the flag under test is not the default.
    results.push_tool_result("", "execute", "line\n".repeat(20));

    let mut app = app();
    replay(
        &mut app,
        vec![
            assistant,
            results,
            // The carrier arrives last and splices in at index 1, between
            // `render` and `execute`.
            ChatMessage::user_with_images(
                "Image(s) returned by `render`:",
                vec![Image::new("aGk=", "image/png").at_path(PathBuf::from("/img/hat.png"))],
            ),
        ],
    );

    // render, its image, execute.
    assert!(
        matches!(app.transcript.get(1), Some(TranscriptItem::Images { .. })),
        "the splice landed mid-vector: {:?}",
        app.transcript.items()
    );
    assert!(
        matches!(app.transcript.get(2), Some(TranscriptItem::Tool(tool)) if tool.name == "execute"),
        "{:?}",
        app.transcript.items()
    );
    assert!(!app.transcript.folded(0), "render's short output is open");
    assert!(
        app.transcript.folded(2),
        "execute's long output is folded — the flag moved down with its row"
    );
    // Not vacuous: had the flags stayed put, index 1 would carry the fold.
    assert!(!app.transcript.folded(1), "an image row is never folded");
}

#[test]
fn rewind_picker_selection_becomes_a_rewind_command() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Rewind,
        title: " rewind to before turn ".to_string(),
        items: vec![
            PickerItem {
                value: "9".to_string(),
                detail: "fix tests · notes.txt".to_string(),
                current: false,
            },
            PickerItem {
                value: "8".to_string(),
                detail: String::new(),
                current: false,
            },
        ],
        selected: 0,
    });
    press(&mut app, KeyCode::Down);
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Rewind(Some(8))))
    ));
    assert!(app.picker.is_none(), "the picker closed");
}

#[test]
fn rewind_picker_esc_cancels() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Rewind,
        title: " rewind to before turn ".to_string(),
        items: vec![PickerItem {
            value: "3".to_string(),
            detail: String::new(),
            current: false,
        }],
        selected: 0,
    });
    let action = press(&mut app, KeyCode::Esc);
    assert!(action.is_none());
    assert!(app.picker.is_none(), "Esc closed the picker");
}

#[test]
fn agents_parses_to_the_roster_picker() {
    // /agents opens the roster picker. Live runs are watched on the
    // subagent rail below the composer — no separate slash command.
    assert!(matches!(
        SlashCommand::parse("/agents"),
        Some(Ok(SlashCommand::Agents))
    ));
    assert!(
        matches!(
            SlashCommand::parse("/subagents"),
            Some(Err(message)) if message.contains("unknown command")
        ),
        "/subagents was removed; the rail is always on screen"
    );
}

#[test]
fn subagent_picker_selection_prefills_a_delegation_request() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Subagent,
        title: " delegate to subagent ".to_string(),
        items: vec![
            PickerItem {
                value: "worker".to_string(),
                detail: "general-purpose".to_string(),
                current: false,
            },
            PickerItem {
                value: "reviewer".to_string(),
                detail: "code review".to_string(),
                current: false,
            },
        ],
        selected: 0,
    });
    press(&mut app, KeyCode::Down);
    let action = press(&mut app, KeyCode::Enter);
    // Subagents are model-invoked, so Enter pre-fills input instead of
    // emitting a command.
    assert!(action.is_none());
    assert!(app.picker.is_none(), "the picker closed");
    assert_eq!(app.input, "Use the reviewer subagent to ");
    assert_eq!(app.cursor, app.input.chars().count());
}

#[test]
fn ctrl_c_idle_arms_then_exits() {
    let mut app = app();
    assert!(press_ctrl(&mut app, 'c').is_none());
    assert!(app.ctrl_c_armed);
    assert!(!app.should_quit, "first press only arms");
    assert!(press_ctrl(&mut app, 'c').is_none());
    assert!(app.should_quit, "second press exits");
}

#[test]
fn ctrl_c_busy_interrupts_then_exits() {
    let mut app = app();
    app.status.busy = true;
    // First press while busy interrupts the turn, doesn't quit.
    assert!(matches!(
        press_ctrl(&mut app, 'c'),
        Some(AppAction::Interrupt)
    ));
    assert!(!app.should_quit);
    // Armed now: a second press exits even while busy.
    assert!(press_ctrl(&mut app, 'c').is_none());
    assert!(app.should_quit);
}

#[test]
fn any_other_key_disarms_ctrl_c() {
    let mut app = app();
    press_ctrl(&mut app, 'c');
    assert!(app.ctrl_c_armed);
    press(&mut app, KeyCode::Char('x'));
    assert!(!app.ctrl_c_armed);
    // So the next Ctrl-C re-arms rather than quitting.
    assert!(press_ctrl(&mut app, 'c').is_none());
    assert!(!app.should_quit);
}

#[test]
fn dashboard_navigates_and_esc_closes() {
    use crate::session_registry::{SessionRecord, SessionState};
    let mut app = app();
    let make = |id: &str, state: SessionState| SessionRecord {
        id: id.to_string(),
        name: id.to_string(),
        cwd: "/tmp".to_string(),
        model: "m".to_string(),
        mode: "genie".to_string(),
        state,
        activity: String::new(),
        pid: 1,
        started_unix: 0,
        updated_unix: 0,
    };
    app.sessions = vec![
        make("a", SessionState::Working),
        make("b", SessionState::Idle),
    ];
    app.show_dashboard = true;

    // ↓ moves the selection and wraps; ↑ wraps back.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.dashboard_selected, 1);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.dashboard_selected, 0, "wraps to the top");
    press(&mut app, KeyCode::Up);
    assert_eq!(app.dashboard_selected, 1, "wraps to the bottom");

    // Esc closes the modal.
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_dashboard);
}

#[test]
fn dashboard_input_composes_and_esc_clears_then_closes() {
    let mut app = app();
    app.show_dashboard = true;
    press(&mut app, KeyCode::Char('h'));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.dashboard_input, "hi");
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.dashboard_input, "h");
    // Esc with text clears it but keeps the modal open.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.dashboard_input, "");
    assert!(app.show_dashboard);
    // Esc again, now empty, closes the modal.
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_dashboard);
}

#[test]
fn session_record_reflects_state() {
    let mut app = app();
    app.session_id = "sess-1".to_string();
    app.session_name = "fix bug".to_string();
    assert_eq!(app.session_record().state, SessionState::Idle);
    app.status.busy = true;
    assert_eq!(app.session_record().state, SessionState::Working);
}

#[test]
fn todo_update_mirrors_the_list_and_auto_shows_the_overlay_once() {
    use crate::tools::todo::{TodoItem, TodoStatus};
    let mut app = app();
    assert!(!app.show_todos);

    let items = vec![TodoItem {
        content: "first".to_string(),
        status: TodoStatus::InProgress,
    }];
    app.handle_agent_event(AgentEvent::TodoUpdated(items.clone()));
    assert_eq!(app.todos, items);
    assert!(app.show_todos, "first update auto-shows the overlay");

    // The user hides it; later updates respect that.
    app.show_todos = false;
    app.handle_agent_event(AgentEvent::TodoUpdated(items.clone()));
    assert!(!app.show_todos, "auto-show happens only once");
    assert_eq!(app.todos, items, "the list still updates");
}

#[test]
fn esc_dismisses_the_todo_overlay_after_the_diff_sidebar() {
    let mut app = app();
    app.show_todos = true;
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_todos, "Esc dismisses the todo band");

    // Diff sidebar and todo band are independent: Esc closes the
    // diff first, then the overlay, then falls through to the input.
    app.show_todos = true;
    app.diff = Some(DiffPane::default());
    app.input = "draft".to_string();
    press(&mut app, KeyCode::Esc);
    assert!(app.diff.is_none(), "diff closes first");
    assert!(app.show_todos, "todos stay open until the next Esc");
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_todos);
    assert_eq!(app.input, "draft", "input untouched while panels close");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.input, "", "Esc finally clears the input");

    // Vim Normal mode keeps the same escape hatch.
    let mut app = vim_app();
    press(&mut app, KeyCode::Esc); // insert -> normal
    app.show_todos = true;
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_todos, "Normal-mode Esc dismisses the todo band");
}

#[test]
fn usage_events_drive_session_totals_and_the_context_meter() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::Usage {
        prompt_tokens: 100,
        completion_tokens: 20,
    });
    app.handle_agent_event(AgentEvent::Usage {
        prompt_tokens: 50,
        completion_tokens: 5,
    });
    // Session lifetime still accumulates for /cost.
    assert_eq!(app.status.prompt_tokens, 150);
    assert_eq!(app.status.completion_tokens, 25);
    // Context meter tracks the most recent prompt size, not the sum.
    assert_eq!(app.status.context_tokens, 50);

    // Auto-compaction replaces the meter without touching lifetime totals.
    app.handle_agent_event(AgentEvent::ContextSize { tokens: 12 });
    assert_eq!(app.status.context_tokens, 12);
    assert_eq!(app.status.prompt_tokens, 150);
    assert_eq!(app.status.completion_tokens, 25);
}

#[test]
fn background_task_events_drive_the_live_status_bar_counter() {
    let mut app = app();
    assert_eq!(app.status.background_tasks, 0);

    app.handle_agent_event(AgentEvent::TaskStarted {
        id: 1,
        command: "sleep 5".to_string(),
    });
    assert_eq!(
        app.status.background_tasks, 1,
        "marker appears while running"
    );

    app.handle_agent_event(AgentEvent::TaskStarted {
        id: 2,
        command: "ping -c 1 example.com".to_string(),
    });
    assert_eq!(app.status.background_tasks, 2);

    app.handle_agent_event(AgentEvent::TaskFinished {
        id: 1,
        command: "sleep 5".to_string(),
        status: crate::tools::tasks::TaskStatus::Done(0),
    });
    assert_eq!(
        app.status.background_tasks, 1,
        "counter drops back down as tasks finish"
    );
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Notice(text))
            if text.contains("background task #1 finished")
    ));

    app.handle_agent_event(AgentEvent::TaskFinished {
        id: 2,
        command: "ping -c 1 example.com".to_string(),
        status: crate::tools::tasks::TaskStatus::Done(0),
    });
    assert_eq!(
        app.status.background_tasks, 0,
        "marker clears once all finish"
    );
}

#[test]
fn failed_tool_cards_start_collapsed() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "web_fetch".to_string(),
        args: serde_json::json!({"url": "https://example.com"}),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "web_fetch".to_string(),
        output: crate::tools::ToolOutput::error("HTTP 403 Forbidden\n<!DOCTYPE html>\n..."),
    });
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptItem::Tool(tool))
                if tool.output.as_ref().expect("answered").is_error
        ) && app.transcript.folded(app.transcript.len() - 1),
        "errors show only the ✗ card line until expanded via Ctrl-T"
    );

    // Short successful outputs still arrive expanded.
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "read_file".to_string(),
        args: serde_json::json!({"path": "a.txt"}),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "read_file".to_string(),
        output: crate::tools::ToolOutput::ok("one line"),
    });
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Tool(tool))
            if !tool.output.as_ref().expect("answered").is_error
    ));
    assert!(!app.transcript.folded(app.transcript.len() - 1));
}

#[test]
fn stream_retry_discards_the_partial_streamed_text() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::TextDelta("half an ans".to_string()));
    app.handle_agent_event(AgentEvent::StreamRetrying);
    app.handle_agent_event(AgentEvent::Error(
        "LLM unavailable (stream stalled); sleeping 5s then retrying (attempt 1)".to_string(),
    ));
    assert_eq!(
        app.transcript.streaming().1,
        "",
        "the doomed attempt's partial text is dropped, not flushed"
    );
    assert!(
        !app.transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::Text(text) if text.contains("half an ans"))),
        "no assistant row made of the discarded partial"
    );

    // The retry streams the full answer; only that lands.
    app.handle_agent_event(AgentEvent::TextDelta("the full answer".to_string()));
    assert_eq!(app.transcript.streaming().1, "the full answer");
}

#[test]
fn long_outputs_start_collapsed_by_lines_or_length() {
    assert!(!collapse_long("short output"));
    assert!(!collapse_long(&"line\n".repeat(6)));
    assert!(collapse_long(&"line\n".repeat(7)), "more than six lines");
    assert!(
        collapse_long(&"x".repeat(601)),
        "a giant single line wraps to fill the screen just the same"
    );
}

fn click(app: &mut App, column: u16, row: u16) {
    use crossterm::event::MouseEvent;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let _ = app.handle_event(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }
}

#[test]
fn clicking_a_tool_card_header_toggles_its_output() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "execute".to_string(),
        args: serde_json::json!({"command": "ls"}),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "execute".to_string(),
        output: crate::tools::ToolOutput::ok("a\nb\nc\nd\ne\nf\ng\nh"),
    });
    let index = app.transcript.len() - 1;
    assert!(app.transcript.folded(index));

    // Render a frame so the click hit map is populated.
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
    let (row, hit_index) = *app
        .card_hits
        .borrow()
        .first()
        .expect("the card header should be clickable");
    assert_eq!(hit_index, index);

    // A plain click on the header expands the card...
    click(&mut app, 2, row);
    assert!(!app.transcript.folded(index));

    // ...and a second click (at its possibly-shifted row) collapses it.
    terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
    let row = app.card_hits.borrow().first().map(|(y, _)| *y).unwrap();
    click(&mut app, 2, row);
    assert!(app.transcript.folded(index));
}

/// A real PNG on disk, as the image store would have left it: a solid red
/// square, so any cell that drew it is unmistakable.
fn red_png(dir: &Path) -> ImageRef {
    let path = dir.join("red.png");
    image::RgbaImage::from_pixel(48, 48, image::Rgba([255, 0, 0, 255]))
        .save(&path)
        .expect("wrote the png");
    ImageRef {
        path,
        mime: "image/png".to_string(),
        bytes: std::fs::metadata(dir.join("red.png")).unwrap().len() as usize,
    }
}

/// Every cell of a drawn frame, row by row: what is on screen.
fn screen(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// Rows holding image pixels. No token in either built-in theme is a 24-bit
/// color (both are named or palette-indexed on purpose; see
/// `assets/themes/minimal.toml`), so a cell painted in RGB is an image cell
/// and nothing else. That makes this both the "it drew" check and the "it left
/// nothing behind" check.
///
/// The one other RGB source is the syntax highlighter's gray ramp, which these
/// fixtures never render.
fn pixel_rows(buf: &ratatui::buffer::Buffer) -> Vec<u16> {
    use ratatui::style::Color;
    (0..buf.area.height)
        .filter(|&y| {
            (0..buf.area.width).any(|x| {
                let cell = buf.cell((x, y)).unwrap();
                matches!(cell.fg, Color::Rgb(..)) || matches!(cell.bg, Color::Rgb(..))
            })
        })
        .collect()
}

fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).unwrap().symbol())
        .collect()
}

#[test]
fn an_image_from_the_model_and_one_from_a_tool_both_render_with_their_file() {
    let dir = tempfile::tempdir().unwrap();
    let image = red_png(dir.path());
    let mut app = app();
    app.welcome_dismissed = true;

    app.handle_agent_event(AgentEvent::TextDelta("here it is".to_string()));
    app.handle_agent_event(AgentEvent::Images {
        source: ImageSource::Assistant,
        images: vec![image.clone()],
    });
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "render".to_string(),
        args: serde_json::json!({}),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "render".to_string(),
        output: crate::tools::ToolOutput::ok("drawn"),
    });
    app.handle_agent_event(AgentEvent::Images {
        source: ImageSource::Tool("render".to_string()),
        images: vec![image.clone()],
    });
    assert_eq!(
        app.transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Images { .. }))
            .count(),
        2,
    );

    let buf = screen(&app, 80, 40);
    let text: String = (0..buf.area.height)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    // Both images were drawn, in pixels.
    assert_eq!(
        pixel_rows(&buf).len(),
        6,
        "two three-row image blocks, drawn in pixels:\n{text}"
    );
    // Each is named by what made it, and each names its file — untruncated,
    // on a line of its own, so it can be copied out and opened.
    assert!(text.contains("image · image/png"), "{text}");
    assert!(text.contains("image from `render` · image/png"), "{text}");
    let path = image.path.display().to_string();
    assert_eq!(
        (0..buf.area.height)
            .filter(|&y| row_text(&buf, y).trim() == path)
            .count(),
        2,
        "each image's path stands alone on its own row:\n{text}"
    );
}

#[test]
fn a_scrolled_image_is_clipped_to_the_transcript_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let image = red_png(dir.path());
    let mut app = app();
    app.welcome_dismissed = true;
    app.handle_agent_event(AgentEvent::Images {
        source: ImageSource::Assistant,
        images: vec![image],
    });
    // Enough text after it to push the image off the top of a short screen.
    for line in 0..12 {
        app.handle_agent_event(AgentEvent::Notice(format!("line {line}")));
    }

    // Pinned to the bottom, the image is above the viewport: no pixels.
    let (width, height) = (60u16, 12u16);
    let buf = screen(&app, width, height);
    assert!(pixel_rows(&buf).is_empty(), "the image is scrolled away");

    // Scroll it back into view a row at a time. However the block straddles
    // the edge of the viewport, its pixels stay inside the transcript body —
    // never in the composer, the rail or the status bar below it.
    let body = crate::ui::regions(&app, ratatui::layout::Rect::new(0, 0, width, height)).body;
    let mut ever_drawn = false;
    for _ in 0..20 {
        app.scroll_transcript(1);
        let buf = screen(&app, width, height);
        let rows = pixel_rows(&buf);
        ever_drawn |= !rows.is_empty();
        for row in rows {
            assert!(
                row < body.bottom(),
                "row {row} has pixels below the transcript body (which ends at {})",
                body.bottom()
            );
        }
    }
    assert!(
        ever_drawn,
        "scrolling back never brought the image into view"
    );

    // And back at the bottom, the screen is exactly what it was before the
    // scroll — no pixels left over anywhere.
    app.scroll_to_bottom();
    assert!(pixel_rows(&screen(&app, width, height)).is_empty());
}

#[test]
fn a_subagents_image_renders_inside_that_runs_pane() {
    let dir = tempfile::tempdir().unwrap();
    let image = red_png(dir.path());
    let mut app = app();
    app.welcome_dismissed = true;
    app.handle_agent_event(AgentEvent::SubagentRunStarted {
        run: 1,
        bg: None,
        name: "researcher".to_string(),
        task: "look".to_string(),
    });
    app.handle_agent_event(AgentEvent::SubagentRunImages {
        run: 1,
        source: ImageSource::Tool("screenshot".to_string()),
        images: vec![image],
    });

    // The run's image is its own: the main chat, which the subagent has said
    // nothing to yet, shows no pixels.
    assert!(pixel_rows(&screen(&app, 80, 40)).is_empty());

    // Open the pane and it is there, on the tool that took it.
    app.attached = Some(0);
    let buf = screen(&app, 80, 40);
    assert!(!pixel_rows(&buf).is_empty(), "the pane draws the image");
    let text: String = (0..buf.area.height)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("image from `screenshot`"), "{text}");
}

#[test]
fn a_resumed_session_replays_the_images_it_left_on_disk() {
    use crate::llm::{ChatMessage, Image};
    let png = Image::new("iVBOR", "image/png").at_path(PathBuf::from("/img/a.png"));

    let mut app = app();
    let mut assistant = ChatMessage::assistant("done");
    assistant.push_image(png.clone());
    replay(
        &mut app,
        vec![
            ChatMessage::user("draw"),
            assistant,
            ChatMessage::tool_result("call_render", "render", "ok"),
            // The images `render` returned, riding back to the model. Not a
            // prompt — the agent wrote it, not the user.
            ChatMessage::user_with_images("Image(s) returned by `render`:", vec![png]),
        ],
    );

    let images: Vec<&TranscriptItem> = app
        .transcript
        .iter()
        .filter(|item| matches!(item, TranscriptItem::Images { .. }))
        .collect();
    assert!(
        matches!(
            images.as_slice(),
            [
                TranscriptItem::Images {
                    source: ImageSource::Assistant,
                    ..
                },
                TranscriptItem::Images {
                    source: ImageSource::Tool(tool),
                    images,
                },
            ] if tool == "render" && images[0].path == Path::new("/img/a.png")
        ),
        "both directions came back, attributed: {images:?}"
    );
    assert!(
        !app.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::User { text, .. } if text.contains("Image(s) returned")
        )),
        "the carrier message is not replayed as something the user said"
    );
}

#[test]
fn backtab_toggles_plan_mode() {
    let mut app = app();
    let action = press(&mut app, KeyCode::BackTab);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Plan))
    ));
}

#[test]
fn backtab_in_a_picker_still_navigates() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Mode,
        title: " select mode ".to_string(),
        items: vec![
            PickerItem {
                value: "genie".to_string(),
                detail: String::new(),
                current: true,
            },
            PickerItem {
                value: "sovereign".to_string(),
                detail: String::new(),
                current: false,
            },
        ],
        selected: 0,
    });
    let action = press(&mut app, KeyCode::BackTab);
    assert!(action.is_none(), "the picker captured the key");
    assert_eq!(app.picker.as_ref().expect("open").selected, 1);
}

/// Open a plan review via the agent event, returning the verdict
/// receiver.
fn open_review(app: &mut App, plan: &str) -> tokio::sync::oneshot::Receiver<PlanVerdict> {
    let (gate, rx) = crate::agent::PlanGate::open();
    app.handle_agent_event(AgentEvent::PlanReady {
        plan: plan.to_string(),
        gate,
    });
    rx
}

#[test]
fn plan_ready_opens_a_review_and_y_approves() {
    let mut app = app();
    let mut rx = open_review(&mut app, "# the plan");
    let review = app.plan_review.as_ref().expect("review open");
    assert_eq!(review.plan, "# the plan");
    assert!(app.plan_mode, "a pending plan implies plan mode");

    // Review keys never leak into the input line.
    press(&mut app, KeyCode::Char('y'));
    assert!(app.input.is_empty());
    assert!(app.plan_review.is_none(), "review closed");
    assert!(!app.plan_mode, "approval clears the plan-mode mirror");
    assert_eq!(rx.try_recv(), Ok(PlanVerdict::approve()));
}

#[test]
fn plan_review_enter_also_approves() {
    let mut app = app();
    let mut rx = open_review(&mut app, "# p");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert_eq!(rx.try_recv(), Ok(PlanVerdict::approve()));
}

#[test]
fn plan_review_rejection_collects_feedback() {
    let mut app = app();
    let mut rx = open_review(&mut app, "# p");

    press(&mut app, KeyCode::Char('n'));
    let review = app.plan_review.as_ref().expect("still open");
    assert_eq!(review.feedback.as_deref(), Some(""));

    type_str(&mut app, "add testz");
    press(&mut app, KeyCode::Backspace);
    type_str(&mut app, "s first");
    assert!(app.input.is_empty(), "feedback typing never hits the input");
    press(&mut app, KeyCode::Enter);

    assert!(app.plan_review.is_none(), "review closed");
    assert!(app.plan_mode, "rejection keeps plan mode on");
    assert_eq!(rx.try_recv(), Ok(PlanVerdict::reject("add tests first")));
}

#[test]
fn plan_review_esc_leaves_feedback_entry() {
    let mut app = app();
    let mut rx = open_review(&mut app, "# p");
    press(&mut app, KeyCode::Char('n'));
    type_str(&mut app, "half a thought");
    press(&mut app, KeyCode::Esc);
    let review = app.plan_review.as_ref().expect("still open");
    assert!(review.feedback.is_none(), "back to the review state");
    assert!(rx.try_recv().is_err(), "no verdict sent yet");
    // 'n' again starts fresh feedback.
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(
        app.plan_review.as_ref().expect("open").feedback.as_deref(),
        Some("")
    );
}

/// Open an interview via the agent event, returning the answers receiver.
fn open_interview(
    app: &mut App,
    questions: Vec<InterviewQuestion>,
) -> tokio::sync::oneshot::Receiver<Option<Vec<String>>> {
    let (gate, rx) = crate::agent::InterviewGate::open();
    app.handle_agent_event(AgentEvent::Interview { questions, gate });
    rx
}

fn question(q: &str, options: &[&str]) -> InterviewQuestion {
    InterviewQuestion {
        question: q.to_string(),
        options: options.iter().map(|s| s.to_string()).collect(),
    }
}

/// Parse `input` and return the agent-runnable verdict, asserting it is a
/// well-formed command first.
fn runnable(input: &str) -> Result<(), String> {
    match SlashCommand::parse(input) {
        Some(Ok(command)) => command.agent_runnable(),
        other => panic!("{input} did not parse to a command: {other:?}"),
    }
}

#[test]
fn agent_runnable_allows_self_config_and_info_commands() {
    for input in [
        "/effort high",
        "/model claude-sonnet-5",
        "/mode sovereign",
        "/goal ship it",
        "/goal",
        "/status",
        "/diff",
        "/compact",
        "/reload",
        "/settings",
        "/fusion",
        "/ultra",
    ] {
        assert!(runnable(input).is_ok(), "{input} should be runnable");
    }
}

#[test]
fn command_requested_event_queues_for_post_turn_dispatch() {
    let mut app = app();
    assert!(app.pending_agent_commands.is_empty());
    app.handle_agent_event(AgentEvent::CommandRequested("/effort high".into()));
    assert_eq!(app.pending_agent_commands, vec!["/effort high".to_string()]);
    // A second request accumulates rather than replacing.
    app.handle_agent_event(AgentEvent::CommandRequested("/compact".into()));
    assert_eq!(
        app.pending_agent_commands,
        vec!["/effort high".to_string(), "/compact".to_string()]
    );
}

#[test]
fn agent_runnable_refuses_pickers_and_dangerous_commands() {
    for input in [
        "/effort",   // interactive picker without an argument
        "/model",    // interactive picker without an argument
        "/mode",     // interactive picker without an argument
        "/quit",     // ends the session
        "/clear",    // wipes the conversation
        "/rewind 2", // restores checkpoints
        "/resume",   // switches sessions
        "/login xai",
        "/provider list",
        "/publish",
        "/agents",
        "/fusion config",
        "/ultra config",
    ] {
        assert!(runnable(input).is_err(), "{input} should be refused");
    }
}

#[test]
fn interview_collects_answers_and_advances() {
    let mut app = app();
    let mut rx = open_interview(
        &mut app,
        vec![
            question("which db?", &["sqlite", "postgres"]),
            question("any auth?", &[]),
        ],
    );
    assert!(app.interview.is_some(), "interview modal open");

    // Pick option 2 for the first question, then accept it with Enter.
    press(&mut app, KeyCode::Char('2'));
    assert_eq!(
        app.interview.as_ref().expect("open").input,
        "postgres",
        "digit fills the matching option"
    );
    assert!(
        app.input.is_empty(),
        "interview keys never hit the input line"
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.interview.as_ref().expect("still open").current, 1);

    // Free-text the second answer.
    type_str(&mut app, "yes, oauth");
    press(&mut app, KeyCode::Enter);

    assert!(
        app.interview.is_none(),
        "interview closed after the last answer"
    );
    assert_eq!(
        rx.try_recv(),
        Ok(Some(vec!["postgres".to_string(), "yes, oauth".to_string()]))
    );
}

#[test]
fn interview_esc_dismisses_with_no_answers() {
    let mut app = app();
    let mut rx = open_interview(&mut app, vec![question("which db?", &[])]);
    press(&mut app, KeyCode::Esc);
    assert!(app.interview.is_none(), "dismissed");
    assert_eq!(rx.try_recv(), Ok(None), "decline sent to the tool");
}

#[test]
fn empty_interview_declines_immediately() {
    let mut app = app();
    let mut rx = open_interview(&mut app, vec![]);
    assert!(app.interview.is_none(), "nothing to ask");
    assert_eq!(rx.try_recv(), Ok(None));
}

#[test]
fn omakase_proceeding_clears_flags_and_shows_the_plan() {
    let mut app = app();
    app.plan_mode = true;
    app.omakase = true;
    app.handle_agent_event(AgentEvent::OmakaseProceeding {
        plan: "# chef plan".to_string(),
    });
    assert!(!app.plan_mode, "chef's choice leaves plan mode");
    assert!(!app.omakase, "omakase cleared once proceeding");
    let shown = app.transcript.iter().any(|item| {
        matches!(
            item,
            TranscriptItem::Tool(tool)
                if tool.output.as_ref().is_some_and(|out| out.content == "# chef plan")
        )
    });
    assert!(shown, "the chosen plan is surfaced in the transcript");
}

#[test]
fn cursor_editing_inserts_mid_line() {
    let mut app = app();
    type_str(&mut app, "helo");
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(app.input, "hello");
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Delete);
    assert_eq!(app.input, "ello");
    press(&mut app, KeyCode::End);
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.input, "ell");
}

#[test]
fn history_recall_restores_draft() {
    let mut app = app();
    type_str(&mut app, "first message");
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "second message");
    press(&mut app, KeyCode::Enter);

    type_str(&mut app, "draft");
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "second message");
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "first message");
    press(&mut app, KeyCode::Down);
    assert_eq!(app.input, "second message");
    press(&mut app, KeyCode::Down);
    assert_eq!(app.input, "draft");
}

#[test]
fn picker_navigation_wraps_and_enter_selects() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Model,
        title: " select model ".to_string(),
        items: vec![
            PickerItem {
                value: "qwen3.6:27b".to_string(),
                detail: String::new(),
                current: true,
            },
            PickerItem {
                value: "llama4:8b".to_string(),
                detail: String::new(),
                current: false,
            },
        ],
        selected: 0,
    });

    press(&mut app, KeyCode::Up);
    assert_eq!(app.picker.as_ref().expect("open").selected, 1);
    let action = press(&mut app, KeyCode::Enter);
    match action {
        Some(AppAction::Command(SlashCommand::Model(Some(tag)))) => {
            assert_eq!(tag, "llama4:8b");
        }
        other => panic!("expected model switch, got {other:?}"),
    }
    assert!(app.picker.is_none());
}

#[test]
fn picker_escape_cancels() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Mode,
        title: " select mode ".to_string(),
        items: vec![PickerItem {
            value: "genie".to_string(),
            detail: String::new(),
            current: true,
        }],
        selected: 0,
    });
    press(&mut app, KeyCode::Esc);
    assert!(app.picker.is_none());
}

#[test]
fn ctrl_w_kills_previous_word() {
    let mut app = app();
    type_str(&mut app, "fix the parser bug");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL))
        .expect("key handled");
    assert_eq!(app.input, "fix the parser ");
}

#[test]
fn history_recall_of_slash_command_keeps_browsing_history() {
    let mut app = app();
    type_str(&mut app, "older message");
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "/model");
    press(&mut app, KeyCode::Enter);

    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "/model");
    // The recalled slash command repopulates suggestions; ↑ must keep
    // walking history instead of cycling them.
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "older message");
}

#[test]
fn unbound_ctrl_chords_do_not_insert_literal_chars() {
    let mut app = app();
    type_str(&mut app, "abc");
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
        .expect("key handled");
    assert_eq!(app.input, "abc");
}

#[test]
fn ctrl_g_requests_external_prompt_edit() {
    let mut app = app();
    type_str(&mut app, "draft in progress");
    let action = press_ctrl(&mut app, 'g');
    assert!(action.is_none());
    assert!(app.pending_edit_prompt);
    // The buffer is only replaced after the editor exits cleanly.
    assert_eq!(app.input, "draft in progress");
}

#[test]
fn ctrl_g_is_inert_during_masked_key_entry() {
    // An API key being typed must never be staged into a temp file.
    let mut app = app();
    app.web_key_backend = Some("brave".to_string());
    type_str(&mut app, "sk-secret");
    press_ctrl(&mut app, 'g');
    assert!(!app.pending_edit_prompt);
    assert_eq!(app.input, "sk-secret", "chord must not insert a literal g");
}

#[test]
fn editor_text_replaces_input_with_cursor_at_end() {
    let mut app = app();
    type_str(&mut app, "old draft");
    app.set_input_from_editor("hello\nworld\n".to_string());
    // Exactly one trailing newline (the editor's) is trimmed.
    assert_eq!(app.input, "hello\nworld");
    assert_eq!(app.cursor, app.input.chars().count());
}

#[test]
fn editor_text_trims_at_most_one_line_ending() {
    let mut app = app();
    app.set_input_from_editor("two\n\n".to_string());
    assert_eq!(app.input, "two\n");
    app.set_input_from_editor("crlf\r\n".to_string());
    assert_eq!(app.input, "crlf");
}

#[test]
fn busy_submit_queues_the_message() {
    let mut app = app();
    app.status.busy = true;
    type_str(&mut app, "queued message");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none(), "queued submit is not an AppAction");
    assert_eq!(app.history, vec!["queued message".to_string()]);
    assert_eq!(app.message_queue.len(), 1);
    assert_eq!(app.message_queue[0].text, "queued message");
    assert!(app.input.is_empty(), "composer clears on queue");
    assert!(
        matches!(
            app.transcript
                .iter()
                .find(|item| matches!(item, TranscriptItem::User { .. })),
            Some(TranscriptItem::User { text, .. }) if text == "queued message"
        ),
        "queued text lands in the transcript"
    );
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptItem::Notice(n)) if n.contains("queued")
        ),
        "a notice announces the queue position"
    );
}

#[test]
fn busy_submit_respects_the_queue_cap() {
    let mut app = app();
    app.status.busy = true;
    for i in 0..MESSAGE_QUEUE_CAP {
        type_str(&mut app, &format!("msg {i}"));
        let action = press(&mut app, KeyCode::Enter);
        assert!(action.is_none());
    }
    assert_eq!(app.message_queue.len(), MESSAGE_QUEUE_CAP);
    type_str(&mut app, "one too many");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert_eq!(app.message_queue.len(), MESSAGE_QUEUE_CAP);
    assert_eq!(app.input, "one too many", "overflow keeps the composer");
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptItem::Notice(n)) if n.contains("full")
        ),
        "overflow surfaces a full-queue notice"
    );
}

#[test]
fn goal_kickoff_queues_a_working_turn() {
    let mut app = app();
    app.queue_goal_kickoff("rewrite spore in assembly");
    assert_eq!(app.message_queue.len(), 1);
    assert!(
        app.message_queue[0]
            .text
            .contains("rewrite spore in assembly")
    );
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptItem::User { text: t, .. }) if t.contains("rewrite spore in assembly")
        ),
        "the kickoff prompt lands in the transcript"
    );
}

#[test]
fn goal_kickoff_respects_the_queue_cap() {
    let mut app = app();
    app.status.busy = true;
    for i in 0..MESSAGE_QUEUE_CAP {
        type_str(&mut app, &format!("msg {i}"));
        press(&mut app, KeyCode::Enter);
    }
    app.queue_goal_kickoff("rewrite spore in assembly");
    assert_eq!(app.message_queue.len(), MESSAGE_QUEUE_CAP);
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptItem::Notice(n)) if n.contains("full")
        ),
        "a full queue surfaces a notice instead of auto-starting"
    );
}

#[test]
fn pop_queued_message_is_fifo() {
    let mut app = app();
    app.status.busy = true;
    type_str(&mut app, "first");
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "second");
    press(&mut app, KeyCode::Enter);
    let a = app.pop_queued_message().expect("first");
    let b = app.pop_queued_message().expect("second");
    assert_eq!(a.text, "first");
    assert_eq!(b.text, "second");
    assert!(app.pop_queued_message().is_none());
}

#[test]
fn ctrl_u_kills_to_line_start_keeping_tail() {
    let mut app = app();
    type_str(&mut app, "hello world");
    for _ in 0..6 {
        press(&mut app, KeyCode::Left);
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .expect("key handled");
    assert_eq!(app.input, " world");
    assert_eq!(app.cursor, 0);
}

#[test]
fn submit_rejected_while_agent_rebuilds() {
    let mut app = app();
    app.rebuilding = Some("switching to qwen3:0.6b".to_string());
    type_str(&mut app, "hello");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert!(app.history.is_empty());
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Notice(_))
    ));
}

#[test]
fn ctrl_p_is_a_noop_while_busy() {
    let mut app = app();
    app.status.busy = true;
    let action = app
        .handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .expect("key handled");
    assert!(action.is_none());
    assert!(app.picker.is_none());
}

// --- vim modal editing ---

fn vim_app() -> App {
    let mut app = app();
    app.toggle_vim();
    assert!(app.vim.enabled);
    assert_eq!(app.vim.mode, VimMode::Insert);
    app
}

#[test]
fn esc_enters_normal_x_deletes_and_i_returns_to_insert() {
    let mut app = vim_app();
    type_str(&mut app, "hello");
    assert_eq!(app.vim.mode, VimMode::Insert);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.vim.mode, VimMode::Normal);
    // Leaving insert nudges the cursor left onto the last char ('o').
    assert_eq!(app.cursor, 4);
    // In normal mode 'x' deletes the char under the cursor, not insert 'x'.
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(app.input, "hell");
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.vim.mode, VimMode::Insert);
}

#[test]
fn word_motions_and_dw_in_normal_mode() {
    let mut app = vim_app();
    type_str(&mut app, "foo bar baz");
    press(&mut app, KeyCode::Esc); // normal, cursor on last 'z'
    press(&mut app, KeyCode::Char('0')); // start of line
    assert_eq!(app.cursor, 0);
    press(&mut app, KeyCode::Char('w')); // -> "bar"
    assert_eq!(app.cursor, 4);
    // dw deletes the word + trailing space.
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('w'));
    assert_eq!(app.input, "foo baz");
}

#[test]
fn insert_transitions_append() {
    let mut app = vim_app();
    type_str(&mut app, "ab");
    press(&mut app, KeyCode::Esc); // normal, cursor on 'b' (index 1)
    press(&mut app, KeyCode::Char('0')); // index 0 ('a')
    press(&mut app, KeyCode::Char('a')); // insert after 'a'
    assert_eq!(app.vim.mode, VimMode::Insert);
    press(&mut app, KeyCode::Char('X'));
    assert_eq!(app.input, "aXb");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('A')); // append at end
    type_str(&mut app, "Z");
    assert_eq!(app.input, "aXbZ");
}

#[test]
fn dd_clears_line_and_u_undoes() {
    let mut app = vim_app();
    type_str(&mut app, "scratch");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('d'));
    assert_eq!(app.input, "");
    press(&mut app, KeyCode::Char('u'));
    assert_eq!(app.input, "scratch");
}

#[test]
fn count_prefix_repeats_motion() {
    let mut app = vim_app();
    type_str(&mut app, "abcdef");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('0'));
    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('l')); // 3 right -> index 3
    assert_eq!(app.cursor, 3);
    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char('x')); // delete 2 chars
    assert_eq!(app.input, "abcf");
}

#[test]
fn delete_then_paste_register() {
    let mut app = vim_app();
    type_str(&mut app, "ab");
    press(&mut app, KeyCode::Esc); // cursor on 'b' (index 1)
    press(&mut app, KeyCode::Char('0')); // index 0
    press(&mut app, KeyCode::Char('x')); // delete 'a' -> register "a", input "b"
    assert_eq!(app.input, "b");
    press(&mut app, KeyCode::Char('p')); // paste after 'b'
    assert_eq!(app.input, "ba");
}

#[test]
fn enter_submits_in_normal_mode() {
    let mut app = vim_app();
    type_str(&mut app, "/help");
    press(&mut app, KeyCode::Esc);
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Help))
    ));
    assert_eq!(app.input, "");
}

#[test]
fn disabled_vim_inserts_hjkl_literally() {
    let mut app = app(); // vim off
    type_str(&mut app, "hjkl");
    press(&mut app, KeyCode::Esc); // plain clear, not a mode switch
    assert_eq!(app.input, "");
}

// --- Shift/Alt+Enter newline ---

#[test]
fn shift_enter_inserts_newline_without_submitting() {
    let mut app = app();
    type_str(&mut app, "line one");
    let action = press_mod(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    assert!(action.is_none());
    type_str(&mut app, "line two");
    assert_eq!(app.input, "line one\nline two");
    // Nothing was submitted.
    assert!(!app.has_conversation());
}

#[test]
fn alt_enter_also_inserts_newline() {
    let mut app = app();
    type_str(&mut app, "a");
    press_mod(&mut app, KeyCode::Enter, KeyModifiers::ALT);
    type_str(&mut app, "b");
    assert_eq!(app.input, "a\nb");
}

#[test]
fn plain_enter_submits_multiline_input() {
    let mut app = app();
    type_str(&mut app, "first");
    press_mod(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_str(&mut app, "second");
    let action = press(&mut app, KeyCode::Enter);
    match action {
        Some(AppAction::Submit(prepared)) => {
            assert!(
                prepared.text.contains("first")
                    && prepared.text.contains('\n')
                    && prepared.text.contains("second")
            );
        }
        other => panic!("expected a submit action, got {other:?}"),
    }
    assert_eq!(app.input, "");
}

#[test]
fn shift_enter_inserts_newline_in_vim_normal_mode() {
    let mut app = vim_app();
    type_str(&mut app, "xy");
    press(&mut app, KeyCode::Esc); // NORMAL, cursor on the last char
    let action = press_mod(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    // A break is inserted (never submits); the cursor sits on a char, so it
    // lands before it rather than at the very end.
    assert!(action.is_none());
    assert!(app.input.contains('\n'));
    assert_eq!(app.input.chars().filter(|c| !c.is_whitespace()).count(), 2);
}

// ---- Subagent rail ---------------------------------------------------

#[test]
fn subagent_run_events_build_a_pane() {
    let mut app = app_with_panes(1);
    assert_eq!(app.panes.len(), 1);
    assert_eq!(app.panes[0].name, "agent0");
    assert_eq!(app.panes[0].status, PaneStatus::Running);

    app.handle_agent_event(AgentEvent::SubagentRunToolStarted {
        run: 0,
        name: "read_file".to_string(),
        args: serde_json::json!({"path": "src/app.rs"}),
    });
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "found it".to_string(),
    });

    // The subagent's work lands in *its* pane, not the main transcript.
    assert_eq!(app.panes[0].transcript.len(), 2);
    assert!(app.transcript.is_empty());
    // …and it is flagged as unread, since the user is not watching it.
    assert_eq!(app.panes[0].unread, 2);
}

#[test]
fn concurrent_runs_of_one_subagent_stay_in_separate_panes() {
    let mut app = app();
    for run in [7, 9] {
        app.handle_agent_event(AgentEvent::SubagentRunStarted {
            run,
            bg: None,
            name: "worker".to_string(),
            task: format!("task {run}"),
        });
    }
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 9,
        text: "from the second".to_string(),
    });

    assert_eq!(app.panes.len(), 2);
    assert!(app.panes[0].transcript.is_empty());
    assert_eq!(app.panes[1].transcript.len(), 1);
}

#[test]
fn tool_output_lands_on_the_panes_open_card() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunToolStarted {
        run: 0,
        name: "read_file".to_string(),
        args: Value::Null,
    });
    app.handle_agent_event(AgentEvent::SubagentRunToolFinished {
        run: 0,
        name: "read_file".to_string(),
        output: crate::tools::ToolOutput::ok("contents"),
    });

    assert_eq!(app.panes[0].transcript.len(), 1);
    let Some(TranscriptItem::Tool(tool)) = app.panes[0].transcript.get(0) else {
        panic!("expected a tool card");
    };
    assert_eq!(
        tool.output.as_ref().map(|output| output.content.as_str()),
        Some("contents")
    );
}

#[test]
fn down_from_the_composer_focuses_the_rail_then_enter_attaches() {
    let mut app = app_with_panes(2);
    assert_eq!(app.rail_focus, None);

    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(0));

    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(1));
    // Clamped at the bottom rather than wrapping — you cannot fall off.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(1));

    press(&mut app, KeyCode::Enter);
    assert_eq!(app.attached, Some(1));
    assert_eq!(
        app.attached_pane().map(|pane| pane.name.as_str()),
        Some("agent1")
    );

    // Esc backs out to the main chat, all the way to the composer.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.attached, None);
    assert_eq!(app.rail_focus, None);
}

#[test]
fn up_off_the_top_of_the_rail_returns_to_the_composer() {
    let mut app = app_with_panes(2);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(0));

    press(&mut app, KeyCode::Up);
    assert_eq!(app.rail_focus, None);

    // Focus really is back in the composer: typing goes to the input.
    press(&mut app, KeyCode::Char('h'));
    assert_eq!(app.input, "h");
}

#[test]
fn down_still_walks_history_when_there_are_no_subagents() {
    let mut app = app();
    app.history.push("earlier".to_string());
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "earlier");
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, None);
    assert!(app.input.is_empty());
}

#[test]
fn typing_on_the_rail_hands_focus_back_to_the_composer() {
    let mut app = app_with_panes(1);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(0));

    // The keystroke must not be swallowed by the rail.
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(app.rail_focus, None);
    assert_eq!(app.input, "x");
}

#[test]
fn vim_normal_j_and_k_drive_the_rail_like_arrows() {
    let mut app = app_with_panes(2);
    app.toggle_vim();
    press(&mut app, KeyCode::Esc);
    assert!(app.vim.is_normal());

    // j from the composer drops into the rail, then walks it down,
    // clamping at the bottom just like ↓.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(0));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(1));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(1));

    // k walks back up; off the top it returns to the composer.
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.rail_focus, Some(0));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.rail_focus, None);

    // Insert mode is still text: on the rail, j hands focus back and
    // types.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(0));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.rail_focus, None);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.input, "j");
}

#[test]
fn vim_normal_j_finishes_history_before_dropping_into_the_rail() {
    let mut app = app_with_panes(1);
    app.toggle_vim();
    app.history.push("earlier".to_string());
    press(&mut app, KeyCode::Esc);

    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.input, "earlier");
    // Mid-history j walks forward (back to the empty draft) rather than
    // jumping to the rail.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, None);
    assert!(app.input.is_empty());
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(0));
}

#[test]
fn vim_normal_j_keeps_walking_subagents_while_attached() {
    let mut app = app_with_panes(2);
    app.toggle_vim();
    press(&mut app, KeyCode::Esc);
    app.attach_pane(0);

    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.attached, Some(1));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.attached, Some(0));

    // In insert mode the composer under the pane is live again: j types.
    press(&mut app, KeyCode::Char('i'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.attached, Some(0));
    assert_eq!(app.input, "j");
}

#[test]
fn attaching_clears_the_unread_badge_and_live_entries_stay_read() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "one".to_string(),
    });
    assert_eq!(app.panes[0].unread, 1);

    app.attach_pane(0);
    assert_eq!(app.panes[0].unread, 0);

    // While you are watching, new work is not "unread".
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "two".to_string(),
    });
    assert_eq!(app.panes[0].unread, 0);
}

#[test]
fn run_done_retires_the_pane() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "report".to_string(),
        steps_used: 3,
        error: None,
    });
    assert_eq!(app.panes[0].status, PaneStatus::Done);
    assert!(app.panes[0].finished.is_some());
    assert_eq!(app.running_panes(), 0);
}

#[test]
fn the_final_report_lands_in_the_pane() {
    let mut app = app_with_panes(1);
    // The report is the step that made no tool call, so the sub-loop ends
    // on it and never streams it as text — it only arrives on the Done
    // event. The pane must still show it.
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "the auth flow starts in login.rs".to_string(),
        steps_used: 2,
        error: None,
    });
    let Some(TranscriptItem::Text(text)) = app.panes[0].transcript.last() else {
        panic!("expected the report as an assistant message");
    };
    assert_eq!(text, "the auth flow starts in login.rs");
    assert_eq!(app.panes[0].activity(), "the auth flow starts in login.rs");
}

#[test]
fn the_report_is_not_duplicated_when_it_also_streamed() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "all done".to_string(),
    });
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "all done".to_string(),
        steps_used: 1,
        error: None,
    });
    assert_eq!(app.panes[0].transcript.len(), 1);
}

#[test]
fn a_failed_run_shows_its_error_in_the_pane() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: false,
        output: String::new(),
        steps_used: 1,
        error: Some("provider is down".to_string()),
    });
    assert_eq!(app.panes[0].status, PaneStatus::Failed);
    let Some(TranscriptItem::Notice(text)) = app.panes[0].transcript.get(0) else {
        panic!("expected a notice");
    };
    assert!(text.contains("provider is down"));
}

#[test]
fn focus_rail_prefers_a_running_pane_over_a_finished_one() {
    let mut app = app_with_panes(2);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "done".to_string(),
        steps_used: 1,
        error: None,
    });
    // agent0 has finished; ↓ should land on the one still working.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(1));
}

#[test]
fn arrows_walk_from_one_pane_straight_into_the_next() {
    let mut app = app_with_panes(3);
    app.attach_pane(0);

    press(&mut app, KeyCode::Down);
    assert_eq!(app.attached, Some(1));
    press(&mut app, KeyCode::Down);
    assert_eq!(app.attached, Some(2));
    // Wraps rather than dead-ending at the last run.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.attached, Some(0));

    press(&mut app, KeyCode::Up);
    assert_eq!(app.attached, Some(2));
    // Browsing runs never scrolls the one you passed through.
    assert!(app.panes.iter().all(|pane| pane.transcript.scroll == 0));
}

#[test]
fn shift_arrows_scroll_the_pane_you_are_reading() {
    let mut app = app_with_panes(3);
    app.attach_pane(1);
    // Pretend the last frame had room to scroll (renderer fills this).
    app.panes[1].transcript.max_scroll.set(100);

    press_mod(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    press_mod(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert_eq!(app.attached, Some(1), "shift+↑ must not change pane");
    assert!(
        !app.panes[1].transcript.follow,
        "scrolling up leaves the tail"
    );
    assert_eq!(
        app.panes[1].transcript.scroll, 98,
        "top-anchored: two lines up from max"
    );

    press_mod(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
    assert_eq!(app.panes[1].transcript.scroll, 99);
    assert!(!app.panes[1].transcript.follow);
}

#[test]
fn arrows_in_a_pane_scroll_it_instead_of_recalling_history() {
    let mut app = app_with_panes(1);
    app.history.push("an earlier prompt".to_string());
    app.attach_pane(0);
    app.panes[0].transcript.max_scroll.set(100);

    // The bug: ↑/↓ fell through to the composer and walked the main chat's
    // history while the user was plainly looking at a subagent.
    press(&mut app, KeyCode::Up);
    assert!(app.input.is_empty(), "↑ must not recall history in a pane");
    assert!(!app.panes[0].transcript.follow);
    assert_eq!(app.panes[0].transcript.scroll, 99);

    press(&mut app, KeyCode::Up);
    assert_eq!(app.panes[0].transcript.scroll, 98);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.panes[0].transcript.scroll, 99);
    // Pinned at the live tail; it cannot scroll past the bottom.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert!(
        app.panes[0].transcript.follow,
        "reaching the bottom re-follows"
    );
    assert_eq!(app.panes[0].transcript.scroll, 0);
    assert!(app.input.is_empty());
    assert_eq!(app.attached, Some(0));
}

#[test]
fn scroll_step_clamps_and_tracks_follow() {
    // Following the tail: scrolling up unsticks and moves off the bottom.
    assert_eq!(scroll_step(true, 0, 10, 3), (7, false));
    // Scrolling further up clamps at the oldest content.
    assert_eq!(scroll_step(false, 2, 10, 5), (0, false));
    // Scrolling down past the bottom clamps and re-enables follow.
    assert_eq!(scroll_step(false, 8, 10, -5), (0, true));
    // While following, scrolling down stays stuck to the bottom.
    assert_eq!(scroll_step(true, 0, 10, -1), (0, true));
}

#[test]
fn transcript_stays_put_while_streaming_after_scroll_up() {
    let mut app = app();
    // Viewport is full and we are following the live tail.
    app.transcript.max_scroll.set(50);
    assert!(app.transcript.follow);

    // User scrolls up to re-read earlier output.
    app.scroll_transcript(10);
    assert!(!app.transcript.follow);
    assert_eq!(app.transcript.scroll, 40);

    // Content grows (renderer would bump max_scroll); the top-anchored
    // offset must not change — that is the whole stick-to-bottom contract.
    app.transcript.max_scroll.set(80);
    assert_eq!(
        app.transcript.scroll, 40,
        "scroll offset holds while content grows"
    );
    assert!(!app.transcript.follow);

    // Scrolling down to the (new) bottom re-enables follow.
    app.scroll_transcript(-100);
    assert!(app.transcript.follow);
    assert_eq!(app.transcript.scroll, 0);

    // Ctrl-End is the explicit jump-to-tail chord.
    app.scroll_transcript(5);
    assert!(!app.transcript.follow);
    app.scroll_to_bottom();
    assert!(app.transcript.follow);
    assert_eq!(app.transcript.scroll, 0);
}

#[test]
fn wheel_and_page_keys_drive_stick_to_bottom() {
    let mut app = app();
    app.transcript.max_scroll.set(30);

    press(&mut app, KeyCode::PageUp);
    assert!(!app.transcript.follow);
    assert_eq!(app.transcript.scroll, 20);

    // One PgDn of 10 lands exactly on the bottom and re-enables follow.
    press(&mut app, KeyCode::PageDown);
    assert!(
        app.transcript.follow,
        "PgDn onto the bottom should re-enable follow"
    );
    assert_eq!(app.transcript.scroll, 0);

    // Esc while scrolled away jumps to the tail (instead of clearing input).
    app.scroll_transcript(5);
    assert!(!app.transcript.follow);
    press(&mut app, KeyCode::Esc);
    assert!(app.transcript.follow);

    // Ctrl-End does the same.
    app.scroll_transcript(5);
    press_mod(&mut app, KeyCode::End, KeyModifiers::CONTROL);
    assert!(app.transcript.follow);
}

#[test]
fn esc_from_a_pane_lands_in_the_composer_in_one_press() {
    let mut app = app_with_panes(2);
    app.attach_pane(1);

    press(&mut app, KeyCode::Esc);
    assert_eq!(app.attached, None);
    // Focus is all the way back in the composer, not parked on the rail —
    // one Esc, and you are typing again.
    assert_eq!(app.rail_focus, None);
    press(&mut app, KeyCode::Char('h'));
    assert_eq!(app.input, "h");
}

#[test]
fn an_aborted_turn_closes_out_the_panes_it_left_running() {
    let mut app = app_with_panes(3);
    // The first run finished before the interrupt; the other two were still
    // streaming when the task was killed, so their loops were dropped
    // mid-poll and no SubagentRunDone is ever coming for them.
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "report".to_string(),
        steps_used: 1,
        error: None,
    });

    app.fail_running_panes("interrupted");

    assert_eq!(
        app.running_panes(),
        0,
        "nothing is left pulsing on the rail"
    );
    assert_eq!(
        app.panes[0].status,
        PaneStatus::Done,
        "the finished one is untouched"
    );
    for pane in &app.panes[1..] {
        assert_eq!(pane.status, PaneStatus::Failed);
        assert!(pane.finished.is_some(), "so its linger clock can start");
    }

    // And they retire like any other finished run, instead of sitting on the
    // rail with a live clock for the rest of the session.
    for pane in &mut app.panes {
        pane.finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
    }
    app.retire_finished_panes();
    assert!(app.panes.is_empty());
}

#[test]
fn the_ultra_pre_phase_leaves_its_drafts_in_the_transcript() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::UltraGuidance {
        label: "ultra ×2 · implementer+skeptic · 1 judge".to_string(),
        guidance: "[Ultra] 2 agent(s)…\n\ndraft from the implementer".to_string(),
    });

    // The candidates' panes retire seconds after they finish, while the main
    // agent works on for minutes — so the card is the only place the drafts
    // the user paid 3× for can still be read.
    let card = app
        .transcript
        .iter()
        .enumerate()
        .find_map(|(index, item)| match item {
            TranscriptItem::Tool(tool) => Some((
                tool.name.clone(),
                tool.output.as_ref().map(|output| output.content.clone()),
                app.transcript.folded(index),
            )),
            _ => None,
        })
        .expect("the guidance card");
    assert_eq!(card.0, "ultra ×2 · implementer+skeptic · 1 judge");
    assert!(
        card.1
            .as_deref()
            .is_some_and(|body| body.contains("draft from the implementer"))
    );
    assert!(card.2, "folded: the answer is the point of the turn");
}

#[test]
fn a_finished_run_retires_off_the_rail() {
    let mut app = app_with_panes(2);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "report".to_string(),
        steps_used: 1,
        error: None,
    });
    // It lingers first, so you actually see it land.
    app.retire_finished_panes();
    assert_eq!(app.panes.len(), 2);

    // Once its linger is up it drops off, leaving the rail showing live work.
    app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
    app.retire_finished_panes();
    assert_eq!(app.panes.len(), 1);
    assert_eq!(app.panes[0].name, "agent1");
    assert_eq!(app.running_panes(), 1);
}

#[test]
fn the_pane_you_are_watching_never_retires_under_you() {
    let mut app = app_with_panes(1);
    app.attach_pane(0);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "report".to_string(),
        steps_used: 1,
        error: None,
    });
    app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));

    // Long past its linger, but you are reading it.
    app.retire_finished_panes();
    assert_eq!(app.panes.len(), 1);
    assert_eq!(app.attached, Some(0));

    // Esc lets it go, and lands you back in the composer.
    press(&mut app, KeyCode::Esc);
    assert!(app.panes.is_empty());
    assert_eq!(app.attached, None);
    assert_eq!(app.rail_focus, None);
}

#[test]
fn retiring_keeps_the_selection_on_the_run_it_pointed_at() {
    let mut app = app_with_panes(3);
    // Focus the third run, then retire the first.
    app.rail_focus = Some(2);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "done".to_string(),
        steps_used: 1,
        error: None,
    });
    app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
    app.retire_finished_panes();

    // Indices shifted, but the selection still points at the same run.
    assert_eq!(app.panes.len(), 2);
    assert_eq!(app.rail_focus, Some(1));
    assert_eq!(app.panes[1].name, "agent2");
}

#[test]
fn a_background_report_survives_its_pane_retiring() {
    let mut app = app_with_panes(1);
    // The card the model got back when it delegated: a placeholder.
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "spawn_subagent".to_string(),
        args: serde_json::json!({"subagent": "agent0", "task": "task 0"}),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "spawn_subagent".to_string(),
        output: crate::tools::ToolOutput::ok("Delegated to subagent 'agent0' (#0)"),
    });
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "the auth flow starts in login.rs".to_string(),
        steps_used: 4,
        error: None,
    });
    app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
    app.retire_finished_panes();
    assert!(app.panes.is_empty());

    // The pane is gone, but the run is still readable in the main chat.
    let Some(TranscriptItem::Tool(tool)) = app.transcript.get(0) else {
        panic!("expected the spawn card");
    };
    assert_eq!(
        tool.output.as_ref().map(|output| output.content.as_str()),
        Some("the auth flow starts in login.rs")
    );
}

#[test]
fn the_composer_stays_live_while_attached() {
    let mut app = app_with_panes(1);
    app.attach_pane(0);
    // Better than a modal: you can keep driving the main conversation
    // while you watch a subagent work.
    press(&mut app, KeyCode::Char('h'));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.input, "hi");
    assert_eq!(app.attached, Some(0));
}

#[test]
fn activity_reports_the_tool_in_flight_then_the_last_message() {
    let mut app = app_with_panes(1);
    // Nothing yet: fall back to the task.
    assert_eq!(app.panes[0].activity(), "task 0");

    app.handle_agent_event(AgentEvent::SubagentRunToolStarted {
        run: 0,
        name: "grep".to_string(),
        args: Value::Null,
    });
    assert_eq!(app.panes[0].activity(), "grep");

    app.handle_agent_event(AgentEvent::SubagentRunToolFinished {
        run: 0,
        name: "grep".to_string(),
        output: crate::tools::ToolOutput::ok("hit"),
    });
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "narrowing it down".to_string(),
    });
    assert_eq!(app.panes[0].activity(), "narrowing it down");
}

#[test]
fn bare_commands_parse_to_their_variants() {
    for (input, expected) in [
        ("/plan", SlashCommand::Plan),
        ("/todos", SlashCommand::Todos),
        ("/cost", SlashCommand::Cost),
        ("/compact", SlashCommand::Compact),
        ("/dashboard", SlashCommand::Dashboard),
        ("/omakase", SlashCommand::Omakase),
    ] {
        assert_eq!(SlashCommand::parse(input), Some(Ok(expected)), "{input}");
    }
}

#[test]
fn welcome_hides_while_a_turn_is_in_flight_and_returns_after() {
    let mut app = app();
    assert!(app.welcome_visible());

    app.status.busy = true;
    assert!(
        !app.welcome_visible(),
        "a running turn replaces the welcome"
    );
    app.status.busy = false;
    assert!(app.welcome_visible(), "an aborted turn brings it back");

    app.handle_agent_event(AgentEvent::TextDelta("partial".to_string()));
    assert!(!app.welcome_visible(), "streamed text replaces the welcome");
    app.handle_agent_event(AgentEvent::StreamRetrying);
    assert!(app.welcome_visible());
}

#[test]
fn builtin_with_bad_args_still_dismisses_the_welcome_screen() {
    let mut app = app();
    assert!(app.welcome_visible());
    type_str(&mut app, "/mode warlock");
    press(&mut app, KeyCode::Enter);
    assert!(
        !app.welcome_visible(),
        "a mistyped builtin still begins the session"
    );
}

// ---------------------------------------------------------------------------
// Theme tokens
// ---------------------------------------------------------------------------

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::style::Color;

use crate::theme::{self, ColorDepth, Theme, Token};

/// One frame of state that paints every element type the TUI has: each
/// transcript entry kind, the tool-card lifecycle (running / done / failed),
/// notices both plain and error, markdown (heading, prose, inline code, link,
/// quote, list, fenced code), the todo band, the subagent rail in all three
/// states, the `/diff` sidebar with all four line kinds, the composer, and a
/// status bar carrying the sovereign warning.
///
/// It is the fixture the per-theme snapshots render, so anything added to the
/// UI belongs here too: a token that nothing paints is a token nobody tests.
fn themed_fixture() -> App {
    use crate::agent::session::{SessionEntry, SessionRecord, TurnMarker};
    use crate::llm::ChatMessage;

    let mut app = app();
    app.welcome_dismissed = true;
    app.tick = 0;
    // Sovereign is the mode the status bar renders as a warning.
    app.status.mode = Mode::Sovereign;
    app.config.mode = Mode::Sovereign;

    // The turn boundary and the prompt come in by the replay door, because a
    // `TurnMarker` only exists in a session file — the fixture is a session,
    // resumed, and then continued live, which is the pair of doors the whole
    // model exists to keep in agreement.
    app.load_transcript(&[
        SessionEntry::Marker(TurnMarker {
            timestamp: chrono::Utc::now(),
            turn: 1,
            prompt: "show me the theme".to_string(),
        }),
        SessionEntry::Message(SessionRecord {
            timestamp: chrono::Utc::now(),
            message: ChatMessage::user("show me the theme"),
            system_note: false,
        }),
    ]);

    // …and the rest live, which is the only way to get reasoning and a tool
    // call caught mid-flight.
    app.handle_agent_event(AgentEvent::ThinkingDelta("weighing options".to_string()));
    app.handle_agent_event(AgentEvent::TextDelta(
        "# Heading\n\n\
         Body prose.\n\n\
         Use `wizardry` now.\n\n\
         See [docs](http://x.test).\n\n\
         > quoted line\n\n\
         - bullet\n\n\
         ```rust\nfn main() {}\n```\n"
            .to_string(),
    ));
    app.handle_agent_event(AgentEvent::Images {
        source: ImageSource::Tool("shot".to_string()),
        images: vec![ImageRef {
            path: PathBuf::from("/img/theme.png"),
            mime: "image/png".to_string(),
            bytes: 2048,
        }],
    });
    // Running, done, failed — the whole tool-card lifecycle.
    for (name, output) in [
        ("probe", None),
        ("inspect", Some(crate::tools::ToolOutput::ok("output line"))),
        ("explode", Some(crate::tools::ToolOutput::error("it broke"))),
    ] {
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: name.to_string(),
            args: serde_json::Value::Null,
        });
        if let Some(output) = output {
            app.handle_agent_event(AgentEvent::ToolFinished {
                name: name.to_string(),
                output,
            });
        }
    }
    app.handle_agent_event(AgentEvent::Notice("plain notice".to_string()));
    app.handle_agent_event(AgentEvent::Error("something broke".to_string()));

    use crate::tools::todo::{TodoItem, TodoStatus};
    app.show_todos = true;
    app.todos = vec![
        TodoItem {
            content: "done item".to_string(),
            status: TodoStatus::Completed,
        },
        TodoItem {
            content: "current item".to_string(),
            status: TodoStatus::InProgress,
        },
        TodoItem {
            content: "later item".to_string(),
            status: TodoStatus::Pending,
        },
    ];

    for (run, name) in [(0u64, "running"), (1, "finished"), (2, "broken")] {
        app.handle_agent_event(AgentEvent::SubagentRunStarted {
            run,
            bg: Some(run as u32),
            name: name.to_string(),
            task: format!("task {run}"),
        });
    }
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 1,
        completed: true,
        output: "all good".to_string(),
        steps_used: 1,
        error: None,
    });
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 2,
        completed: false,
        output: String::new(),
        steps_used: 1,
        error: Some("died".to_string()),
    });

    app.diff = Some(DiffPane {
        text: "+++ b/file.txt\n@@ -1,2 +1,2 @@\n+added line\n-removed line\n context\n".to_string(),
        scroll: 0,
    });
    app
}

/// The fixture is only worth snapshotting if it still contains one of
/// everything, and "everything" is now a closed set: [`TranscriptItem`]'s
/// variants. A variant added to the model and never drawn would otherwise
/// sail through both theme snapshots and both degraded-color tests, which is
/// exactly how the last renderer grew a case nobody had ever looked at.
#[test]
fn the_theme_fixture_contains_every_transcript_item_variant() {
    let app = themed_fixture();
    let mut seen = [false; 7];
    for item in app.transcript.iter() {
        let slot = match item {
            TranscriptItem::TurnMarker { .. } => 0,
            TranscriptItem::User { .. } => 1,
            TranscriptItem::Text(_) => 2,
            TranscriptItem::Thinking(_) => 3,
            TranscriptItem::Tool(_) => 4,
            TranscriptItem::Images { .. } => 5,
            TranscriptItem::Notice(_) => 6,
        };
        seen[slot] = true;
    }
    assert!(
        seen.iter().all(|found| *found),
        "the fixture is missing a variant ({seen:?}): {:?}",
        app.transcript.items()
    );
}

/// The distinct colors a frame painted, sorted and formatted stably.
///
/// Syntect's grayscale ramp collapses into a single `rgb` entry: it is the one
/// color in the UI computed rather than named by a token (see
/// `ui::syntect_style`), so a snapshot that listed every shade would break on
/// any highlighting change while saying nothing about the theme.
fn palette(buf: &Buffer) -> Vec<String> {
    let mut seen: Vec<String> = buf
        .content()
        .iter()
        .flat_map(|cell| [cell.fg, cell.bg])
        .map(|color| match color {
            Color::Rgb(..) => "rgb".to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    seen.sort();
    seen.dedup();
    seen
}

/// Foreground color of the cell `offset` columns into the first occurrence of
/// `needle` on screen. Panics with the screen contents when the needle is not
/// there, because a fixture that stopped rendering an element would otherwise
/// pass a color assertion vacuously.
fn fg_at(buf: &Buffer, needle: &str, offset: u16) -> Color {
    for y in 0..buf.area.height {
        let row = row_text(buf, y);
        if let Some(byte) = row.find(needle) {
            let column = row[..byte].chars().count() as u16 + offset;
            return buf.cell((column, y)).expect("cell on screen").fg;
        }
    }
    let screen: Vec<String> = (0..buf.area.height).map(|y| row_text(buf, y)).collect();
    panic!("'{needle}' is not on screen:\n{}", screen.join("\n"));
}

/// Every element of the fixture that carries a token, as
/// `(needle, offset, token)`. Shared by both theme snapshots so the two are
/// the same test with a different palette behind it.
const TOKEN_SITES: &[(&str, u16, Token)] = &[
    ("show me the theme", 0, Token::Muted),
    ("Heading", 0, Token::Heading),
    ("Body prose.", 0, Token::Text),
    ("wizardry", 0, Token::Code),
    ("(http://x.test)", 0, Token::Link),
    ("quoted line", 0, Token::Quote),
    ("weighing options", 0, Token::Faint),
    ("⠋ probe", 0, Token::ToolRunning),
    ("✓ inspect", 0, Token::ToolDone),
    ("✗ explode", 0, Token::ToolFailed),
    ("output line", 0, Token::Muted),
    ("plain notice", 0, Token::Faint),
    ("error: something broke", 0, Token::Error),
    ("sovereign", 0, Token::Warning),
    ("≡", 0, Token::Accent),
    ("git diff", 0, Token::Muted),
    ("+++ b/file.txt", 0, Token::DiffMeta),
    ("@@ -1,2", 0, Token::DiffHunk),
    ("+added line", 0, Token::DiffAdd),
    ("-removed line", 0, Token::DiffDel),
];

/// Render the fixture under `name` and assert every element took its token's
/// color from the theme file rather than from a literal in the renderer.
fn assert_fixture_paints_tokens(name: &str) -> Buffer {
    let theme = Arc::new(theme::load(name).expect("theme loads"));
    let _pinned = theme::pin(theme.clone());
    let app = themed_fixture();
    let buf = screen(&app, 100, 40);
    for (needle, offset, token) in TOKEN_SITES {
        assert_eq!(
            fg_at(&buf, needle, *offset),
            theme.color(*token),
            "'{needle}' should paint with the '{}' token under {name}",
            token.key()
        );
    }
    buf
}

#[test]
fn minimal_theme_snapshot_over_the_fixture_transcript() {
    let buf = assert_fixture_paints_tokens("minimal");
    // The default look: greys plus one white accent, hues only where a diff
    // makes them conventional. `rgb` is the syntax highlighter's grey ramp.
    assert_eq!(
        palette(&buf),
        [
            "DarkGray".to_string(),
            "Gray".to_string(),
            "Green".to_string(),
            "Red".to_string(),
            "Reset".to_string(),
            "White".to_string(),
            "rgb".to_string(),
        ]
    );
    // Borders are theme data too, not only colors.
    let rows: Vec<String> = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
    assert!(
        rows.iter().any(|row| row.contains('╭')),
        "minimal draws rounded borders"
    );
}

#[test]
fn a_skin_palette_snapshot_over_the_fixture_transcript() {
    let buf = assert_fixture_paints_tokens("codex");
    // Same renderer, same fixture, an entirely different palette: this is the
    // whole claim of the token layer, and nothing in `ui.rs` changed to get it.
    assert_eq!(
        palette(&buf),
        [
            "Blue".to_string(),
            "Cyan".to_string(),
            "DarkGray".to_string(),
            "Gray".to_string(),
            "Green".to_string(),
            "Red".to_string(),
            "Reset".to_string(),
            "White".to_string(),
            "Yellow".to_string(),
            "rgb".to_string(),
        ],
        "every color the palette defines reaches the screen, and no color it \
         does not define does: a token that stopped being asked for would drop \
         out of this list rather than fail somewhere only the eye would catch it"
    );
    let rows: Vec<String> = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
    // The palette carries the border style too, so this is the same claim as
    // the colors above in a different medium: codex draws plain corners where
    // minimal draws rounded ones, and neither renderer knows the difference.
    assert!(
        rows.iter().any(|row| row.contains('┌')),
        "codex draws plain borders"
    );
    assert!(
        !rows.iter().any(|row| row.contains('╭')),
        "and none of minimal's rounded ones survive the swap"
    );
}

/// 80×24 is the floor a terminal is allowed to be, and the fixture is a busy
/// frame: a diff sidebar, a todo band, a three-run rail and a status bar all
/// competing for the same 24 rows. Every one of those is a *reserved* layout
/// band rather than a floating panel, so the failure this catches is not
/// ugliness — it is a band claiming rows that no longer exist and the
/// transcript rendering into nothing.
///
/// Checked against 100×40 rather than asserted in the abstract, so "it still
/// renders" cannot pass by rendering an empty screen.
#[test]
fn the_fixture_survives_the_smallest_terminal_under_both_themes() {
    for name in ["minimal", "codex"] {
        let _pinned = theme::pin(Arc::new(theme::load(name).expect("theme loads")));
        let app = themed_fixture();
        let buf = screen(&app, 80, 24);
        let rows: Vec<String> = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        let screen_text = rows.join("\n");

        // Nothing painted outside the 80 columns it was given.
        assert_eq!(buf.area.width, 80);
        assert!(
            rows.iter().all(|row| row.chars().count() == 80),
            "{name} at 80×24 produced a ragged frame:\n{screen_text}"
        );
        // Every layout band still has its say: the chat (pinned to its live
        // tail, so it is the newest rows that have to be there, not the
        // prompt that has scrolled off), the diff beside it, the todos, the
        // rail, the composer, the status line.
        for needle in [
            "error: something broke",
            "✗ explode",
            "git diff",
            "todos",
            "running",
            "❯",
            "sovereign",
        ] {
            assert!(
                screen_text.contains(needle),
                "{name} at 80×24 lost '{needle}':\n{screen_text}"
            );
        }
        // And it is a different frame from the roomy one, so the assertions
        // above are about this size rather than about the fixture.
        let wide: Vec<String> = {
            let buf = screen(&app, 100, 40);
            (0..buf.area.height).map(|y| row_text(&buf, y)).collect()
        };
        assert_ne!(rows, wide, "{name} ignored the terminal size");
    }
}

#[test]
fn the_low_color_fallback_paints_only_sixteen_color_values() {
    // What Windows ConHost gets. Every cell of a full frame, including the
    // syntax highlighter's own greys, has to land inside the 16-color palette:
    // one stray 24-bit escape and the whole screen fills with garbage.
    for name in ["minimal", "codex"] {
        let theme = theme::load(name)
            .expect("loads")
            .with_depth(ColorDepth::Ansi16);
        let _pinned = theme::pin(Arc::new(theme));
        let app = themed_fixture();
        let buf = screen(&app, 100, 40);
        for cell in buf.content() {
            assert!(
                theme::is_ansi16(cell.fg) && theme::is_ansi16(cell.bg),
                "{name} painted {:?}/{:?}, which is outside the 16-color palette",
                cell.fg,
                cell.bg
            );
        }
    }
}

#[test]
fn no_color_paints_nothing_but_still_renders_every_element() {
    // `NO_COLOR` / `TERM=dumb`: meaning has to survive on glyphs and bold
    // alone, so the elements must all still be on screen.
    let theme = theme::load("codex")
        .expect("loads")
        .with_depth(ColorDepth::Mono);
    let _pinned = theme::pin(Arc::new(theme));
    let app = themed_fixture();
    let buf = screen(&app, 100, 40);
    for cell in buf.content() {
        assert_eq!(cell.fg, Color::Reset);
    }
    for (needle, _, _) in TOKEN_SITES {
        let rows: Vec<String> = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        assert!(
            rows.iter().any(|row| row.contains(needle)),
            "'{needle}' vanished with the colors"
        );
    }
}

#[test]
fn a_theme_swap_repaints_the_next_frame() {
    // A palette swap has no cache to invalidate: styles resolve per span, per frame.
    // The syntax-highlight cache is the one exception, and it is keyed by the
    // active theme, which is why the code block moves too.
    let _pinned = theme::pin(theme::minimal());
    let app = themed_fixture();
    let before = palette(&screen(&app, 100, 40));
    theme::set_active_by_name("codex").expect("swaps");
    let after = palette(&screen(&app, 100, 40));
    assert_ne!(before, after, "the frame kept its old palette");
    assert!(
        after.iter().any(|color| color != "Reset"),
        "the new palette never reached the screen: {after:?}"
    );
}

#[test]
fn the_theme_a_session_starts_with_follows_the_resolution_order() {
    // The full order lives in `theme::resolve_name` (config > env > default);
    // this pins the wiring: a session that names no theme gets the default,
    // and every token it paints comes from that theme's file.
    let default = theme::load(theme::DEFAULT_THEME).expect("default loads");
    let _pinned = theme::pin(Arc::new(default.clone()));
    let app = App::new(Config::default());
    let buf = screen(&app, 60, 12);
    let known: Vec<Color> = Token::ALL
        .into_iter()
        .map(|token| default.color(token))
        .collect();
    for cell in buf.content() {
        assert!(
            cell.fg == Color::Reset || known.contains(&cell.fg),
            "the welcome screen painted {:?}, which no token names",
            cell.fg
        );
    }
    assert_eq!(app.transcript.len(), 0, "a loadable theme says nothing");
}

/// A theme with no `[tokens]` table at all still renders: it inherits the
/// default's whole palette, which is what lets a user theme be three lines.
#[test]
fn a_theme_may_override_only_what_it_cares_about() {
    let source = "name = \"squint\"\ndescription = \"one change\"\n[tokens]\naccent = \"blue\"\n";
    let theme = Theme::parse("squint", source, &theme::minimal()).expect("parses");
    let _pinned = theme::pin(Arc::new(theme));
    let app = themed_fixture();
    let buf = screen(&app, 100, 40);
    assert_eq!(fg_at(&buf, "≡", 0), Color::Blue);
    assert_eq!(
        fg_at(&buf, "+added line", 0),
        theme::minimal().color(Token::DiffAdd),
        "an unspecified token still comes from the default"
    );
}

// ---------------------------------------------------------------------------
// A running command's console
// ---------------------------------------------------------------------------

/// Open a console the way a running `execute` does, returning the host end so
/// a test can read what the composer typed into the child.
fn open_console(app: &mut App, command: &str) -> crate::agent::ConsoleHost {
    let (gate, host) = crate::agent::ConsoleGate::open();
    // The command's card exists first: the dispatcher announces the call, then
    // the tool opens the console.
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "execute".to_string(),
        args: serde_json::json!({ "command": command }),
    });
    app.handle_agent_event(AgentEvent::ConsoleOpened {
        command: command.to_string(),
        gate,
    });
    // Opening only holds the writer. The composer changes hands when the
    // command actually asks something, which is what the tool reports here.
    app.handle_agent_event(AgentEvent::ConsoleWaiting { gate });
    host
}

/// The reported bug: `ls` took the composer away from the agent for as long as
/// it ran, even though it never asked anything. Opening a console must hold the
/// writer without touching where Enter goes.
#[test]
fn a_command_that_never_prompts_leaves_the_composer_alone() {
    let mut app = app();
    let (gate, _host) = crate::agent::ConsoleGate::open();
    app.handle_agent_event(AgentEvent::ConsoleOpened {
        command: "ls -la".to_string(),
        gate,
    });
    assert!(
        app.console.is_none(),
        "opening a console must not repoint the composer"
    );
    assert!(
        app.console_pending.is_some(),
        "but the writer is ours, ready for a question that may never come"
    );

    // Output alone is not a question: a command that writes whole lines and
    // exits is working, not waiting.
    app.handle_agent_event(AgentEvent::ConsoleOutput {
        gate,
        chunk: "total 8\n".to_string(),
    });
    assert!(app.console.is_none(), "output is not a prompt");

    app.handle_agent_event(AgentEvent::ConsoleClosed { gate });
    assert!(app.console_pending.is_none(), "the held writer goes too");
}

/// And the other half: once the command does ask, the composer changes hands
/// and says so.
#[test]
fn a_command_that_prompts_takes_the_composer_and_announces_it() {
    let mut app = app();
    let (gate, _host) = crate::agent::ConsoleGate::open();
    app.handle_agent_event(AgentEvent::ConsoleOpened {
        command: "npm init".to_string(),
        gate,
    });
    app.handle_agent_event(AgentEvent::ConsoleWaiting { gate });
    assert!(app.console.is_some(), "the composer claimed the console");
    assert!(
        app.console_pending.is_none(),
        "promoted, not duplicated: two writers for one child is the bug consoles fix"
    );
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptItem::Notice(text)) if text.contains("Enter now types into this command")
        ),
        "a composer that quietly meant something else would be the worse bug"
    );
}

/// A `ConsoleWaiting` for a command this composer never held must not hand it
/// a composer. Same rule as `ConsoleClosed`, and the same reason.
#[test]
fn a_prompt_from_a_console_we_never_held_is_ignored() {
    let mut app = app();
    let (ours, _ours_host) = crate::agent::ConsoleGate::open();
    let (theirs, _theirs_host) = crate::agent::ConsoleGate::open();
    app.handle_agent_event(AgentEvent::ConsoleOpened {
        command: "npm init".to_string(),
        gate: ours,
    });
    app.handle_agent_event(AgentEvent::ConsoleWaiting { gate: theirs });
    assert!(app.console.is_none(), "not our command, not our composer");
    assert!(app.console_pending.is_some(), "and ours is still held");
}

/// The reported bug, at the surface: Enter reaches the command instead of
/// being queued as a message to the agent.
#[test]
fn enter_types_into_the_running_command_not_the_agent() {
    let mut app = app();
    let mut host = open_console(&mut app, "npm init");
    assert!(app.console.is_some(), "the composer claimed the console");

    type_str(&mut app, "wizard");
    let action = press(&mut app, KeyCode::Enter);

    assert!(action.is_none(), "no turn is started by answering a prompt");
    assert!(
        app.message_queue.is_empty(),
        "nothing was queued for the agent"
    );
    assert_eq!(
        host.receive.try_recv(),
        Ok(crate::agent::ConsoleInput::Line("wizard".to_string()))
    );
    assert!(app.input.is_empty(), "the composer was cleared");
}

/// The exact keystroke from the report: Enter on an empty line, which is how a
/// person accepts `[Y/n]`. It must reach the child, not be swallowed as
/// "nothing to submit".
#[test]
fn a_bare_enter_is_sent_to_the_command_as_an_empty_line() {
    let mut app = app();
    let mut host = open_console(&mut app, "apt install ripgrep");
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        host.receive.try_recv(),
        Ok(crate::agent::ConsoleInput::Line(String::new()))
    );
}

/// A line goes to the child verbatim: no slash parsing, no trimming. An
/// installer asking for a prefix wants `/usr/local`, and `/usr/local` is not a
/// command.
#[test]
fn a_typed_line_reaches_the_command_unparsed() {
    let mut app = app();
    let mut host = open_console(&mut app, "./configure");
    type_str(&mut app, "/usr/local");
    assert!(
        app.suggestions.is_empty(),
        "no command popup may open over a console"
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        host.receive.try_recv(),
        Ok(crate::agent::ConsoleInput::Line("/usr/local".to_string()))
    );
}

/// The answer has to appear somewhere: a pipe does not echo the way a terminal
/// does, so the composer writes it into the command's card itself.
#[test]
fn an_answer_is_echoed_into_the_commands_card() {
    let mut app = app();
    let _host = open_console(&mut app, "npm init");
    app.handle_agent_event(AgentEvent::ConsoleOutput {
        gate: app.console.as_ref().expect("open").gate,
        chunk: "package name: ".to_string(),
    });
    type_str(&mut app, "wizard");
    press(&mut app, KeyCode::Enter);

    let TranscriptItem::Tool(tool) = app
        .transcript
        .iter()
        .find(|item| matches!(item, TranscriptItem::Tool(_)))
        .expect("the command's card")
    else {
        unreachable!()
    };
    assert_eq!(tool.progress, "package name: ❯ wizard\n");
}

/// Esc gives the composer back without killing anything, and the writer drops
/// — which is what tells the command it is unattended again.
#[test]
fn esc_detaches_the_console_and_enter_talks_to_the_agent_again() {
    let mut app = app();
    let mut host = open_console(&mut app, "npm init");
    press(&mut app, KeyCode::Esc);
    assert!(app.console.is_none(), "detached");
    assert_eq!(
        host.receive.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected),
        "dropping the writer is how the command learns nobody is there"
    );

    type_str(&mut app, "what is it asking?");
    let action = press(&mut app, KeyCode::Enter);
    assert!(
        matches!(action, Some(AppAction::Submit(_))),
        "Enter is a message to the agent again, got {action:?}"
    );
}

/// Ctrl-D is end-of-input for the child, exactly as in a terminal — not a
/// request to quit Wizard out from under a half-answered prompt.
#[test]
fn ctrl_d_ends_the_commands_input_instead_of_quitting() {
    let mut app = app();
    let mut host = open_console(&mut app, "cat");
    press_ctrl(&mut app, 'd');
    assert!(
        !app.should_quit,
        "Wizard stays up while a command is running"
    );
    assert_eq!(host.receive.try_recv(), Ok(crate::agent::ConsoleInput::Eof));
}

/// With no console open Ctrl-D still quits: the console borrows the key, it
/// does not take it.
#[test]
fn ctrl_d_still_quits_with_no_console_open() {
    let mut app = app();
    press_ctrl(&mut app, 'd');
    assert!(app.should_quit);
}

/// Ctrl-C interrupts the turn, which is what reaches the command's process
/// group from inside the parked `execute`.
#[test]
fn ctrl_c_during_a_console_interrupts_the_turn() {
    let mut app = app();
    let _host = open_console(&mut app, "npm init");
    app.status.busy = true;
    let action = press_ctrl(&mut app, 'c');
    assert!(matches!(action, Some(AppAction::Interrupt)), "{action:?}");
}

/// The console closes when its own command ends, and only then. Two `execute`
/// calls in one turn are sequential today, but a close for the wrong one must
/// not take the composer away from the one the user is answering.
#[test]
fn only_this_commands_close_gives_the_composer_back() {
    let mut app = app();
    let _host = open_console(&mut app, "npm init");
    let mine = app.console.as_ref().expect("open").gate;

    let (other, _other_host) = crate::agent::ConsoleGate::open();
    app.handle_agent_event(AgentEvent::ConsoleClosed { gate: other });
    assert!(app.console.is_some(), "somebody else's close is not ours");

    app.handle_agent_event(AgentEvent::ConsoleClosed { gate: mine });
    assert!(app.console.is_none(), "our command finished");
}

/// A console whose gate somebody else already claimed is not ours to drive —
/// the same rule as a plan review, and the reason a teed stream cannot produce
/// two authors of a child's input.
#[test]
fn an_already_claimed_console_is_not_taken_over() {
    let mut app = app();
    let (gate, _host) = crate::agent::ConsoleGate::open();
    let _first = gate.claim().expect("first claim wins");
    app.handle_agent_event(AgentEvent::ConsoleOpened {
        command: "npm init".to_string(),
        gate,
    });
    assert!(app.console.is_none());
}

/// A console cannot outlive its turn. The tool closes its own on every path it
/// controls, but a turn that died on a hard error never got there — and a
/// composer still pointing at a dead child is a composer whose Enter key does
/// nothing, which is the bug rather than the fix.
#[test]
fn a_turn_ending_takes_back_a_console_the_tool_never_closed() {
    let mut app = app();
    let _host = open_console(&mut app, "npm init");
    app.handle_agent_event(AgentEvent::Done {
        reason: DoneReason::Stopped,
    });
    assert!(app.console.is_none());

    type_str(&mut app, "what happened?");
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(action, Some(AppAction::Submit(_))), "{action:?}");
}

/// Every keystroke a surface names in its own text is one it binds.
///
/// This is the Ctrl-D bug's permanent fix. The console banner's button read
/// "end input (Ctrl-D)" while nothing anywhere bound Ctrl-D: `ConsoleEof` had
/// exactly two mentions, the button and the arm that would have handled it.
/// Somebody answering a prompt pressed the documented key, watched nothing
/// happen, and reasonably concluded the console was broken rather than that
/// the label was.
///
/// Both surfaces, because the class is not the window's: the TUI's `/help`
/// lists ten keys and its binding table is far larger, so a promise there is
/// easier to make and just as broken. The two spell a binding differently —
/// iced's `Key::Character("x")` and crossterm's `KeyCode::Char('x')` — and
/// either counts, since what is being asserted is that *something* handles it.
///
/// Nothing structural connects a label to a binding, so a scan of the source
/// is the instrument.
#[test]
fn every_key_a_surface_advertises_is_bound() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    for surface in ["native", "app"] {
        let mut files = Vec::new();
        sources(&root.join(surface), &mut files);
        if surface == "app" {
            // The TUI draws in `ui/` and binds in `app/`; both are its text.
            sources(&root.join("ui"), &mut files);
        }

        let mut advertised: Vec<char> = Vec::new();
        let mut bound = String::new();
        for path in &files {
            // A test file names keys only to talk about them.
            if path.file_name().is_some_and(|n| n == "tests.rs") {
                continue;
            }
            let source = std::fs::read_to_string(path).expect("a source file");
            bound.push_str(&source);
            for line in source.lines() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let mut rest = line;
                while let Some(at) = rest.find("Ctrl-") {
                    rest = &rest[at + "Ctrl-".len()..];
                    // `Ctrl-W/U/K` advertises three keys, not one. Consume the
                    // slash-separated run, or the compound spelling hides two
                    // of every three promises from this test.
                    let mut chars = rest.chars().peekable();
                    loop {
                        match chars.next() {
                            Some(key) if key.is_ascii_alphabetic() => {
                                advertised.push(key.to_ascii_lowercase());
                            }
                            _ => break,
                        }
                        if chars.peek() == Some(&'/') {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        advertised.sort_unstable();
        advertised.dedup();
        assert!(
            !advertised.is_empty(),
            "found no advertised keys in {surface}, so this test is reading nothing"
        );

        let unbound: Vec<char> = advertised
            .iter()
            .copied()
            .filter(|key| {
                !bound.contains(&format!("Character(\"{key}\")"))
                    && !bound.contains(&format!("Char('{key}')"))
            })
            .collect();
        assert!(
            unbound.is_empty(),
            "{surface} tells the user about these keys and binds none of them: \
             {unbound:?} (advertised: {advertised:?})"
        );
    }
}

/// The readline chords act on the line the caret is on, not the whole draft.
///
/// The composer is a real multi-line editor (Alt+Enter inserts a hard break),
/// and these chords used to operate on the entire buffer. On a three-line
/// draft Ctrl-K deleted the two lines *below* the caret and Ctrl-U deleted
/// every line above it — silently, and with nothing to restore them: vim's undo
/// does not cover readline chords, and vim is off by default. `/help` calls
/// these "kill word / to start / to end", and the Ctrl-U arm's own comment
/// always said "from the line start".
#[test]
fn the_kill_chords_only_touch_the_caret_s_own_line() {
    // Ctrl-K: to the end of this line, leaving the lines below alone.
    let mut kill = app();
    kill.input = "one\ntwo\nthree".to_string();
    kill.cursor = 3; // end of "one"
    press_ctrl(&mut kill, 'k');
    assert_eq!(
        kill.input, "one\ntwo\nthree",
        "nothing to kill at a line end"
    );

    kill.cursor = 1; // after "o"
    press_ctrl(&mut kill, 'k');
    assert_eq!(kill.input, "o\ntwo\nthree", "only line one is shortened");

    // Ctrl-U: back to the start of this line, leaving the lines above alone.
    let mut back = app();
    back.input = "one\ntwo\nthree".to_string();
    back.cursor = "one\ntwo".chars().count(); // end of "two"
    press_ctrl(&mut back, 'u');
    assert_eq!(back.input, "one\n\nthree", "line one survives");
    assert_eq!(
        back.cursor,
        "one\n".chars().count(),
        "caret at the line start"
    );

    // Ctrl-A / Ctrl-E are the same question without the deletion.
    let mut nav = app();
    nav.input = "one\ntwo\nthree".to_string();
    nav.cursor = "one\ntw".chars().count();
    press_ctrl(&mut nav, 'a');
    assert_eq!(
        nav.cursor,
        "one\n".chars().count(),
        "Ctrl-A: start of line two"
    );
    press_ctrl(&mut nav, 'e');
    assert_eq!(
        nav.cursor,
        "one\ntwo".chars().count(),
        "Ctrl-E: end of line two"
    );

    // Home/End likewise.
    nav.cursor = "one\ntw".chars().count();
    press(&mut nav, KeyCode::Home);
    assert_eq!(nav.cursor, "one\n".chars().count());
    press(&mut nav, KeyCode::End);
    assert_eq!(nav.cursor, "one\ntwo".chars().count());

    // And on a single line every one of them is what it always was.
    let mut one = app();
    one.input = "hello".to_string();
    one.cursor = 5;
    press_ctrl(&mut one, 'u');
    assert_eq!(one.input, "");
}

/// A notice the user's own keypress produced is visible, not hidden behind
/// the welcome screen.
///
/// `has_conversation` filters notices out on purpose, so a startup message —
/// a hook that appended context, an MCP server that did not connect — does not
/// replace the splash on a session nobody has used. That is right for those
/// and wrong for a reply: on a fresh session Ctrl-V printed "no image on the
/// clipboard to attach" where nobody could see it, and the first Ctrl-C's
/// "press Ctrl-C again to exit" was invisible too — so a fresh session gave no
/// warning at all before the second press quit it.
#[test]
fn a_notice_the_user_asked_for_is_not_hidden_by_the_welcome_screen() {
    // Before any key: a startup notice leaves the splash up.
    let mut boot = app();
    boot.notice("mcp: playwright did not connect");
    assert!(
        boot.welcome_visible(),
        "a startup notice must not replace the welcome screen"
    );

    // After a key: the same call has to be visible.
    let mut used = app();
    press(&mut used, KeyCode::Left); // a no-op on an empty composer
    used.notice("no image on the clipboard to attach");
    assert!(
        !used.welcome_visible(),
        "a notice raised after a keypress is a reply and must be on screen"
    );
}

/// Four vim divergences found by driving the composer, all of them things vim
/// does differently from what this did.
#[test]
fn vim_normal_mode_matches_vim_on_the_cases_that_diverged() {
    fn vim_app(text: &str, cursor: usize) -> App {
        let mut app = app();
        app.vim.enabled = true;
        app.vim.mode = crate::vim::VimMode::Normal;
        app.input = text.to_string();
        app.cursor = cursor;
        app
    }

    // `cw` is `ce`: it stops at the end of the word and leaves the space, so
    // you do not have to retype it. `dw` takes the space and was already
    // right. Both were handed the identical `w` range.
    let mut change = vim_app("foo bar baz", 0);
    press(&mut change, KeyCode::Char('c'));
    press(&mut change, KeyCode::Char('w'));
    assert_eq!(change.input, " bar baz", "cw must leave the space");

    let mut delete = vim_app("foo bar baz", 0);
    press(&mut delete, KeyCode::Char('d'));
    press(&mut delete, KeyCode::Char('w'));
    assert_eq!(delete.input, "bar baz", "dw still takes it");

    // `2d3w` is six words, not twenty-three. The two counts shared one field
    // and their digits concatenated.
    let mut counted = vim_app("one two three four five six seven", 0);
    for key in ['2', 'd', '3', 'w'] {
        press(&mut counted, KeyCode::Char(key));
    }
    assert_eq!(counted.input, "seven", "2d3w deletes 2 x 3 words");

    // A caret one past the end is not a place vim leaves you after a recall,
    // and the first `x` there deleted nothing.
    let mut recalled = vim_app("", 0);
    recalled.set_input("/status".to_string());
    assert_eq!(
        recalled.cursor,
        "/status".chars().count() - 1,
        "the block sits on the last character, not past it"
    );
    press(&mut recalled, KeyCode::Char('x'));
    assert_eq!(recalled.input, "/statu", "the first x deletes");
}

/// Escape closes the command popup in Normal mode too.
///
/// It was unreachable from there: Escape did not close it, Tab fell through to
/// the catch-all, and Up/Down are bound to history in Normal mode — so the only
/// way out was to edit the text, while the status bar advertised all three keys.
#[test]
fn the_command_popup_can_be_dismissed_from_vim_normal_mode() {
    let mut app = app();
    app.vim.enabled = true;
    type_str(&mut app, "/st");
    assert!(!app.suggestions.is_empty(), "the popup is open");

    app.vim.mode = crate::vim::VimMode::Normal;
    press(&mut app, KeyCode::Esc);
    assert!(app.suggestions.is_empty(), "Escape closes it");
    assert_eq!(app.input, "/st", "and keeps the draft");
}

/// The line motions mean the caret's line, on a draft that has several.
///
/// Alt+Enter puts hard newlines in the composer, and these used to measure the
/// whole buffer: `0` on line two jumped to the start of line *one*, `$` on line
/// one jumped to the end of the *last* line, and `D` at the start of line one
/// deleted every line below it. `dd`/`S` still take the whole draft and `j`/`k`
/// still browse history — both deliberate, both stated in `vim.rs`'s header.
#[test]
fn the_vim_line_motions_respect_hard_newlines() {
    fn vim_app(text: &str, cursor: usize) -> App {
        let mut app = app();
        app.vim.enabled = true;
        app.vim.mode = crate::vim::VimMode::Normal;
        app.input = text.to_string();
        app.cursor = cursor;
        app
    }
    let line2 = "one\ntwo".chars().count() - 1; // on the 'o' of "two"

    let mut zero = vim_app("one\ntwo", line2);
    press(&mut zero, KeyCode::Char('0'));
    assert_eq!(
        zero.cursor, 4,
        "0 goes to the start of line two, not line one"
    );

    // Normal-mode `$` lands *on* the last character of the line, not past it.
    let mut dollar = vim_app("one\ntwo", 1);
    press(&mut dollar, KeyCode::Char('$'));
    assert_eq!(dollar.cursor, 2, "$ lands on the 'e' of line one");

    let mut kill = vim_app("one\ntwo", 1);
    press(&mut kill, KeyCode::Char('D'));
    assert_eq!(kill.input, "o\ntwo", "D takes the rest of line one only");

    // And on a single-line draft every one of them is unchanged.
    let mut flat = vim_app("hello", 2);
    press(&mut flat, KeyCode::Char('$'));
    assert_eq!(flat.cursor, 4, "on the 'o', as vim leaves it");
    press(&mut flat, KeyCode::Char('0'));
    assert_eq!(flat.cursor, 0);
}

// ---------------------------------------------------------------------------
// UI skins
// ---------------------------------------------------------------------------

/// Render the theme fixture under `skin`, with that skin's companion palette,
/// as the whole screen's text.
///
/// Both are *pinned* rather than set, because the process-wide slots belong to
/// every other test thread: `App::new` writes them on every construction, so a
/// test that installed a skin globally would restyle whatever another thread
/// was rendering at the time.
fn skinned_screen(skin: crate::skin::Skin, width: u16, height: u16) -> Vec<String> {
    let _skin = crate::skin::pin(skin);
    let theme = Arc::new(theme::load(skin.companion_theme()).expect("companion theme loads"));
    let _theme = theme::pin(theme);
    let app = themed_fixture();
    let buf = screen(&app, width, height);
    (0..buf.area.height).map(|y| row_text(&buf, y)).collect()
}

/// The same, on the home screen: no conversation, no panels.
fn skinned_welcome(skin: crate::skin::Skin, width: u16, height: u16) -> Vec<String> {
    let _skin = crate::skin::pin(skin);
    let theme = Arc::new(theme::load(skin.companion_theme()).expect("companion theme loads"));
    let _theme = theme::pin(theme);
    let mut app = themed_fixture();
    app.welcome_dismissed = false;
    app.transcript.clear();
    app.diff = None;
    app.show_todos = false;
    let buf = screen(&app, width, height);
    (0..buf.area.height).map(|y| row_text(&buf, y)).collect()
}

#[test]
fn each_skin_marks_the_transcript_with_its_own_glyphs() {
    // The claim the whole feature rests on: one renderer, one fixture, four
    // recognizably different screens. Each row here is a glyph you could point
    // at in a screenshot of the product being borrowed from.
    for (skin, needles) in [
        (
            crate::skin::Skin::Wizard,
            ["· weighing options", "✓ inspect"],
        ),
        (
            crate::skin::Skin::Codex,
            ["• weighing options", "└ output line"],
        ),
        (
            // Grok Build's own header grammar: a `◆` bullet and the tool's own
            // name. "Ran" is Codex's past tense, not xAI's.
            crate::skin::Skin::Grok,
            ["┃  weighing options", "┃  ◆ inspect"],
        ),
    ] {
        let rows = skinned_screen(skin, 92, 60);
        for needle in needles {
            assert!(
                rows.iter().any(|row| row.contains(needle)),
                "{} should draw '{needle}':\n{}",
                skin.key(),
                rows.join("\n")
            );
        }
    }
}

#[test]
fn the_user_prompt_takes_the_skins_own_marker() {
    for (skin, marker) in [
        (crate::skin::Skin::Wizard, "❯ show me the theme"),
        (crate::skin::Skin::Codex, "› show me the theme"),
        // A Grok Build user prompt has no rail of its own — the accent column
        // is reserved and cleared — so what marks it is the `❯` hanging in the
        // block's content column, three columns in from the screen edge.
        (crate::skin::Skin::Grok, "   ❯ show me the theme"),
    ] {
        let rows = skinned_screen(skin, 92, 60);
        assert!(
            rows.iter().any(|row| row.contains(marker)),
            "{} should echo the prompt as '{marker}':\n{}",
            skin.key(),
            rows.join("\n")
        );
    }
}

#[test]
fn a_borrowed_cell_carries_no_row_its_upstream_does_not_have() {
    // Three places where Wizard's own state used to be bolted onto a cell that
    // was ported whole, and each one made the skin readable as a copy rather
    // than as the thing. The state itself is not lost — it moved to the
    // surfaces that already carried that class of fact.
    let codex = skinned_screen(crate::skin::Skin::Codex, 92, 60).join("\n");
    // `plans.rs:204` writes `• ` and `Updated Plan` and stops.
    assert!(
        codex.contains("• Updated Plan\n") || codex.contains("• Updated Plan "),
        "the codex plan cell should carry no count and no dismiss hint:\n{codex}"
    );
    assert!(
        !codex.contains("esc to hide"),
        "the codex plan cell should carry no dismiss hint:\n{codex}"
    );

    // `SessionHeaderHistoryCell::display_lines` (`session.rs:311-383`) has a
    // `model:` row and a `directory:` row; `permissions:` only appears when
    // permissions are wide open. Wizard's mode belongs in the footer's status
    // line, where it already is.
    let welcome = skinned_welcome(crate::skin::Skin::Codex, 92, 40).join("\n");
    assert!(
        welcome.contains("model:") && welcome.contains("directory:"),
        "the session card keeps the two rows Codex draws:\n{welcome}"
    );
    assert!(
        !welcome.contains("mode:"),
        "the session card should not grow a row of Wizard's:\n{welcome}"
    );

    // `TodoPane::render` (`todo_pane.rs:486-522`) is a bare `ListPane`. The
    // `▾ Group N` header belongs to the *tasks* pane, which is a different one.
    let grok = skinned_screen(crate::skin::Skin::Grok, 92, 60).join("\n");
    assert!(
        grok.contains("✓ done item"),
        "the grok todo pane still lists its items:\n{grok}"
    );
    assert!(
        !grok.contains("Todos"),
        "the grok todo pane should have no header row:\n{grok}"
    );
}

#[test]
fn the_codex_session_card_hugs_its_widest_row() {
    // `with_border_internal(lines, None)` — `session.rs:34-42`: the frame is
    // sized to the content, never to the width the cell was handed. Stretched
    // to `SESSION_HEADER_MAX_INNER_WIDTH` it is still a card, just visibly not
    // Codex's.
    let rows = skinned_welcome(crate::skin::Skin::Codex, 92, 40);
    let top = rows
        .iter()
        .find(|row| row.trim_start().starts_with('╭'))
        .expect("the session card is drawn");
    let card_width = top.trim_end().chars().count();
    assert!(
        card_width < 92 - 4,
        "the card should not fill the width it was given: {card_width} columns"
    );
    let widest_row = rows
        .iter()
        .filter(|row| row.trim_start().starts_with('│'))
        .map(|row| row.trim_end().chars().count())
        .max()
        .expect("the card has content rows");
    assert_eq!(
        card_width,
        widest_row,
        "every row of the card is the width of its widest:\n{}",
        rows.join("\n")
    );
}

#[test]
fn the_grok_shortcut_bar_names_keys_the_way_grok_build_does() {
    // `KeyShortcut::display()` — `P/src/input/key.rs:80-140`. Modifiers are
    // `Ctrl+`/`Alt+`/`Shift+`, named keys are `Enter`/`Esc`/`Tab`/`PgUp`/`PgDn`,
    // and a plain character stays lowercase. Wizard writes keys lowercase
    // everywhere else, which is exactly why this one has to be asserted.
    let rows = skinned_welcome(crate::skin::Skin::Grok, 92, 40).join("\n");
    for key in ["Enter:send", "Shift+Enter:newline", "Ctrl+t:expand"] {
        assert!(
            rows.contains(key),
            "the grok shortcut bar should write '{key}':\n{rows}"
        );
    }
    for lowercased in ["enter:send", "shift+enter", "ctrl+t"] {
        assert!(
            !rows.contains(lowercased),
            "'{lowercased}' is Wizard's casing, not Grok Build's:\n{rows}"
        );
    }
}

#[test]
fn the_composer_is_framed_the_way_the_skin_asks() {
    // Rules, a box, or nothing — and the draft has to be inside whichever it
    // is. A boxed composer whose text started a column left of its border was
    // the first thing this feature got wrong.
    let rules = skinned_screen(crate::skin::Skin::Wizard, 60, 24);
    assert!(
        rules.iter().any(|row| row.starts_with("───")),
        "wizard rules the composer:\n{}",
        rules.join("\n")
    );
    assert!(rules.iter().any(|row| row.starts_with(" ❯ ")));

    let boxed = skinned_screen(crate::skin::Skin::Grok, 60, 24);
    assert!(
        boxed.iter().any(|row| row.starts_with("  │ ❯ ")),
        "grok boxes the composer, with the prompt inside it:\n{}",
        boxed.join("\n")
    );

    let bare = skinned_screen(crate::skin::Skin::Codex, 60, 24);
    assert!(
        bare.iter().any(|row| row.starts_with("› ")),
        "codex hangs the prompt in the margin, with no frame at all:\n{}",
        bare.join("\n")
    );
    assert!(
        !bare.iter().any(|row| row.starts_with("───")),
        "and draws no rule:\n{}",
        bare.join("\n")
    );
}

#[test]
fn a_boxed_composer_wraps_inside_its_border() {
    // `composer_budget` and `draw_input` have to agree about what the frame
    // costs. When they did not, a full row of text under the boxed skins wrote
    // one column past the border and ratatui clipped it.
    let _skin = crate::skin::pin(crate::skin::Skin::Grok);
    let mut app = app();
    app.welcome_dismissed = true;
    app.input = "x".repeat(200);
    app.cursor = app.input.chars().count();
    let buf = screen(&app, 40, 20);
    for y in 0..buf.area.height {
        let row = row_text(&buf, y);
        // Grok Build's box sits inside the screen's own margin, so the border
        // is not in column zero — trim before looking for it, or this walks
        // every row and checks nothing.
        if row.trim_start().starts_with('│') {
            assert!(
                row.trim_end().ends_with('│'),
                "the composer's right border survived the draft: {row}"
            );
        }
    }
}

#[test]
fn every_welcome_screen_says_who_it_is_and_nothing_it_is_not() {
    for skin in crate::skin::Skin::ALL {
        let rows = skinned_welcome(skin, 92, 30);
        let screen = rows.join("\n");
        assert!(
            screen.contains("Wizard") || screen.contains("w i z a r d"),
            "{} must say which agent this is:\n{screen}",
            skin.key()
        );
        // A borrowed home screen carries no extra row of ours — the credit for
        // the chrome lives in `docs/ui-skins.md` and `NOTICE`, where the rest
        // of the attribution already is, and the screen itself is upstream's
        // shape with Wizard's name and Wizard's commands on it.
        assert!(
            !screen.contains("chrome after"),
            "{} must not add a credit row to a borrowed screen:\n{screen}",
            skin.key()
        );
    }
}

#[test]
fn every_skin_survives_a_terminal_too_small_to_draw_in() {
    // Four skins × the sizes that have historically broken the renderer: one
    // column, one row, and the two-row window where the composer has no room
    // for its own frame.
    for skin in crate::skin::Skin::ALL {
        for (width, height) in [(1, 1), (4, 3), (20, 2), (20, 5), (200, 8)] {
            let rows = skinned_screen(skin, width, height);
            assert_eq!(
                rows.len(),
                height as usize,
                "{} at {width}x{height}",
                skin.key()
            );
        }
    }
}

#[test]
fn the_status_line_narrates_a_turn_in_every_skins_words() {
    for (skin, needle) in [
        (crate::skin::Skin::Wizard, "step 3"),
        (crate::skin::Skin::Codex, "Working"),
        (crate::skin::Skin::Grok, "Thinking…"),
    ] {
        let _skin = crate::skin::pin(skin);
        let mut app = app();
        app.welcome_dismissed = true;
        app.status.busy = true;
        app.status.step = 3;
        let buf = screen(&app, 100, 12);
        let screen: Vec<String> = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        let screen = screen.join("\n");
        assert!(
            screen.contains(needle),
            "{} should narrate a busy turn with '{needle}':\n{screen}",
            skin.key()
        );
        // Whatever the wording, the step count is Wizard's own state and stays
        // on screen: a skin restyles the UI, it does not withhold from it.
        assert!(
            screen.contains("step 3"),
            "{} dropped the step counter:\n{screen}",
            skin.key()
        );
    }
}

#[test]
fn ui_command_lists_the_skins_and_marks_the_active_one() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let mut app = app();
    let listing = command::ui_command(&mut app, None);
    assert!(listing.contains("● wizard"), "{listing}");
    for other in ["codex", "grok"] {
        assert!(listing.contains(&format!("· {other}")), "{listing}");
    }
}

#[test]
fn ui_command_switches_brings_its_palette_and_persists_the_choice() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let _theme = theme::pin(theme::minimal());
    let mut app = app();
    let notice = command::ui_command(&mut app, Some("grok build"));
    assert_eq!(crate::skin::active(), crate::skin::Skin::Grok);
    assert!(notice.contains("grok build"), "{notice}");
    // The skin brings its palette with it.
    assert_eq!(theme::active().name, "grok");
    assert_eq!(app.config.ui.skin.as_deref(), Some("grok"));

    // And switching again brings the next one's, unconditionally — there is
    // no longer a user-set palette that could be left behind.
    command::ui_command(&mut app, Some("codex"));
    assert_eq!(crate::skin::active(), crate::skin::Skin::Codex);
    assert_eq!(theme::active().name, "codex");
}

#[test]
fn an_unknown_ui_name_is_an_error_and_changes_nothing() {
    let _skin = crate::skin::pin(crate::skin::Skin::Grok);
    let mut app = app();
    let notice = command::ui_command(&mut app, Some("emacs"));
    assert!(notice.starts_with("error:"), "{notice}");
    assert!(notice.contains("wizard, codex, grok"), "{notice}");
    assert_eq!(crate::skin::active(), crate::skin::Skin::Grok);
}

#[test]
fn the_settings_menu_cycles_the_interface_in_place() {
    // The menu stays open and the row updates, so cycling is its own preview.
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let mut app = app();
    app.open_settings_picker();
    let row = app
        .picker
        .as_ref()
        .expect("settings menu")
        .items
        .iter()
        .position(|item| item.value == "Interface")
        .expect("the menu offers the interface");

    for expected in [
        crate::skin::Skin::Codex,
        crate::skin::Skin::Grok,
        crate::skin::Skin::Wizard,
    ] {
        app.picker.as_mut().unwrap().selected = row;
        press(&mut app, KeyCode::Enter);
        assert_eq!(crate::skin::active(), expected);
        let picker = app.picker.as_ref().expect("the menu stays open");
        assert_eq!(picker.selected, row, "and the cursor stays on the row");
        assert_eq!(picker.items[row].detail, expected.label());
    }
}

/// A turn that is killed rather than allowed to finish leaves four separate
/// pieces of the surface lying, and every one of them reads to the user as
/// Wizard having stopped working: a status bar spinning over a turn that
/// ended, a rail full of subagents that will never report, a composer typing
/// into a dead command's stdin, and a step counter frozen mid-turn.
#[test]
fn a_killed_turn_hands_the_whole_surface_back() {
    let mut app = app_with_panes(2);
    let _host = open_console(&mut app, "npm init");
    app.status.busy = true;
    app.status.step = 4;
    app.turn_started = Some(Instant::now());

    app.end_turn_abruptly("interrupted");

    assert!(!app.status.busy, "the spinner has to stop");
    assert_eq!(app.status.step, 0);
    assert!(app.turn_started.is_none(), "and so does the elapsed clock");
    assert_eq!(
        app.running_panes(),
        0,
        "no subagent is left pulsing on the rail"
    );
    assert!(
        app.console.is_none(),
        "Enter goes back to the agent instead of a command that is gone"
    );

    // Typing works again, and Enter starts a turn rather than answering a
    // command or queueing behind one.
    type_str(&mut app, "again please");
    assert!(
        matches!(press(&mut app, KeyCode::Enter), Some(AppAction::Submit(_))),
        "the composer is usable and the next message runs"
    );
}

/// Queued prompts are the user's, and whether they survive depends on why the
/// turn ended — so the teardown does not decide it.
#[test]
fn the_message_queue_outlives_the_teardown_that_did_not_ask_about_it() {
    let mut app = app();
    app.status.busy = true;
    type_str(&mut app, "one");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.message_queue.len(), 1);

    app.end_turn_abruptly("the turn crashed");

    assert_eq!(
        app.message_queue.len(),
        1,
        "a crash is not a reason to throw away what the user typed"
    );
}

#[test]
fn input_ending_quits_instead_of_repainting_a_session_nobody_can_type_into() {
    let mut app = app();
    assert!(!app.should_quit);

    let action = app
        .handle_event(crate::event::Event::InputClosed(
            "terminal input ended (stdin closed)".to_string(),
        ))
        .expect("the end of input is not itself an error");

    assert!(action.is_none());
    assert!(
        app.should_quit,
        "the tick task keeps painting, so a reader that gave up has to end the run"
    );
    let notice = format!("{:?}", app.transcript.last());
    assert!(
        notice.contains("stdin closed"),
        "the reason belongs in the transcript that survives on disk: {notice}"
    );
}

/// `--omakase` reaches the agent on the TUI surface too.
///
/// `App::new` lights the OMAKASE badge from `config.omakase` alone, while
/// `run_tui` used to hand the agent nothing but `set_plan_mode` from
/// `config.plan_first`. So `wizard --omakase` in a terminal advertised
/// chef's choice and ran plain plan mode: no omakase system prompt,
/// `interview` still asking, `exit_plan` still opening the review modal.
///
/// Grep, in the manner of the headless copy of this test: the defect is the
/// *absence* of a call, which nothing observable at runtime can distinguish
/// from a session that simply was not asked for omakase.
#[test]
fn the_tui_runtime_applies_omakase_and_not_only_plan_mode() {
    let source = include_str!("runtime.rs");
    assert!(
        source.contains("config.plan_first"),
        "plan_first is still read here"
    );
    assert!(
        source.contains("agent.set_omakase("),
        "--omakase must reach the agent on the TUI surface too, not just --plan"
    );
}

/// The badge and the agent agree about omakase, in both directions.
///
/// The status line reads `App::omakase`, the agent reads `Agent::omakase`,
/// and they are set from the same `Config`. `omakase = true` in config.toml
/// with no `--plan` and no `plan_first` used to light the badge and leave the
/// agent in neither plan mode nor omakase.
#[test]
fn omakase_in_config_alone_lights_the_badge_and_the_mode() {
    use clap::Parser as _;

    let mut config = crate::config::Config {
        omakase: true,
        ..crate::config::Config::default()
    };
    config.apply_cli(&crate::cli::Cli::try_parse_from(["wizard"]).expect("valid args"));
    assert!(
        config.plan_first,
        "omakase is a flavor of plan mode wherever it was asked for"
    );
    let app = App::new(config);
    assert!(app.omakase, "the badge is lit");
    assert!(app.plan_mode, "and plan mode with it");
}
