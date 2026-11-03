//! Image paste for the claude look.
//!
//! Copying a picture in Finder puts the file on the clipboard, and macOS also offers a PNG
//! of the file's icon. Reading that PNG attaches the icon, not the picture. A file URL wins:
//! we read the file. A pasted filename that matches that file attaches it too, instead of
//! dropping the name into the prompt as text.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// True when `path` names an image file that exists.
pub fn is_image_file(path: &Path) -> bool {
    image_ext(path) && path.is_file()
}

fn image_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// The pasted string is an image to attach, not text to insert.
///
/// A path to an image file, or the filename of the image file currently on the clipboard
/// (what a terminal pastes when you copy a picture in Finder).
pub fn pasted_image(s: &str) -> Option<PathBuf> {
    let s = s.trim().trim_matches('"');
    if s.is_empty() {
        return clipboard_image_file();
    }
    let as_path = PathBuf::from(s);
    if is_image_file(&as_path) {
        return Some(as_path);
    }
    // `~/Desktop/shot.png` and a bare filename both arrive as paste text.
    if let Some(home) = std::env::var_os("HOME") {
        let expanded = s.replacen('~', &home.to_string_lossy(), 1);
        let p = PathBuf::from(&expanded);
        if is_image_file(&p) {
            return Some(p);
        }
    }
    let file = clipboard_image_file()?;
    name_matches(&file, s).then_some(file)
}

/// Finder copy pastes the filename, often without the extension.
fn name_matches(file: &Path, pasted: &str) -> bool {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = file
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    pasted == name || pasted == stem || pasted == file.to_string_lossy()
}

/// Copy `src` to `~/.wizard/attachments/` under a name with no spaces, so the prompt can
/// reference it as `@path` and wizard attaches the real bytes.
pub fn stage_file(src: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(src).ok()?;
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    stage_bytes(&bytes, ext)
}

pub fn stage_bytes(bytes: &[u8], ext: &str) -> Option<PathBuf> {
    if bytes.is_empty() {
        return None;
    }
    let dir = attachments_dir()?;
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("paste-{ms}-{}.{}", std::process::id(), ext.trim_start_matches('.'));
    let dest = dir.join(name);
    std::fs::write(&dest, bytes).ok()?;
    Some(dest)
}

fn attachments_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".wizard").join("attachments");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Prompt text wizard should see: the typed words plus `@path` for each staged image.
/// `preprocess` collects those refs and attaches the files.
pub fn prompt_with_images(text: &str, paths: &[PathBuf]) -> String {
    let mut out = text.trim_end().to_string();
    for p in paths {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push('@');
        out.push_str(&p.to_string_lossy());
    }
    out
}

/// What the transcript shows: chips, not the raw paths.
pub fn shown_with_chips(text: &str, n: usize) -> String {
    let mut out = text.trim_end().to_string();
    for i in 1..=n {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("[Image #{i}]"));
    }
    out
}

/// Replace `@/path/to/file.png` refs in a shown prompt with `[Image #N]`.
pub fn chips_in_text(text: &str) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let end = after
            .find(char::is_whitespace)
            .unwrap_or(after.len());
        let token = &after[..end];
        if image_ext(Path::new(token)) {
            n += 1;
            out.push_str(&format!("[Image #{n}]"));
        } else {
            out.push('@');
            out.push_str(token);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// File URL on the clipboard, when it points at an image file.
fn clipboard_image_file() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("osascript")
            .arg("-e")
            .arg("try\nPOSIX path of (the clipboard as «class furl»)\nend try")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() {
            return None;
        }
        let p = PathBuf::from(path);
        return is_image_file(&p).then_some(p);
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_replace_image_refs_only() {
        let s = chips_in_text("look @/tmp/shot.png and @notes.md");
        assert_eq!(s, "look [Image #1] and @notes.md");
    }

    #[test]
    fn prompt_keeps_the_path_for_wizard() {
        let p = PathBuf::from("/tmp/paste-1.png");
        assert_eq!(
            prompt_with_images("hi", &[p]),
            "hi @/tmp/paste-1.png"
        );
    }

    #[test]
    fn filename_without_extension_matches_the_file() {
        let file = PathBuf::from("/tmp/Screenshot 2026-11-03 at 11.33.05 AM.png");
        assert!(name_matches(
            &file,
            "Screenshot 2026-11-03 at 11.33.05 AM"
        ));
        assert!(!name_matches(&file, "hello"));
    }

    #[test]
    fn shown_chips_do_not_leak_the_path() {
        assert_eq!(shown_with_chips("hi", 2), "hi [Image #1] [Image #2]");
        assert_eq!(shown_with_chips("", 1), "[Image #1]");
    }
}
