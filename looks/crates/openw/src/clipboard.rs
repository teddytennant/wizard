//! Reading the system clipboard for ctrl+v. Terminals only deliver text through a bracketed
//! paste, so a copied screenshot has to be fetched from the clipboard tool itself.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum Clip {
    Text(String),
    /// An image, saved to a file the prompt can point wizard at with `@path`.
    Image {
        path: String,
    },
}

/// How long one clipboard helper may take. A hung `wl-paste` (no compositor, a dead owner)
/// otherwise held whatever was waiting for it.
const HELPER_TIMEOUT: Duration = Duration::from_secs(1);
/// Most bytes read from a helper; a screenshot is a few megabytes.
const HELPER_MAX: u64 = 64 * 1024 * 1024;

fn run(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    run_for(cmd, args, HELPER_TIMEOUT)
}

fn run_for(cmd: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.take(HELPER_MAX).read_to_end(&mut v);
        v
    });
    let end = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            // Out of time: kill it and leave the reader to finish whenever the pipe closes.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?;
    status.success().then_some(bytes)
}

fn clip_dir() -> PathBuf {
    let base = std::env::var_os("OPENW_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/openw")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("clips")
}

/// Screenshots kept in `clips/`; older ones go when a new one is saved.
const CLIPS_KEEP: usize = 20;

fn save_image(bytes: &[u8]) -> Option<String> {
    use std::io::Write;
    let dir = clip_dir();
    crate::private::create_dir_all(&dir).ok()?;
    let name = format!(
        "clip-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    );
    let p = dir.join(name);
    crate::private::create_new(&p).ok()?.write_all(bytes).ok()?;
    prune(&dir, CLIPS_KEEP);
    Some(p.display().to_string())
}

/// Delete all but the newest `keep` `clip-*.png` files in `dir`.
fn prune(dir: &std::path::Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut clips: Vec<_> = rd
        .filter_map(Result::ok)
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("clip-") && n.ends_with(".png")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    clips.sort();
    let drop = clips.len().saturating_sub(keep);
    for (_, p) in clips.into_iter().take(drop) {
        let _ = std::fs::remove_file(p);
    }
}

/// Wayland first, then X11, then macOS. `None` when there is no tool or nothing on it. Blocks
/// for up to a second per helper, so call it off the UI thread ([`read_in_background`]).
pub fn read() -> Option<Clip> {
    if let Some(types) = run("wl-paste", &["--list-types"]) {
        let types = String::from_utf8_lossy(&types);
        if types.lines().any(|t| t == "image/png") {
            if let Some(img) = run("wl-paste", &["--type", "image/png"]) {
                return save_image(&img).map(|path| Clip::Image { path });
            }
        }
        return run("wl-paste", &["--no-newline"])
            .map(|b| Clip::Text(String::from_utf8_lossy(&b).into_owned()))
            .filter(|c| matches!(c, Clip::Text(t) if !t.is_empty()));
    }
    if let Some(types) = run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]) {
        if String::from_utf8_lossy(&types)
            .lines()
            .any(|t| t == "image/png")
        {
            if let Some(img) = run(
                "xclip",
                &["-selection", "clipboard", "-t", "image/png", "-o"],
            ) {
                return save_image(&img).map(|path| Clip::Image { path });
            }
        }
        return run("xclip", &["-selection", "clipboard", "-o"])
            .map(|b| Clip::Text(String::from_utf8_lossy(&b).into_owned()))
            .filter(|c| matches!(c, Clip::Text(t) if !t.is_empty()));
    }
    run("pbpaste", &[])
        .map(|b| Clip::Text(String::from_utf8_lossy(&b).into_owned()))
        .filter(|c| matches!(c, Clip::Text(t) if !t.is_empty()))
}

/// Run [`read`] on its own thread and send the answer back as a [`crate::app::Msg::Clip`].
pub fn read_in_background(tx: tokio::sync::mpsc::UnboundedSender<crate::app::Msg>) {
    std::thread::spawn(move || {
        let _ = tx.send(crate::app::Msg::Clip(read()));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finding 17: a helper that never answers held the UI thread, 12 s for two `wl-paste` calls.
    #[test]
    fn a_helper_that_hangs_is_killed_at_its_deadline() {
        let t = Instant::now();
        assert_eq!(run_for("sleep", &["30"], Duration::from_millis(200)), None);
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert_eq!(
            run_for("echo", &["hi"], Duration::from_secs(5)).as_deref(),
            Some(&b"hi\n"[..])
        );
        assert_eq!(run_for("false", &[], Duration::from_secs(5)), None);
        assert_eq!(
            run_for("/nonexistent/helper", &[], Duration::from_secs(5)),
            None
        );
    }

    #[test]
    fn only_the_newest_clips_are_kept() {
        let d = std::env::temp_dir().join(format!("openw-clips-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..8 {
            let p = d.join(format!("clip-{i}.png"));
            std::fs::write(&p, "x").unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        std::fs::write(d.join("notes.txt"), "keep").unwrap();
        prune(&d, 3);
        let mut left: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["clip-5.png", "clip-6.png", "clip-7.png", "notes.txt"]
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
