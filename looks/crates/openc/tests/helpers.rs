//! Clipboard helpers (finding 11): a helper that sits for seconds must not stop the interface.
//! Its own test binary, because it sets PATH and display variables for the whole process.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use openc::app::{App, Msg, Opts, ThemeChoice};
use openc::palette::{Depth, UNICODE};
use openc::term::Caps;
use tokio::sync::mpsc::unbounded_channel;

fn shim(dir: &std::path::Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_slow_clipboard_helper_does_not_hold_the_ui_thread() {
    let dir = std::env::temp_dir().join(format!("openc-helpers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // wl-copy takes a second and a half, wl-paste says there is no image after two seconds.
    shim(&dir, "wl-copy", "cat >/dev/null; sleep 1.5");
    shim(&dir, "wl-paste", "sleep 2; exit 1");
    // The shims come first on PATH, so no real wl-copy or wl-paste can answer.
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path}", dir.display()));
    std::env::set_var("WAYLAND_DISPLAY", "wayland-test");
    std::env::remove_var("DISPLAY");
    std::env::remove_var("TMUX");
    std::env::remove_var("SSH_CONNECTION");
    std::env::set_var("TERM", "xterm-256color");
    openc::store::use_dir(dir.join("state"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _enter = rt.enter();
    let (tx, _reqs) = unbounded_channel();
    let (app_tx, mut app_rx) = unbounded_channel();
    let opts = Opts {
        cwd: PathBuf::from("/work/proj"),
        theme: ThemeChoice::Hearth,
        prompt: None,
        mouse: true,
        spinner: true,
    };
    let mut app = App::new(
        opts,
        tx,
        app_tx,
        Caps::default(),
        Depth::True,
        UNICODE,
        (100, 30),
    );
    let now = Instant::now();

    let t = Instant::now();
    app.copy("some text to copy", now);
    let took = t.elapsed();
    assert!(
        took < Duration::from_millis(300),
        "copy held the UI thread for {took:?}"
    );

    let t = Instant::now();
    app.on_key(
        KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
        now,
    );
    let took = t.elapsed();
    assert!(
        took < Duration::from_millis(300),
        "ctrl+v held the UI thread for {took:?}"
    );

    // Both results arrive later, as messages.
    let (mut copied, mut image) = (false, false);
    rt.block_on(async {
        let end = tokio::time::Instant::now() + Duration::from_secs(10);
        while !(copied && image) {
            match tokio::time::timeout_at(end, app_rx.recv()).await {
                Ok(Some(Msg::Copied(_))) => copied = true,
                Ok(Some(Msg::ClipImage(r))) => {
                    assert!(r.is_err());
                    image = true;
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
    });
    assert!(copied, "the copy report never arrived");
    assert!(image, "the clipboard read never reported");
    let _ = std::fs::remove_dir_all(&dir);
}
