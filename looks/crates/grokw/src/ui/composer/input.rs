//! Composer state that is not the text itself: paste and image chips, the history panel, the
//! stash, slash-command recency and the `@` picker's file index.
//!
//! Chips are plain text in the editor (`[Pasted: 40 lines]`, `[Image #1]`) with their content kept
//! here. A chip's identity is its position among the labels in the text: the n-th chip owns the
//! n-th label that matches it, so two pastes of the same size stay apart. Everything that edits
//! the text calls [`reconcile`] first, which drops chips whose label no longer matches.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::hist::Panel;
use crate::app::App;

/// Image chips allowed in one prompt.
pub const IMAGE_CAP: usize = 10;
/// A paste over this many bytes becomes a chip whatever its line count.
const CHIP_BYTES: usize = 10_000;
const RECENCY_HALF_LIFE: f64 = 7.0 * 86_400.0;
const RECENCY_FLOOR: f64 = 0.1;

#[derive(Clone, Debug)]
pub struct ImageInfo {
    pub mime: String,
    pub dims: Option<(u32, u32)>,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct Chip {
    pub image: bool,
    pub label: String,
    /// The pasted text, or the image's path.
    pub content: String,
    pub info: Option<ImageInfo>,
}

#[derive(Default)]
pub struct Input {
    pub chips: Vec<Chip>,
    pub image_counter: usize,
    pub hist: Option<Panel>,
    /// A chord stash comes back after the next send; a double-Esc stash only on the chord.
    pub stash_auto: bool,
    /// Chips that went away with the stashed text.
    pub stash_chips: Vec<Chip>,
    mru: HashMap<String, u64>,
    mru_path: Option<PathBuf>,
    /// Every file, hidden and ignored ones too, for `@!`; built the first time it is asked for.
    pub hidden_index: std::cell::OnceCell<Vec<String>>,
    /// First row of the `@` picker's window.
    pub at_scroll: usize,
    /// Cursor before the key being handled, to snap it off the inside of a chip.
    pub prev_cursor: usize,
    pub last_click: Option<(Duration, u16, u16)>,
    /// Where pasted images are written.
    pub image_dir: Option<PathBuf>,
    /// Popup row under the pointer.
    pub popup_hover: Option<usize>,
    /// The text to open in `$EDITOR`; the event loop takes it, runs the editor and hands the
    /// result to [`edit_done`].
    pub edit_request: Option<String>,
}

impl Input {
    pub fn new(state_dir: Option<&Path>) -> Input {
        let mut inp = Input::default();
        if let Some(d) = state_dir {
            let p = d.join("slash-mru.json");
            if let Ok(s) = std::fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<HashMap<String, u64>>(&s) {
                    inp.mru = v;
                }
            }
            inp.mru_path = Some(p);
            inp.image_dir = Some(d.join("images"));
        }
        inp
    }

    /// Recency score of a command: its last use scaled by a seven-day half-life, floored at a tenth.
    pub fn mru_score(&self, name: &str) -> u64 {
        let Some(&t) = self.mru.get(name) else {
            return 0;
        };
        let now = now_secs();
        let age = now.saturating_sub(t) as f64;
        let f = 0.5_f64.powf(age / RECENCY_HALF_LIFE).max(RECENCY_FLOOR);
        (t as f64 * f) as u64
    }

    pub fn touch_command(&mut self, name: &str) {
        self.mru.insert(name.to_string(), now_secs());
        if let Some(p) = &self.mru_path {
            if let Ok(s) = serde_json::to_string(&self.mru) {
                let _ = std::fs::write(p, s);
            }
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---- chip positions -----------------------------------------------------------------------

/// Where each chip's label sits in `text`: the chips take the labels in order, left to right.
pub fn spans(text: &str, chips: &[Chip]) -> Vec<Option<Range<usize>>> {
    let mut pos = 0;
    chips
        .iter()
        .map(|c| {
            let at = text[pos..].find(&c.label)? + pos;
            pos = at + c.label.len();
            Some(at..pos)
        })
        .collect()
}

/// Drop chips whose label is gone from the text.
pub fn reconcile(app: &mut App) {
    if app.inp.chips.is_empty() {
        return;
    }
    let sp = spans(app.ed.text(), &app.inp.chips);
    let mut it = sp.iter();
    app.inp
        .chips
        .retain(|_| it.next().is_some_and(|s| s.is_some()));
}

/// The chip the cursor is on: at its first cell or inside it.
pub fn chip_at(app: &App) -> Option<(usize, Range<usize>)> {
    let cur = app.ed.cursor();
    spans(app.ed.text(), &app.inp.chips)
        .into_iter()
        .enumerate()
        .find_map(|(i, s)| s.filter(|r| r.start <= cur && cur < r.end).map(|r| (i, r)))
}

/// The chip the cursor is on or right after.
pub fn chip_near(app: &App) -> Option<(usize, Range<usize>)> {
    let cur = app.ed.cursor();
    spans(app.ed.text(), &app.inp.chips)
        .into_iter()
        .enumerate()
        .find_map(|(i, s)| s.filter(|r| r.start <= cur && cur <= r.end).map(|r| (i, r)))
}

/// An image chip with the cursor on it, right after it, or one cell later (the space an insert
/// leaves behind, where the card stays up).
pub fn image_near(app: &App) -> Option<usize> {
    let cur = app.ed.cursor();
    let text = app.ed.text();
    spans(text, &app.inp.chips)
        .into_iter()
        .enumerate()
        .find_map(|(i, s)| {
            let r = s?;
            let after_space = cur == r.end + 1 && text.as_bytes().get(r.end) == Some(&b' ');
            (app.inp.chips[i].image && (r.start <= cur && cur <= r.end || after_space)).then_some(i)
        })
}

/// Move a cursor that ended up inside a chip to the edge it was heading for.
pub fn snap_cursor(app: &mut App, old: usize) {
    let cur = app.ed.cursor();
    for r in spans(app.ed.text(), &app.inp.chips).into_iter().flatten() {
        if r.start < cur && cur < r.end {
            app.ed.set_cursor(if cur > old { r.end } else { r.start });
            return;
        }
    }
}

// ---- labels -------------------------------------------------------------------------------

pub fn paste_label(lines: usize, bytes: usize) -> String {
    if bytes > CHIP_BYTES {
        let size = if bytes >= 1_000_000 {
            format!("{:.1} MB", bytes as f64 / 1_000_000.0)
        } else if bytes >= 1000 {
            format!("{} KB", bytes / 1000)
        } else {
            format!("{bytes} bytes")
        };
        return format!("[Pasted: {size}]");
    }
    format!(
        "[Pasted: {lines} line{}]",
        if lines != 1 { "s" } else { "" }
    )
}

pub fn normalize_breaks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push('\n');
            }
            '\u{2028}' | '\u{2029}' => out.push('\n'),
            _ => out.push(c),
        }
    }
    out
}

// ---- paste --------------------------------------------------------------------------------

/// A bracketed paste: a chip when it is long, inline text otherwise. Pasting a chip's own text
/// again while the cursor is on or right after it expands that chip instead.
pub fn paste(app: &mut App, raw: &str) {
    if raw.is_empty() {
        return;
    }
    reconcile(app);
    let text = normalize_breaks(raw);
    // a dropped image file arrives as its path
    if let Some(path) = dropped_image(&text) {
        match insert_image(app, &path) {
            Ok(()) => return,
            Err(msg) => {
                app.toast(msg);
                return;
            }
        }
    }
    if let Some((i, _)) = chip_near(app) {
        if !app.inp.chips[i].image && app.inp.chips[i].content == text {
            expand(app, i);
            return;
        }
    }
    // `! cmd` into an empty composer starts shell mode
    let text = if app.ed.is_empty() && !app.shell_mode {
        match text.strip_prefix("! ") {
            Some(cmd) => {
                app.shell_mode = true;
                cmd.to_string()
            }
            None => text,
        }
    } else {
        text
    };
    let lines = text.lines().count();
    let min_lines = if crate::ui::Layout::compute(app).compact {
        2
    } else {
        4
    };
    if lines >= min_lines || text.len() > CHIP_BYTES {
        let label = paste_label(lines, text.len());
        let before = app.ed.cursor();
        let idx = spans(app.ed.text(), &app.inp.chips)
            .iter()
            .filter(|s| s.as_ref().is_some_and(|r| r.start < before))
            .count();
        app.ed.paste(&label);
        app.inp.chips.insert(
            idx,
            Chip {
                image: false,
                label,
                content: text,
                info: None,
            },
        );
    } else {
        app.ed.paste(&text);
    }
}

/// Replace chip `idx` with what it stands for.
pub fn expand(app: &mut App, idx: usize) {
    let Some(Some(r)) = spans(app.ed.text(), &app.inp.chips).get(idx).cloned() else {
        return;
    };
    let chip = app.inp.chips.remove(idx);
    if chip.image {
        // an image has nothing to inline; leave it as it was
        app.inp.chips.insert(idx, chip);
        return;
    }
    app.ed.set_cursor(r.end);
    for _ in 0..chip.label.chars().count() {
        app.ed.backspace();
    }
    app.ed.paste(&chip.content);
}

/// The text with every chip replaced by its content: pasted text as is, an image as `@path`.
pub fn expand_all(app: &App, text: &str) -> String {
    let sp = spans(text, &app.inp.chips);
    let mut out = String::new();
    let mut at = 0;
    for (c, s) in app.inp.chips.iter().zip(sp) {
        let Some(r) = s else { continue };
        out.push_str(&text[at..r.start]);
        if c.image {
            out.push('@');
            out.push_str(&c.content);
        } else {
            out.push_str(&c.content);
        }
        at = r.end;
    }
    out.push_str(&text[at..]);
    out
}

/// Backspace on the end of a chip, or Delete on its start, takes the whole chip.
pub fn delete_whole(app: &mut App, forward: bool) -> bool {
    let cur = app.ed.cursor();
    let sp = spans(app.ed.text(), &app.inp.chips);
    for (i, s) in sp.into_iter().enumerate() {
        let Some(r) = s else { continue };
        let hit = if forward {
            r.start == cur
        } else {
            r.end == cur
        };
        if !hit {
            continue;
        }
        let n = app.inp.chips[i].label.chars().count();
        app.inp.chips.remove(i);
        if forward {
            for _ in 0..n {
                app.ed.delete_forward();
            }
        } else {
            for _ in 0..n {
                app.ed.backspace();
            }
        }
        return true;
    }
    false
}

// ---- images -------------------------------------------------------------------------------

const IMAGE_EXT: [(&str, &str); 7] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("tiff", "image/tiff"),
];

fn mime_of(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_EXT.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

/// The existing image file a paste names, when the paste is one path and nothing else.
fn dropped_image(text: &str) -> Option<PathBuf> {
    let t = text.trim();
    if t.is_empty() || t.contains('\n') {
        return None;
    }
    let t = t.trim_matches(|c| c == '\'' || c == '"');
    let t = t.strip_prefix("file://").unwrap_or(t);
    let p = PathBuf::from(t);
    (p.is_absolute() && p.is_file() && mime_of(&p).is_some()).then_some(p)
}

/// PNG header dimensions; other formats are accepted unchecked.
fn png_dims(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut b = [0u8; 24];
    f.read_exact(&mut b).ok()?;
    if &b[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
    let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    Some((w, h))
}

/// Insert an `[Image #N]` chip for the file at `path`. Wizard's ACP takes text only, so what goes
/// out on send is `@path` and the model reads the file with its own tools.
pub fn insert_image(app: &mut App, path: &Path) -> Result<(), String> {
    reconcile(app);
    if app.inp.chips.iter().filter(|c| c.image).count() >= IMAGE_CAP {
        return Err(format!("Image limit reached (max {IMAGE_CAP})"));
    }
    let dims = png_dims(path);
    if let Some((w, h)) = dims {
        if w < 8 || h < 8 {
            return Err(format!(
                "Image too small ({w}×{h}). Must be at least 8×8 pixels."
            ));
        }
    }
    let mime = mime_of(path).unwrap_or("image/png").to_string();
    let bytes = std::fs::metadata(path).map_or(0, |m| m.len());
    app.inp.image_counter += 1;
    let label = format!("[Image #{}]", app.inp.image_counter);
    let before = app.ed.cursor();
    let idx = spans(app.ed.text(), &app.inp.chips)
        .iter()
        .filter(|s| s.as_ref().is_some_and(|r| r.start < before))
        .count();
    app.ed.paste(&format!("{label} "));
    app.inp.chips.insert(
        idx,
        Chip {
            image: true,
            label,
            content: path.display().to_string(),
            info: Some(ImageInfo { mime, dims, bytes }),
        },
    );
    Ok(())
}

/// Ctrl+V: an image on the clipboard becomes a chip, text is pasted like a bracketed paste.
/// Reads the clipboard with `wl-paste` or `xclip`, whichever is installed; with neither there
/// is nothing to read and the toast says so.
pub fn paste_clipboard(app: &mut App) {
    if let Some(bytes) = clipboard_bytes(&["image/png"]) {
        let dir = app
            .inp
            .image_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("grokw-images"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!(
            "image-{}-{}.png",
            now_secs(),
            app.inp.image_counter + 1
        ));
        if std::fs::write(&path, &bytes).is_ok() {
            if let Err(msg) = insert_image(app, &path) {
                app.toast(msg);
            }
            return;
        }
    }
    match clipboard_bytes(&["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"]) {
        Some(b) => {
            let t = String::from_utf8_lossy(&b).to_string();
            paste(app, &t);
        }
        None => app
            .toast("Nothing to paste. Install wl-clipboard or xclip to paste from the clipboard."),
    }
}

fn clipboard_bytes(types: &[&str]) -> Option<Vec<u8>> {
    use std::process::Command;
    for t in types {
        let out = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Command::new("wl-paste")
                .args(["--no-newline", "--type", t])
                .output()
        } else {
            Command::new("xclip")
                .args(["-selection", "clipboard", "-o", "-t", t])
                .output()
        };
        if let Ok(o) = out {
            if o.status.success() && !o.stdout.is_empty() {
                return Some(o.stdout);
            }
        }
    }
    None
}

// ---- the external editor ------------------------------------------------------------------

/// Ask for `$VISUAL`, `$EDITOR` or `vi` on `text` (the draft, or nothing for `/edit-prompt`).
pub fn request_editor(app: &mut App, text: String) {
    app.inp.edit_request = Some(text);
    app.dirty = true;
}

/// Run the editor on a temporary file holding `text`. `None` when it could not run or failed.
pub fn run_editor(text: &str, cwd: &Path) -> Result<String, String> {
    let editor = var_nonempty("VISUAL")
        .or_else(|| var_nonempty("EDITOR"))
        .unwrap_or_else(|| "vi".to_string());
    let path = std::env::temp_dir().join(format!("grokw-prompt-{}.md", std::process::id()));
    std::fs::write(&path, text).map_err(|e| format!("Could not write the temporary file: {e}"))?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .current_dir(cwd)
        .status();
    let out = match status {
        Ok(s) if s.success() => std::fs::read_to_string(&path).map_err(|e| e.to_string()),
        Ok(s) => Err(format!("The editor exited with {s}")),
        Err(e) => Err(format!("Could not start the editor: {e}")),
    };
    let _ = std::fs::remove_file(&path);
    out
}

fn var_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// What the editor wrote becomes the draft; one trailing newline (the editor's) is dropped. An
/// empty file leaves the draft alone.
pub fn edit_done(app: &mut App, res: Result<String, String>) {
    match res {
        Ok(t) => {
            let t = normalize_breaks(&t);
            let t = t.strip_suffix('\n').unwrap_or(&t).to_string();
            if !t.is_empty() {
                app.inp.chips.clear();
                app.shell_mode = false;
                app.ed.set_text(&t);
            }
        }
        Err(msg) => app.toast(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(paste_label(40, 600), "[Pasted: 40 lines]");
        assert_eq!(paste_label(1, 600), "[Pasted: 1 line]");
        assert_eq!(paste_label(2, 12_345), "[Pasted: 12 KB]");
        assert_eq!(paste_label(2, 1_500_000), "[Pasted: 1.5 MB]");
    }

    #[test]
    fn same_label_chips_keep_their_own_content() {
        let chips = vec![
            Chip {
                image: false,
                label: "[P]".into(),
                content: "a".into(),
                info: None,
            },
            Chip {
                image: false,
                label: "[P]".into(),
                content: "b".into(),
                info: None,
            },
        ];
        let sp = spans("x [P] y [P]", &chips);
        assert_eq!(sp, vec![Some(2..5), Some(8..11)]);
    }

    #[test]
    fn the_editor_round_trip_uses_visual_then_editor() {
        let dir = std::env::temp_dir().join("grokw-edit-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ed = dir.join("ed.sh");
        std::fs::write(
            &ed,
            "#!/bin/sh\nprintf 'edited: ' | cat - \"$1\" > \"$1.new\" && mv \"$1.new\" \"$1\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ed, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("VISUAL", &ed);
        let out = run_editor("draft\n", &dir).unwrap();
        assert_eq!(out, "edited: draft\n");
        std::env::set_var("VISUAL", "false");
        assert!(run_editor("x", &dir).is_err());
        std::env::remove_var("VISUAL");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn breaks() {
        assert_eq!(normalize_breaks("a\r\nb\rc\u{2028}d"), "a\nb\nc\nd");
    }
}
