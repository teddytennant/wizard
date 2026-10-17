//! The welcome screen's banner, against `reference/grok/*/02w-home-ssh-warning`.

mod common;

use common::cells::{assert_cells, Skip};
use common::*;
use grokw::ui::welcome::StartupWarning;

const HERE: (&str, &str) = ("Grok 4.7 is here!", "Select 'Grok 4.7' under /model.");
const LEARN: (&str, &str) = (
    "New /learn skill!",
    "Ask Grok to /learn from your past traces and tune Grok Build to how you work.",
);

fn warned(w: u16, h: u16, ann: (&str, &str), tip: &str) -> Harness {
    let mut hn = Harness::home(w, h);
    hn.app.home.announcement = Some((ann.0.into(), ann.1.into()));
    hn.app.home.tip = tip.into();
    hn.app.home.warnings = vec![StartupWarning::clipboard()];
    hn
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn clipboard_warning_pushes_the_card_down_three_rows() {
    let cases = [
        (
            120u16,
            36u16,
            HERE,
            "Press Ctrl+B to background a running terminal command.",
        ),
        (
            150,
            42,
            HERE,
            "Use @! for hidden or ignored files: @!.github/workflows.",
        ),
        (
            80,
            24,
            LEARN,
            "Use Shift+Tab to cycle between modes like Plan mode.",
        ),
    ];
    for (w, h, ann, tip) in cases {
        let mut hn = warned(w, h, ann, tip);
        hn.app.config.effort = "high".into();
        hn.dump(&format!("home-warning-{w}x{h}"));
        assert_snapshot(&format!("home-warning-{w}x{h}"), &hn.text());
        assert_cells(
            &format!("{w}x{h}/02w-home-ssh-warning"),
            &hn.render(),
            &Skip {
                fg_of: ('\u{2800}'..='\u{28ff}').collect::<String>() + "┃",
                ..Default::default()
            },
        );
    }
}

#[test]
fn an_info_entry_loses_the_slot_to_a_warning() {
    use grokw::ui::welcome::{banner_warning, Severity};
    let info = StartupWarning {
        severity: Severity::Info,
        message: "note".into(),
        action: None,
    };
    let list = [info.clone(), StartupWarning::clipboard()];
    assert_eq!(banner_warning(&list), Some(&list[1]));
    assert_eq!(banner_warning(std::slice::from_ref(&info)), Some(&info));
    assert_eq!(banner_warning(&[]), None);
}

fn temp_repo(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("grokw-wt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    dir
}

#[test]
fn ctrl_w_outside_a_repository_says_so_in_the_banner() {
    let dir = std::env::temp_dir().join(format!("grokw-no-repo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = Harness::home(120, 36);
    h.app.opts.cwd = dir.clone();
    h.ctrl('w');
    assert!(h.app.home.worktree.is_none());
    let t = h.text();
    assert!(
        t.contains("Not inside a git repository. Navigate to a git repo or run 'git init' first."),
        "{t}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_dialog_asks_for_a_name_and_creates_the_checkout() {
    let repo = temp_repo("dialog");
    let mut h = Harness::home(120, 36);
    h.app.opts.cwd = repo.clone();
    let state = std::env::temp_dir().join(format!("grokw-wt-state-{}", std::process::id()));
    h.app.opts.state_dir = Some(state.clone());
    h.ctrl('w');
    assert!(h.app.home.worktree.is_some());
    let t = h.render();
    // 50 wide, 5 tall, centred: columns 35..=84, rows 15..=19
    assert_eq!(t.cell(35, 15).unwrap().symbol(), "╭");
    assert_eq!(t.cell(84, 19).unwrap().symbol(), "╯");
    assert!(t.row(16).contains("New Worktree"), "{}", t.plain());
    assert!(t.row(17).contains("Name (optional): "));
    assert!(t.row(18).contains("enter = create   esc = cancel"));
    h.dump("worktree-dialog-120x36");
    assert_snapshot("worktree-dialog-120x36", &t.plain());
    h.type_str("try one");
    assert!(h.render().row(17).contains("Name (optional): try one"));
    h.press(crossterm::event::KeyCode::Enter);
    // the session moved into the worktree
    let wt = state
        .join("worktrees")
        .join(repo.file_name().unwrap())
        .join("try-one");
    assert!(wt.join("README.md").exists(), "{}", wt.display());
    assert_eq!(h.app.opts.cwd, wt);
    assert_eq!(h.app.screen, grokw::app::Screen::Session);
    assert!(h
        .app
        .toast
        .as_ref()
        .unwrap()
        .text
        .starts_with("✓ Worktree: "));
    // a second one with the same name is refused, and the banner says why
    let mut h2 = Harness::home(120, 36);
    h2.app.opts.cwd = repo.clone();
    h2.app.opts.state_dir = Some(state.clone());
    h2.ctrl('w');
    h2.type_str("try one");
    h2.press(crossterm::event::KeyCode::Enter);
    let t = h2.text();
    assert!(t.contains("Cannot create worktree: "), "{t}");
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn esc_closes_the_dialog_and_a_long_name_grows_it() {
    use grokw::ui::worktree::dialog_width;
    assert_eq!(dialog_width(120, ""), 50);
    assert_eq!(dialog_width(40, ""), 36);
    assert!(dialog_width(120, "a-very-long-worktree-name-that-exceeds-fifty") > 50);
    let repo = temp_repo("esc");
    let mut h = Harness::home(120, 36);
    h.app.opts.cwd = repo.clone();
    h.ctrl('w');
    h.press(crossterm::event::KeyCode::Esc);
    assert!(h.app.home.worktree.is_none());
    let _ = std::fs::remove_dir_all(&repo);
}

/// The welcome screen at every size `reference/grok/layouts` holds, cell for cell, the 40x12
/// one where the first menu row is painted over the header included. The announcement and tip
/// each capture showed are pinned, since they come from a server and rotate.
#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn every_captured_layout_matches_cell_for_cell() {
    const HERE: &str = "Grok 4.7 is here!";
    const SELECT: &str = "Select 'Grok 4.7' under /model.";
    const LEARN: &str = "New /learn skill!";
    const TUNE: &str =
        "Ask Grok to /learn from your past traces and tune Grok Build to how you work.";
    const AT: &str = "Use @ to attach files like @src/main.rs.";
    const COMPACT: &str = "Run /compact [context] when chat gets long.";
    const WORKTREE: &str = "Start Grok in a fresh worktree with `-w`; add `-r <session-id>` to resume an existing session there.";
    const DASH: &str =
        "Run /dashboard (or Ctrl+\\) to see and manage all your agents in one place.";
    const AUTO: &str = "Press Ctrl+O to toggle auto-approve mode.";
    type Case = (u16, u16, Option<(&'static str, &'static str)>, &'static str);
    let cases: [Case; 11] = [
        (100, 28, Some((LEARN, TUNE)), COMPACT),
        (120, 22, None, WORKTREE),
        (120, 24, Some((HERE, SELECT)), AT),
        (150, 30, Some((LEARN, TUNE)), AUTO),
        (200, 50, Some((HERE, SELECT)), COMPACT),
        (60, 20, Some((HERE, SELECT)), WORKTREE),
        (80, 26, Some((HERE, SELECT)), WORKTREE),
        (80, 30, Some((LEARN, TUNE)), DASH),
        (89, 30, Some((HERE, SELECT)), AT),
        (90, 30, Some((LEARN, TUNE)), AUTO),
        (40, 12, Some((HERE, SELECT)), DASH),
    ];
    for (w, h, ann, tip) in cases {
        let mut hn = Harness::home(w, h);
        hn.app.config.effort = "xhigh".into();
        hn.app.home.tip = tip.to_string();
        hn.app.home.announcement = Some(ann.map_or((String::new(), String::new()), |(a, b)| {
            (a.to_string(), b.to_string())
        }));
        assert_cells(
            &format!("layouts/home-{w}x{h}"),
            &hn.render(),
            &Skip {
                fg_of: ('\u{2800}'..='\u{28ff}').collect::<String>() + "┃",
                ..Default::default()
            },
        );
    }
}
