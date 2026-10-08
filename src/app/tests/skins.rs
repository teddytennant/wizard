//! Tests for the UI skins, split out of `app/tests.rs`.

use super::*;

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
            ["◆ weighing options", "● inspect"],
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
    for key in ["Enter:send", "Shift+Enter:newline", "Shift+Tab:mode"] {
        assert!(
            rows.contains(key),
            "the grok shortcut bar should write '{key}':\n{rows}"
        );
    }
    for lowercased in ["enter:send", "shift+enter", "shift+tab"] {
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
            screen.contains("Wizard") || screen.contains(" wizard "),
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
    for other in ["codex", "grok", "opencode", "pi"] {
        assert!(listing.contains(&format!("· {other}")), "{listing}");
    }
    assert!(listing.contains("restarts"), "{listing}");
}

#[test]
fn ui_command_switches_brings_its_palette_and_persists_the_choice() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let _theme = theme::pin(theme::minimal());
    let mut app = app();
    // Grok has an in-process frame, and this test binary has no companion
    // beside it, so the switch stays in process.
    let notice = command::ui_command(&mut app, Some("grok build"));
    assert_eq!(crate::skin::active(), crate::skin::Skin::Grok);
    assert!(notice.contains("grok"), "{notice}");
    assert_eq!(theme::active().name, "grok");
    assert_eq!(app.config.ui.skin.as_deref(), Some("grok"));

    command::ui_command(&mut app, Some("codex"));
    assert_eq!(crate::skin::active(), crate::skin::Skin::Codex);
    assert_eq!(theme::active().name, "codex");
}

#[test]
fn a_full_look_with_no_in_process_frame_refuses_when_its_binary_is_missing() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let mut app = app();
    let notice = command::ui_command(&mut app, Some("pi"));
    assert!(notice.starts_with("error:"), "{notice}");
    assert!(notice.contains("wizard-ui-pi"), "{notice}");
    assert_eq!(crate::skin::active(), crate::skin::Skin::Wizard);
    assert_ne!(app.config.ui.skin.as_deref(), Some("pi"));
}

/// A look directory with the named `wizard-ui-*` binaries in it, searched by
/// this thread only.
fn installed_looks(names: &[&str]) -> (tempfile::TempDir, crate::skin::launch::SearchPinned) {
    let dir = tempfile::tempdir().expect("tempdir");
    for name in names {
        std::fs::write(dir.path().join(name), "").expect("write a look");
    }
    let pinned = crate::skin::launch::pin_search(dir.path());
    (dir, pinned)
}

#[test]
fn a_full_look_quits_the_tui_and_starts_once_the_terminal_is_back() {
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let _looks = installed_looks(&["wizard-ui-pi"]);
    let mut app = app();
    let notice = command::ui_command(&mut app, Some("pi"));
    assert_eq!(notice, "starting wizard-ui-pi");
    assert!(app.should_quit);
    assert_eq!(app.look, Some(crate::skin::Skin::Pi));
    assert_eq!(app.config.ui.skin.as_deref(), Some("pi"));
    assert_eq!(crate::skin::active(), crate::skin::Skin::Wizard);
}

#[test]
fn an_unknown_ui_name_is_an_error_and_changes_nothing() {
    let _skin = crate::skin::pin(crate::skin::Skin::Grok);
    let mut app = app();
    let notice = command::ui_command(&mut app, Some("emacs"));
    assert!(notice.starts_with("error:"), "{notice}");
    assert!(notice.contains("/ui lists them"), "{notice}");
    assert_eq!(crate::skin::active(), crate::skin::Skin::Grok);
}

#[test]
fn the_settings_menu_cycles_the_interface_in_place() {
    // The menu stays open and the row updates, so cycling is its own preview.
    let _skin = crate::skin::pin(crate::skin::Skin::Wizard);
    let _theme = theme::pin(theme::minimal());
    let _looks = installed_looks(&["wizard-ui-codex", "wizard-ui-grok"]);
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

    // The menu cycles looks this process can draw, even with their full UIs
    // installed. Leaving the process out of a menu is /ui's job.
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
        assert!(
            picker.items[row].detail.starts_with(expected.label()),
            "{}",
            picker.items[row].detail
        );
        assert!(!app.should_quit && app.look.is_none());
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
    let source = include_str!("../runtime.rs");
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

/// A plugin's slash command completes in the popup and submits as a command,
/// not as a prompt. The wiring under test is `update_suggestions` and `submit`
/// reading the merged palette rather than the built-in table alone.
#[test]
fn a_plugin_command_completes_and_submits_like_a_builtin() {
    struct Held(&'static str);
    impl Drop for Held {
        fn drop(&mut self) {
            crate::commands::plugin::uninstall(self.0);
        }
    }

    let _held = Held("zzappprobe");
    crate::commands::plugin::install(
        "app-test",
        crate::commands::PluginCommand::new(
            "zzappprobe",
            "a probe",
            std::sync::Arc::new(|_: String| async move { Ok(String::new()) }),
        )
        .args("[thing]"),
    )
    .expect("the name is free");

    let mut app = app();
    type_str(&mut app, "/zzappp");
    let names: Vec<&str> = app.suggestions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["zzappprobe"]);
    assert!(
        app.suggestions[0].takes_args,
        "an argument hint waits for arguments"
    );

    // Tab completes to the name plus a space, then Enter submits the line as a
    // command rather than sending it to the model.
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "/zzappprobe ");
    type_str(&mut app, "here");
    match press(&mut app, KeyCode::Enter) {
        Some(AppAction::Command(SlashCommand::Plugin { name, args })) => {
            assert_eq!(name, "zzappprobe");
            assert_eq!(args, "here");
        }
        other => panic!("expected the plugin command, got {other:?}"),
    }
}

/// ↓ then Enter on the empty welcome composer submits that starter prompt;
/// ↑ alone is still history; digits are typed, never claimed; and an inline
/// prompt (the `/provider add` key field) keeps every key.
#[test]
fn starter_prompts_are_picked_by_arrow_and_enter_only() {
    let mut app = super::App::new(Config::default());
    app.starter_prompts = vec!["Explain this".to_string(), "Review changes".to_string()];
    assert!(app.welcome_visible());

    // ↑ on a fresh session is history, not the list.
    assert!(press(&mut app, KeyCode::Up).is_none());
    assert_eq!(app.starter_index, None);
    assert!(press(&mut app, KeyCode::Down).is_none());
    assert_eq!(app.starter_index, Some(0));
    assert!(press(&mut app, KeyCode::Down).is_none());
    assert_eq!(app.starter_index, Some(1));
    assert!(press(&mut app, KeyCode::Up).is_none());
    assert_eq!(app.starter_index, Some(0));
    let action = press(&mut app, KeyCode::Enter);
    assert!(
        matches!(&action, Some(AppAction::Submit(prepared)) if prepared.text == "Explain this"),
        "{action:?}"
    );

    // A digit is a digit: "2 questions:" starts with one.
    let mut app = super::App::new(Config::default());
    app.starter_prompts = vec!["Explain this".to_string()];
    assert!(press(&mut app, KeyCode::Char('2')).is_none());
    assert_eq!(app.input, "2");
    // Enter on an empty composer with nothing selected submits nothing.
    let mut app = super::App::new(Config::default());
    app.starter_prompts = vec!["Explain this".to_string()];
    assert!(press(&mut app, KeyCode::Enter).is_none());

    // An inline key prompt owns ↓ and Enter.
    let mut app = super::App::new(Config::default());
    app.starter_prompts = vec!["Explain this".to_string()];
    app.web_key_backend = Some("brave".to_string());
    assert!(press(&mut app, KeyCode::Down).is_none());
    assert_eq!(app.starter_index, None);
}

/// The card's provider line carries the remedy, not the URL or the body.
#[test]
fn the_health_line_names_the_fix() {
    assert_eq!(
        super::health_line("not signed in to xAI; run `wizard --login xai` (or /login xai) first"),
        "not signed in to xAI: /login xai"
    );
    assert_eq!(
        super::health_line(
            "https://api.anthropic.com rejected the API key (HTTP 401), check the env var"
        ),
        "api.anthropic.com rejected the key (401): /provider to replace it"
    );
    assert_eq!(
        super::health_line("cannot reach https://api.x.ai/v1: connection refused"),
        "cannot reach api.x.ai"
    );
}

#[test]
fn a_drag_copy_leaves_the_transcript_gutter_behind_in_every_skin() {
    // Drag from the screen edge across a notice and a wrapped reply. Each skin
    // puts a different marker or rail in front of the text; none of it, and
    // none of the blank columns under it, belongs in the clipboard.
    for skin in [
        crate::skin::Skin::Wizard,
        crate::skin::Skin::Codex,
        crate::skin::Skin::Grok,
    ] {
        let _skin = crate::skin::pin(skin);
        let theme = Arc::new(theme::load(skin.companion_theme()).expect("companion theme loads"));
        let _theme = theme::pin(theme);
        let mut app = themed_fixture();
        app.transcript.assistant(
            "First paragraph of the reply that is long enough to wrap around onto a second \
             row of the screen for sure."
                .to_string(),
        );
        let backend = ratatui::backend::TestBackend::new(92, 60);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row = (0..buf.area.height)
            .find(|&y| row_text(&buf, y).contains("First paragraph"))
            .expect("the reply is on screen");
        let selection = Selection {
            anchor: (0, row),
            head: (40, row + 1),
            dragging: false,
        };
        let text = crate::ui::selection_text(&buf, &selection, &app.text_origins.borrow());
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].starts_with("First paragraph") && !lines[1].starts_with(' '),
            "{} copied a gutter:\n{text}",
            skin.key()
        );
    }
}

#[test]
fn plugins_parses_its_verbs() {
    use crate::commands::PluginsAction;
    use crate::pi_plugins::Target;
    let parse = |line: &str| SlashCommand::parse(line).unwrap();
    assert_eq!(
        parse("/plugins"),
        Ok(SlashCommand::Plugins(PluginsAction::Browse(String::new())))
    );
    assert_eq!(
        parse("/plugins sentry skills"),
        Ok(SlashCommand::Plugins(PluginsAction::Browse(
            "sentry skills".into()
        )))
    );
    assert_eq!(
        parse("/plugins search list"),
        Ok(SlashCommand::Plugins(PluginsAction::Browse("list".into())))
    );
    assert_eq!(
        parse("/plugins list"),
        Ok(SlashCommand::Plugins(PluginsAction::List))
    );
    assert_eq!(
        parse("/plugins install npm:pi-x"),
        Ok(SlashCommand::Plugins(PluginsAction::Install {
            spec: "npm:pi-x".into(),
            target: Target::Wizard,
        }))
    );
    assert_eq!(
        parse("/plugins install pi-x --for both"),
        Ok(SlashCommand::Plugins(PluginsAction::Install {
            spec: "pi-x".into(),
            target: Target::Both,
        }))
    );
    assert_eq!(
        parse("/plugins remove pi-x --for=pi"),
        Ok(SlashCommand::Plugins(PluginsAction::Remove {
            name: "pi-x".into(),
            target: Target::Pi,
        }))
    );
    assert!(parse("/plugins install").is_err());
    assert!(parse("/plugins install pi-x --for everyone").is_err());
    assert!(
        SlashCommand::Plugins(PluginsAction::List)
            .agent_runnable()
            .is_err(),
        "the agent does not install plugins for itself"
    );
}

#[test]
fn plugin_target_picker_asks_only_when_pi_is_here() {
    use super::picker::{plugin_target_picker, split_target};
    use crate::pi_plugins::Target;
    assert!(plugin_target_picker("npm:pi-x", false).is_none());
    let picker = plugin_target_picker("npm:pi-x", true).unwrap();
    assert_eq!(picker.kind, PickerKind::PiTarget);
    assert!(picker.title.contains("pi-x"));
    let targets: Vec<Target> = picker
        .items
        .iter()
        .map(|item| split_target(&item.value).unwrap().0)
        .collect();
    // Both first: the default when both harnesses are installed.
    assert_eq!(targets, vec![Target::Both, Target::Wizard, Target::Pi]);
    assert!(picker.items[0].detail.contains("beta"));
    assert_eq!(split_target("nobody npm:x"), None);
}

#[test]
fn plugin_pickers_emit_install_and_remove() {
    use super::picker::{plugin_remove_picker, plugin_target_picker};
    use crate::commands::PluginsAction;
    use crate::pi_plugins::Target;
    let mut app = app();
    app.picker = plugin_target_picker("npm:pi-x", true);
    press(&mut app, KeyCode::Down);
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Plugins(PluginsAction::Install { spec, target })))
            if spec == "npm:pi-x" && target == Target::Wizard
    ));

    app.picker = Some(plugin_remove_picker("pi-x", false));
    assert_eq!(app.picker.as_ref().unwrap().items.len(), 1);
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Plugins(PluginsAction::Remove { name, target })))
            if name == "pi-x" && target == Target::Wizard
    ));
    assert_eq!(plugin_remove_picker("pi-x", true).items.len(), 2);
}
