// OWNER: input
//! Images for `ctrl+v` and pasted file paths: read the clipboard through whatever the platform
//! has, sniff the format and size from the bytes, and base64 them for the backend. Nothing here
//! touches the terminal.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agent_core::ImageData;

/// The API rejects images over 5 MB (after base64 it counts the encoded size).
pub const MAX_BYTES: usize = 3_750_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Image {
    pub fn to_data(&self) -> ImageData {
        ImageData {
            media_type: self.media_type.to_string(),
            base64: b64(&self.bytes),
        }
    }

    /// The chip text: `[image 1280x720]`.
    pub fn label(&self) -> String {
        format!("[image {}x{}]", self.width, self.height)
    }
}

pub fn b64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for chunk in b.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Format and pixel size from the file header. `None` when it is not an image the API takes.
pub fn sniff(b: &[u8]) -> Option<(&'static str, u32, u32)> {
    if b.len() > 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") && &b[12..16] == b"IHDR" {
        let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
        return Some(("image/png", w, h));
    }
    if b.len() > 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        let w = u32::from(u16::from_le_bytes([b[6], b[7]]));
        let h = u32::from(u16::from_le_bytes([b[8], b[9]]));
        return Some(("image/gif", w, h));
    }
    if b.len() > 4 && b.starts_with(&[0xff, 0xd8]) {
        // Walk the marker segments to the first start-of-frame.
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xff {
                i += 1;
                continue;
            }
            let m = b[i + 1];
            if m == 0xff {
                i += 1;
                continue;
            }
            if (0xc0..=0xcf).contains(&m) && !matches!(m, 0xc4 | 0xc8 | 0xcc) {
                let h = u32::from(u16::from_be_bytes([b[i + 5], b[i + 6]]));
                let w = u32::from(u16::from_be_bytes([b[i + 7], b[i + 8]]));
                return Some(("image/jpeg", w, h));
            }
            if m == 0xd8 || m == 0x01 || (0xd0..=0xd7).contains(&m) {
                i += 2;
                continue;
            }
            let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
            i += 2 + len;
        }
        return None;
    }
    if b.len() > 30 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        let dim = match &b[12..16] {
            b"VP8X" => {
                let w = 1 + (u32::from(b[24]) | u32::from(b[25]) << 8 | u32::from(b[26]) << 16);
                let h = 1 + (u32::from(b[27]) | u32::from(b[28]) << 8 | u32::from(b[29]) << 16);
                Some((w, h))
            }
            b"VP8L" => {
                let v = u32::from_le_bytes(b[21..25].try_into().ok()?);
                Some((1 + (v & 0x3fff), 1 + ((v >> 14) & 0x3fff)))
            }
            b"VP8 " => {
                let w = u32::from(u16::from_le_bytes([b[26], b[27]]) & 0x3fff);
                let h = u32::from(u16::from_le_bytes([b[28], b[29]]) & 0x3fff);
                Some((w, h))
            }
            _ => None,
        };
        return dim.map(|(w, h)| ("image/webp", w, h));
    }
    None
}

pub fn from_bytes(bytes: Vec<u8>) -> Result<Image, String> {
    let (media_type, width, height) =
        sniff(&bytes).ok_or("that is not a PNG, JPEG, GIF or WebP")?;
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "image is {:.1} MB, the limit is 5 MB once encoded",
            bytes.len() as f64 / 1e6
        ));
    }
    Ok(Image {
        media_type,
        bytes,
        width,
        height,
    })
}

/// A pasted string that is one path to an image file. Handles quotes, `file://` and the
/// backslash escapes terminals add when a file is dropped on them.
pub fn image_path(pasted: &str, cwd: &Path) -> Option<PathBuf> {
    let t = pasted.trim();
    if t.is_empty() || t.contains('\n') || t.len() > 1024 {
        return None;
    }
    let t = t.trim_matches(|c| c == '\'' || c == '"');
    let t = t.strip_prefix("file://").unwrap_or(t);
    let mut path = String::with_capacity(t.len());
    let mut it = t.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                path.push(n);
            }
        } else {
            path.push(c);
        }
    }
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        return None;
    }
    let p = if let Some(rest) = path.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(rest)
    } else {
        let p = PathBuf::from(&path);
        if p.is_absolute() {
            p
        } else {
            cwd.join(p)
        }
    };
    p.is_file().then_some(p)
}

pub fn from_file(p: &Path) -> Result<Image, String> {
    let md = std::fs::metadata(p).map_err(|e| e.to_string())?;
    if md.len() as usize > MAX_BYTES * 2 {
        return Err(format!("{} is too large to send", p.display()));
    }
    from_bytes(std::fs::read(p).map_err(|e| e.to_string())?)
}

/// Run a command and collect stdout, giving up after `limit`. A clipboard tool that hangs
/// (no compositor, a dead X server) must not freeze the interface.
fn run(cmd: &str, args: &[&str], limit: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let out = child.stdout.take()?;
    // Past the size limit the image is refused anyway; do not hold more than that in memory.
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.take(MAX_BYTES as u64 * 2 + 1).read_to_end(&mut v);
        v
    });
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let v = reader.join().ok()?;
                return (st.success() && !v.is_empty()).then_some(v);
            }
            Ok(None) if t0.elapsed() < limit => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

const TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// The image on the clipboard, or why there is none.
pub fn read_clipboard() -> Result<Image, String> {
    let limit = Duration::from_millis(1500);
    let ssh = std::env::var_os("SSH_CONNECTION").is_some();
    let mut tried = false;
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        tried = true;
        if let Some(list) = run("wl-paste", &["--list-types"], limit) {
            let list = String::from_utf8_lossy(&list).into_owned();
            for t in TYPES {
                if list.lines().any(|l| l.trim() == t) {
                    if let Some(b) = run("wl-paste", &["--type", t, "--no-newline"], limit) {
                        return from_bytes(b);
                    }
                }
            }
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        tried = true;
        for t in TYPES {
            if let Some(b) = run("xclip", &["-selection", "clipboard", "-t", t, "-o"], limit) {
                if sniff(&b).is_some() {
                    return from_bytes(b);
                }
            }
        }
    }
    if cfg!(target_os = "macos") {
        tried = true;
        if let Some(b) = run("pngpaste", &["-"], limit) {
            return from_bytes(b);
        }
    }
    Err(if ssh && !tried {
        "no clipboard over ssh: paste an image file path instead".to_string()
    } else if tried {
        "no image on the clipboard".to_string()
    } else {
        "no clipboard tool found (wl-paste, xclip or pngpaste)".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3x2 PNG header; the rest of the file is not needed to size it.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v.extend([8, 2, 0, 0, 0]);
        v
    }

    #[test]
    fn sizes_come_from_the_headers() {
        assert_eq!(sniff(&png(1280, 720)), Some(("image/png", 1280, 720)));
        let mut gif = b"GIF89a".to_vec();
        gif.extend([0x40, 0x01, 0xc8, 0x00, 0, 0, 0]);
        assert_eq!(sniff(&gif), Some(("image/gif", 320, 200)));
        // JPEG: SOI, an APP0 segment, then SOF0 with 480 high and 640 wide.
        let mut jpg = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0, 0];
        jpg.extend([0xff, 0xc0, 0x00, 0x0b, 8, 0x01, 0xe0, 0x02, 0x80, 3, 0, 0]);
        assert_eq!(sniff(&jpg), Some(("image/jpeg", 640, 480)));
        assert_eq!(sniff(b"hello world, not an image at all...."), None);
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_dropped_path_is_found_through_quotes_and_escapes() {
        let d = std::env::temp_dir().join(format!("openc-clip-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("shot one.png");
        std::fs::write(&f, png(2, 2)).unwrap();
        let esc = f.to_string_lossy().replace(' ', "\\ ");
        assert_eq!(image_path(&esc, &d), Some(f.clone()));
        assert_eq!(
            image_path(&format!("'{}'", f.display()), &d),
            Some(f.clone())
        );
        assert_eq!(
            image_path(&format!("file://{}", f.display()), &d),
            Some(f.clone())
        );
        assert_eq!(image_path("shot one.png", &d), Some(f.clone()));
        assert_eq!(image_path("notes.txt", &d), None);
        assert_eq!(image_path("two\nlines.png", &d), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_oversize_image_is_refused_with_its_size() {
        let mut v = png(10, 10);
        v.resize(MAX_BYTES + 1, 0);
        assert!(from_bytes(v).unwrap_err().contains("5 MB"));
    }
}
