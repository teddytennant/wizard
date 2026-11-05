//! Clipboard and image-paste plumbing: data-URL parsing, OS clipboard
//! readers, and resolving pasted path tokens to image files.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Parse a `data:image/<subtype>;base64,<payload>` URL. Returns `(mime, b64)`.
pub(super) fn parse_data_image_url(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if !meta.contains(";base64") {
        return None;
    }
    let mime = meta.split(';').next()?.trim();
    if !mime.starts_with("image/") {
        return None;
    }
    Some((mime, payload.trim()))
}

/// Write raw image `bytes` under `~/.wizard/attachments/` with extension `ext`,
/// enforcing the model's image size cap. Shared by the data-URL and OS-clipboard
/// paste paths.
pub(super) fn save_image_bytes(bytes: &[u8], ext: &str) -> Result<PathBuf, String> {
    if bytes.len() > crate::llm::MAX_IMAGE_BYTES {
        return Err(format!(
            "image is {} bytes (max {} MB)",
            bytes.len(),
            crate::llm::MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let dir = crate::config::Config::wizard_dir()
        .map_err(|err| err.to_string())?
        .join("attachments");
    std::fs::create_dir_all(&dir).map_err(|err| format!("mkdir {}: {err}", dir.display()))?;
    let name = format!(
        "paste-{}-{}.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        &uuid::Uuid::new_v4().to_string()[..8],
        ext
    );
    let path = dir.join(name);
    std::fs::write(&path, bytes).map_err(|err| format!("write {}: {err}", path.display()))?;
    Ok(path)
}

/// Decode a `data:image/...;base64,...` payload and save it under attachments.
pub(super) fn save_pasted_image_bytes(mime: &str, b64: &str) -> Result<PathBuf, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|err| format!("invalid base64: {err}"))?;
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    };
    save_image_bytes(&bytes, ext)
}

/// Identify a supported image format from its magic bytes, returning the file
/// extension to save it under. Doubles as validation: bytes that are not one of
/// the formats the model accepts return `None`, so junk on the clipboard is
/// never staged.
pub(super) fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Read an image off the OS clipboard as raw bytes, if one is present.
///
/// Terminals cannot deliver pasted image *data* through bracketed paste — an
/// image paste arrives as an empty paste — so the bytes have to be fetched from
/// the system clipboard directly. Each platform shells out to the tool that can
/// read binary clipboard content; a missing tool or a text-only clipboard just
/// yields `None`.
pub(super) fn clipboard_image_bytes() -> Option<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        macos_clipboard_bytes()
    }
    #[cfg(target_os = "windows")]
    {
        windows_clipboard_bytes()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        linux_clipboard_bytes()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        None
    }
}

/// Run `cmd args`, returning captured stdout when it exits cleanly with output.
#[cfg(unix)]
fn capture(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

/// Read a clipboard image on Linux/BSD: Wayland (`wl-clipboard`) first, then
/// X11 (`xclip`).
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_clipboard_bytes() -> Option<Vec<u8>> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && let Some(types) = capture("wl-paste", &["--list-types"])
    {
        let listing = String::from_utf8_lossy(&types);
        if let Some(ty) = listing.split_whitespace().find(|t| t.starts_with("image/"))
            && let Some(bytes) = capture("wl-paste", &["--no-newline", "--type", ty])
            && sniff_image_ext(&bytes).is_some()
        {
            return Some(bytes);
        }
    }
    for ty in ["image/png", "image/jpeg"] {
        if let Some(bytes) = capture("xclip", &["-selection", "clipboard", "-t", ty, "-o"])
            && sniff_image_ext(&bytes).is_some()
        {
            return Some(bytes);
        }
    }
    None
}

/// File URL on the macOS clipboard, when the user copied a file rather than image bytes.
/// The PNG pasteboard class in that case is the file's icon, not the picture.
#[cfg(target_os = "macos")]
fn clipboard_file_url() -> Option<std::path::PathBuf> {
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
    Some(std::path::PathBuf::from(path))
}

/// Read a clipboard image on macOS: the file itself when the clipboard holds a file URL
/// (the PNG class is only the icon), else `pngpaste`, else AppleScript's PNG bytes.
#[cfg(target_os = "macos")]
fn macos_clipboard_bytes() -> Option<Vec<u8>> {
    if let Some(path) = clipboard_file_url() {
        if path.is_file()
            && let Ok(bytes) = std::fs::read(&path)
            && sniff_image_ext(&bytes).is_some()
        {
            return Some(bytes);
        }
        // A file is on the clipboard. Its PNG representation is the icon, not a picture.
        return None;
    }
    if let Some(bytes) = capture("pngpaste", &["-"])
        && sniff_image_ext(&bytes).is_some()
    {
        return Some(bytes);
    }
    let path = std::env::temp_dir().join(format!("wizard-clip-{}.png", std::process::id()));
    let script = format!(
        "try\n\
             set png to (the clipboard as «class PNGf»)\n\
         on error\n\
             return\n\
         end try\n\
         set fh to open for access POSIX file \"{}\" with write permission\n\
         set eof fh to 0\n\
         write png to fh\n\
         close access fh",
        path.display()
    );
    let ok = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let bytes = ok.then(|| std::fs::read(&path).ok()).flatten();
    let _ = std::fs::remove_file(&path);
    bytes.filter(|b| sniff_image_ext(b).is_some())
}

/// Read a clipboard image on Windows via PowerShell's `System.Windows.Forms`
/// clipboard, saved to a temp PNG we read back.
#[cfg(target_os = "windows")]
fn windows_clipboard_bytes() -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("wizard-clip-{}.png", std::process::id()));
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
         $img = [System.Windows.Forms.Clipboard]::GetImage(); \
         if ($img -ne $null) {{ $img.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png) }}",
        path.display()
    );
    let ok = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let bytes = ok.then(|| std::fs::read(&path).ok()).flatten();
    let _ = std::fs::remove_file(&path);
    bytes.filter(|b| sniff_image_ext(b).is_some())
}

/// Whether a paste token looks like an image path (extension only — existence
/// is checked separately).
pub(super) fn looks_like_image_path_token(token: &str) -> bool {
    let cleaned = token
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .strip_prefix("file://")
        .unwrap_or(token.trim().trim_matches(|c| c == '"' || c == '\''));
    crate::commands::is_image_path(Path::new(cleaned))
}

/// Resolve a pasted path token to an existing image file.
pub(super) fn resolve_pasted_image_path(token: &str, project_root: &Path) -> Option<PathBuf> {
    let cleaned = token.trim().trim_matches(|c| c == '"' || c == '\'');
    let cleaned = cleaned.strip_prefix("file://").unwrap_or(cleaned);
    let expanded = shellexpand::tilde(cleaned);
    let candidate = Path::new(expanded.as_ref());
    let path = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_root.join(candidate)
    };
    if path.is_file() && crate::commands::is_image_path(&path) {
        Some(path.canonicalize().unwrap_or(path))
    } else {
        None
    }
}
