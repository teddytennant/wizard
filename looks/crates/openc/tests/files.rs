//! The `@` index at scale: a big listing arrives in chunks, keys never wait for a search, the
//! answer lands from the worker, and nothing is cut. Round 1 interaction findings 3, 4 and 16.

mod common;

use std::time::{Duration, Instant};

use common::Harness;
use openc::app::Msg;
use tuikit::term::Event as TermEvent;

fn chunk(h: &mut Harness, names: Vec<String>, first: bool) {
    let now = h.now();
    h.app.handle(
        TermEvent::Msg(Msg::Files {
            chunk: names,
            first,
        }),
        now,
    );
}

/// Feed the app what the worker thread sends until `done` is true or the time is up.
async fn pump(h: &mut Harness, done: impl Fn(&Harness) -> bool) {
    let t = Instant::now();
    while !done(h) && t.elapsed() < Duration::from_secs(30) {
        if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(50), h.app_rx.recv()).await
        {
            let now = h.now();
            h.app.handle(TermEvent::Msg(m), now);
        }
    }
}

fn popup_has(h: &Harness, path: &str) -> bool {
    h.app
        .composer
        .popup
        .as_ref()
        .is_some_and(|p| p.items.iter().any(|i| i.insert == path))
}

#[tokio::test]
async fn a_forty_thousand_file_index_reaches_the_last_directory_without_blocking_a_key() {
    let mut h = Harness::new(100, 30);
    h.ready();
    // The worker is what spawn_files starts; here the chunks come in by hand.
    h.app.file_search = Some(openc::files::FileSearch::spawn(50, {
        let tx = h.app.app_tx.clone();
        move |query, items| {
            let _ = tx.send(Msg::FileHits { query, items });
        }
    }));
    let all: Vec<String> = (0..40_000)
        .map(|i| format!("a/f{i:05}.txt"))
        .chain(["z/needle_unique.txt".to_string(), "z/".to_string()])
        .collect();
    for (n, c) in all.chunks(8192).enumerate() {
        chunk(&mut h, c.to_vec(), n == 0);
    }
    assert!(
        h.app.composer.remote_files,
        "40k entries are past the inline limit"
    );
    assert!(
        h.app.files.len() < all.len(),
        "the app does not hold a second copy"
    );

    let mut worst = Duration::ZERO;
    for c in "look @needle_uni".chars() {
        let t = Instant::now();
        h.key(crossterm::event::KeyCode::Char(c));
        worst = worst.max(t.elapsed());
    }
    assert!(
        worst < Duration::from_millis(25),
        "a key took {worst:?}: the search ran on the UI thread"
    );
    pump(&mut h, |h| popup_has(h, "z/needle_unique.txt")).await;
    assert!(
        popup_has(&h, "z/needle_unique.txt"),
        "the tail of the list is reachable"
    );
    // Tab completes the row that was searched for.
    h.key(crossterm::event::KeyCode::Tab);
    assert_eq!(h.app.composer.text(), "look @z/needle_unique.txt ");

    // The very last of the alphabetical a/ block, past the old 30,000 cut.
    h.app.composer.clear();
    for c in "@f39999".chars() {
        h.key(crossterm::event::KeyCode::Char(c));
    }
    pump(&mut h, |h| popup_has(h, "a/f39999.txt")).await;
    assert!(popup_has(&h, "a/f39999.txt"));
}

#[tokio::test]
async fn a_small_repository_is_answered_inline_with_no_round_trip() {
    let mut h = Harness::new(100, 30);
    h.ready();
    chunk(
        &mut h,
        vec!["src/lib.rs".into(), "src/main.rs".into()],
        true,
    );
    assert!(!h.app.composer.remote_files);
    h.type_str("@lib");
    assert!(
        popup_has(&h, "src/lib.rs"),
        "rows are there on the same key"
    );
}
