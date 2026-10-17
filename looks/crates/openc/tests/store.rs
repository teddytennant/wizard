//! History and draft files. Its own test binary: `store::use_dir` is a process-wide OnceLock and
//! the other test files set it first through the harness.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn mode(p: PathBuf) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn history_and_drafts_are_private_and_the_history_is_bounded() {
    let dir = std::env::temp_dir().join(format!("openc-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    openc::store::use_dir(dir.clone());
    // An older version's files and directory, group and world readable.
    std::fs::create_dir_all(dir.join("drafts")).unwrap();
    std::fs::write(dir.join("history.jsonl"), "").unwrap();
    std::fs::set_permissions(
        dir.join("history.jsonl"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    openc::store::append_history("/work", "password is hunter2");
    openc::store::save_draft("sess1", "token sk-abc");
    openc::store::note_command("review");
    assert_eq!(mode(dir.join("history.jsonl")), 0o600, "history.jsonl");
    assert_eq!(mode(dir.join("drafts").join("sess1")), 0o600, "draft");
    assert_eq!(mode(dir.join("commands.recent")), 0o600, "commands.recent");

    // A directory this version creates is 0700.
    let _ = std::fs::remove_dir_all(dir.join("drafts"));
    openc::store::save_draft("sess2", "x");
    assert_eq!(mode(dir.join("drafts")), 0o700, "drafts dir");

    // 3000 sends of 20 KB: the file used to grow to 60 MB.
    let text = "x".repeat(20_000);
    for i in 0..3000 {
        openc::store::append_history("/work", &format!("{i} {text}"));
    }
    let size = std::fs::metadata(dir.join("history.jsonl")).unwrap().len();
    assert!(
        size <= 17 << 20,
        "history.jsonl is {} MB and is never trimmed",
        size >> 20
    );
    let h = openc::store::load_history("/work");
    assert!(h.len() > 100, "kept {} entries", h.len());
    assert!(
        h.last().unwrap().starts_with("2999 "),
        "the newest entry survives"
    );
    assert_eq!(mode(dir.join("history.jsonl")), 0o600);
    assert!(!dir.join("history.jsonl.tmp").exists());
}
