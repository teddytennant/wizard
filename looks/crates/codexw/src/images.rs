//! Image attachments for the composer: telling a pasted path from text, reading a file's size
//! from its header, and pulling an image off the clipboard. Wizard takes text only over ACP, so
//! an attached image reaches it as `@<path>`; this module never encodes pixels.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The extensions `@` and Ctrl+V treat as images (Codex lists the same).
pub fn has_image_extension(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp"
        )
    })
}

/// A pasted string as a single filesystem path: surrounding quotes, `file://` URLs and shell
/// escapes (`my\ pic.png`) are undone. `None` when it is not one path (Codex `normalize_pasted_path`).
pub fn normalize_pasted_path(pasted: &str) -> Option<PathBuf> {
    let pasted = pasted.trim();
    let unquoted = pasted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| pasted.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(pasted);
    if let Some(rest) = unquoted.strip_prefix("file://") {
        // `file://host/path` is not local; `file:///path` and `file://localhost/path` are.
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        return rest
            .starts_with('/')
            .then(|| PathBuf::from(percent_decode(rest)));
    }
    let parts = crate::editor::split_command(pasted)?;
    match parts.as_slice() {
        [one] if !one.is_empty() => Some(PathBuf::from(one)),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Width and height from the file header: PNG, GIF, JPEG and WebP. `None` for anything else, so
/// a text file named `x.png` is not an image.
pub fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = Vec::with_capacity(64 * 1024);
    f.by_ref().take(64 * 1024).read_to_end(&mut head).ok()?;
    dimensions_of(&head)
}

pub fn dimensions_of(b: &[u8]) -> Option<(u32, u32)> {
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
    let le16 = |i: usize| Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32);
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        return Some((be32(16)?, be32(20)?)).filter(|d| d.0 > 0 && d.1 > 0);
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some((le16(6)?, le16(8)?)).filter(|d| d.0 > 0 && d.1 > 0);
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut i = 2;
        while i + 4 <= b.len() {
            if b[i] != 0xff {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            if marker == 0xff {
                i += 1;
                continue;
            }
            if matches!(marker, 0xd8 | 0x01 | 0xd0..=0xd7) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]) as u32;
                let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]) as u32;
                return Some((w, h)).filter(|d| d.0 > 0 && d.1 > 0);
            }
            i += 2 + len;
        }
        return None;
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16)? {
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let v = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => {
                let w = 1 + (b[24] as u32 | (b[25] as u32) << 8 | (b[26] as u32) << 16);
                let h = 1 + (b[27] as u32 | (b[28] as u32) << 8 | (b[29] as u32) << 16);
                Some((w, h))
            }
            _ => None,
        };
    }
    None
}

/// An image file the paste handler may attach: it exists, has an image extension and a readable
/// header.
pub fn pasted_image(pasted: &str) -> Option<PathBuf> {
    let path = normalize_pasted_path(pasted)?;
    if !has_image_extension(&path) {
        return None;
    }
    image_dimensions(&path)?;
    Some(path)
}

/// Ctrl+V: save the clipboard's image as a PNG under the system temp directory and return its
/// path. Tries `wl-paste` (Wayland), then `xclip` (X11); the error says what was missing.
pub fn clipboard_image_to_temp() -> Result<PathBuf, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let mut tried: Vec<String> = Vec::new();
    let tools: [(&str, &[&str], bool); 2] = [
        (
            "wl-paste",
            &["--no-newline", "--type", "image/png"],
            wayland,
        ),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
            x11,
        ),
    ];
    for (bin, args, usable) in tools {
        if !usable {
            continue;
        }
        let out = Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        match out {
            Ok(o) if o.status.success() && dimensions_of(&o.stdout).is_some() => {
                let path = std::env::temp_dir().join(format!(
                    "codexw-clipboard-{}-{}.png",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis())
                ));
                std::fs::write(&path, &o.stdout)
                    .map_err(|e| format!("could not save the image: {e}"))?;
                return Ok(path);
            }
            Ok(o) => {
                let why = String::from_utf8_lossy(&o.stderr).trim().to_string();
                tried.push(if why.is_empty() {
                    format!("{bin} found no image")
                } else {
                    format!("{bin}: {why}")
                });
            }
            Err(_) => tried.push(format!("{bin} is not installed")),
        }
    }
    if tried.is_empty() {
        return Err(
            "clipboard unavailable: no display (WAYLAND_DISPLAY and DISPLAY are unset)".into(),
        );
    }
    Err(format!("no image on clipboard: {}", tried.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R', 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89,
    ];

    #[test]
    fn pasted_paths_lose_quotes_urls_and_escapes() {
        let p = |s: &str| normalize_pasted_path(s);
        assert_eq!(p("/a/b.png"), Some("/a/b.png".into()));
        assert_eq!(p("  \"/a/my pic.png\"  "), Some("/a/my pic.png".into()));
        assert_eq!(p("'/a/my pic.png'"), Some("/a/my pic.png".into()));
        assert_eq!(p(r"/a/my\ pic.png"), Some("/a/my pic.png".into()));
        assert_eq!(p("file:///a/my%20pic.png"), Some("/a/my pic.png".into()));
        assert_eq!(p("file://localhost/a/b.png"), Some("/a/b.png".into()));
        assert_eq!(p("file://otherhost/a/b.png"), None);
        assert_eq!(p("two words"), None);
        assert_eq!(p(""), None);
    }

    #[test]
    fn headers_give_sizes_and_garbage_gives_none() {
        assert_eq!(dimensions_of(PNG_1X1), Some((1, 1)));
        assert_eq!(dimensions_of(b"GIF89a\x04\x00\x03\x00"), Some((4, 3)));
        // JPEG: SOI, APP0 (length 4), SOF0 with height 5 and width 7
        let jpg = [
            0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 11, 8, 0, 5, 0, 7, 1, 1, 0x11, 0,
        ];
        assert_eq!(dimensions_of(&jpg), Some((7, 5)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        webp.extend_from_slice(&[9, 0, 0, 19, 0, 0]); // width-1 = 9, height-1 = 19
        assert_eq!(dimensions_of(&webp), Some((10, 20)));
        assert_eq!(dimensions_of(b"not an image at all"), None);
        assert_eq!(dimensions_of(b""), None);
    }

    #[test]
    fn only_real_images_with_an_image_extension_attach() {
        let dir = std::env::temp_dir().join(format!("cxw-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a b.PNG");
        let fake = dir.join("fake.png");
        let txt = dir.join("note.txt");
        std::fs::write(&png, PNG_1X1).unwrap();
        std::fs::write(&fake, "text").unwrap();
        std::fs::write(&txt, PNG_1X1).unwrap();
        let q = |p: &Path| format!("\"{}\"", p.display());
        assert_eq!(pasted_image(&q(&png)), Some(png.clone()));
        assert_eq!(pasted_image(&q(&fake)), None);
        assert_eq!(pasted_image(&q(&txt)), None);
        assert_eq!(pasted_image(&q(&dir.join("missing.png"))), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
