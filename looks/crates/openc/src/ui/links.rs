// OWNER: transcript
//! OSC 8 hyperlinks for URLs and file paths.
//!
//! Layout code knows which words are links, but a [`Span`] has nowhere to keep a target and the
//! ratatui buffer has no hyperlink attribute. A link therefore rides on its span as an
//! underline colour: `Style::underline_color(Color::Rgb(id))` where `id` indexes a small
//! interner. [`apply`] runs once per frame over the finished buffer, turns each marked run of
//! cells into cells that carry their own OSC 8 open and close around the glyph, and clears the
//! marker. Every cell wraps its own glyph because the terminal keeps the hyperlink as pen state:
//! ratatui only rewrites the cells that changed, and a half-wrapped run would leak a link onto
//! whatever is drawn next, or lose it from a cell that was redrawn alone.
//!
//! When the terminal is not trusted with OSC 8 the markers are still stripped, so a disabled
//! frame is byte for byte what it would have been without links.

use std::collections::HashMap;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

static ENABLED: AtomicBool = AtomicBool::new(false);
static CWD: OnceLock<String> = OnceLock::new();

#[derive(Default)]
struct Interner {
    urls: Vec<String>,
    ids: HashMap<String, u32>,
}

fn interner() -> &'static Mutex<Interner> {
    static I: OnceLock<Mutex<Interner>> = OnceLock::new();
    I.get_or_init(Mutex::default)
}

/// Remember whether paths in `cwd` can be turned into `file://` links, and turn OSC 8 on or off.
pub fn init(enabled: bool, cwd: &str) {
    ENABLED.store(enabled, Ordering::Relaxed);
    let _ = CWD.set(cwd.to_string());
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Whether to trust the terminal with OSC 8. Terminals that do not know the sequence swallow it
/// (xterm, tmux before 3.4, screen), so the default is on; the console and `dumb` would print
/// it. `OPENC_LINKS=0` or `1` overrides.
pub fn detect(term: &str, forced: Option<&str>) -> bool {
    match forced {
        Some("0" | "off" | "false") => return false,
        Some("1" | "on" | "true") => return true,
        _ => {}
    }
    !(term.is_empty() || term == "dumb" || term == "linux")
}

fn intern(url: &str) -> u32 {
    let mut g = interner().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(id) = g.ids.get(url) {
        return *id;
    }
    // 24 bits of id fit in the colour; far more links than a session will see.
    if g.urls.len() >= 0xFF_FFFE {
        return 0;
    }
    g.urls.push(url.to_string());
    let id = g.urls.len() as u32;
    g.ids.insert(url.to_string(), id);
    id
}

fn url_of(id: u32) -> Option<String> {
    let g = interner().lock().unwrap_or_else(|e| e.into_inner());
    g.urls.get(id.checked_sub(1)? as usize).cloned()
}

fn marker(id: u32) -> Color {
    Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
}

fn id_of(c: Color) -> Option<u32> {
    match c {
        Color::Rgb(r, g, b) => Some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)),
        _ => None,
    }
}

/// `style` with a hyperlink to `url` attached.
pub fn with_link(style: Style, url: &str) -> Style {
    if url.is_empty() || url.chars().any(char::is_control) || url.len() > 2000 {
        return style;
    }
    match intern(url) {
        0 => style,
        id => style.underline_color(marker(id)),
    }
}

/// The URL a style links to, if it carries a link marker.
pub fn url_in(style: Style) -> Option<String> {
    style
        .underline_color
        .and_then(|c| c.marker_id())
        .and_then(url_of)
}

/// May a Markdown link with this destination become a clickable target? Only the schemes a
/// person expects from a link in an answer: `file:`, `ssh:`, `javascript:`, `vscode:` and the
/// like are handled by programs the text never named.
pub fn clickable_scheme(dest: &str) -> bool {
    let d = dest.trim_start().to_ascii_lowercase();
    d.starts_with("https://") || d.starts_with("http://") || d.starts_with("mailto:")
}

/// The host of a URL or a bare domain, lowercase, without credentials, port or `www.`.
/// `https://github.com@evil.example/` is evil.example's.
pub fn host_of(text: &str) -> Option<String> {
    let t = text.trim();
    let rest = t.split_once("://").map_or(t, |(_, r)| r);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let auth = auth.rsplit('@').next().unwrap_or(auth);
    let host = auth.split(':').next().unwrap_or(auth).to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    (host.contains('.') && !host.contains(char::is_whitespace)).then_some(host)
}

/// Link text that names a place (a URL, a domain) other than where the link goes. Returns the
/// host the link really goes to, so the reader can be told.
pub fn spoofed_host(text: &str, dest: &str) -> Option<String> {
    let t = text.trim();
    // Only text that is itself a URL or a domain claims a destination.
    let claims = t.contains("://")
        || (!t.contains(char::is_whitespace)
            && t.split('/').next().is_some_and(|h| {
                h.contains('.')
                    && h.chars()
                        .all(|c| c.is_alphanumeric() || c == '.' || c == '-')
            }));
    if !claims {
        return None;
    }
    let shown = host_of(t)?;
    let real = host_of(dest)?;
    (shown != real).then_some(real)
}

/// Rewrite the frame: marked runs become OSC 8 cells (when enabled), markers are cleared.
pub fn apply(buf: &mut Buffer) {
    apply_with(buf, enabled());
}

fn apply_with(buf: &mut Buffer, on: bool) {
    let area = buf.area;
    let w = area.width as usize;
    if w == 0 {
        return;
    }
    let cells = buf.content.len();
    let mut i = 0;
    while i < cells {
        let Some(id) = buf.content[i].underline_color.marker_id() else {
            i += 1;
            continue;
        };
        // A run of cells with one id on one row.
        let row_end = (i / w + 1) * w;
        let mut j = i;
        while j < row_end && buf.content[j].underline_color.marker_id() == Some(id) {
            j += 1;
        }
        let url = if on { url_of(id) } else { None };
        let mut k = i;
        while k < j {
            let cell = &mut buf.content[k];
            cell.underline_color = Color::Reset;
            let width = cell.symbol().width().max(1);
            if let Some(url) = &url {
                if !cell.symbol().is_empty() {
                    let sym = format!("\x1b]8;id=o{id};{url}\x1b\\{}\x1b]8;;\x1b\\", cell.symbol());
                    cell.set_symbol(&sym);
                    cell.diff_option =
                        CellDiffOption::ForcedWidth(NonZeroU16::new(width as u16).expect("max(1)"));
                }
            }
            // Cells a wide glyph covers belong to it; they carry the marker but are not
            // written, so only clear them.
            for c in k + 1..(k + width).min(j) {
                buf.content[c].underline_color = Color::Reset;
            }
            k += width;
        }
        i = j.max(i + 1);
    }
}

trait MarkerId {
    fn marker_id(&self) -> Option<u32>;
}

impl MarkerId for Color {
    fn marker_id(&self) -> Option<u32> {
        // Reset is the normal state; a real underline colour is never set by openc.
        if *self == Color::Reset {
            None
        } else {
            id_of(*self).filter(|id| *id != 0)
        }
    }
}

// ---- finding links in text ---------------------------------------------------------------

/// Characters that end a URL or path token without being part of it.
const TRAIL: &[char] = &[
    '.', ',', ';', ':', '!', '?', '\'', '"', '`', ')', ']', '}', '>',
];
const LEAD: &[char] = &['(', '[', '{', '<', '\'', '"', '`'];

/// Extensions that make a bare `name.ext` worth a filesystem check.
const EXTS: &[&str] = &[
    "rs", "toml", "md", "txt", "json", "yaml", "yml", "py", "js", "ts", "tsx", "jsx", "go", "c",
    "h", "cc", "cpp", "hpp", "java", "kt", "swift", "rb", "sh", "fish", "nix", "lock", "css",
    "html", "sql", "lua", "zig", "cfg", "ini", "log", "csv", "xml", "svg", "png", "jpg", "pdf",
];

fn exists(path: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some(v) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .copied()
    {
        return v;
    }
    let v = std::fs::metadata(path).is_ok();
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    if g.len() > 4096 {
        g.clear();
    }
    g.insert(path.to_string(), v);
    v
}

/// Absolute path of a path-looking token, with a trailing `:line[:col]` split off.
fn resolve_path(tok: &str) -> Option<(String, Option<String>)> {
    let (body, pos) = split_position(tok);
    if body.is_empty() || body.len() > 1024 || body.contains("://") {
        return None;
    }
    let abs = if let Some(rest) = body.strip_prefix("~/") {
        format!(
            "{}/{rest}",
            std::env::var("HOME").ok()?.trim_end_matches('/')
        )
    } else if body.starts_with('/') {
        body.to_string()
    } else {
        let cwd = CWD.get()?;
        if cwd.is_empty() {
            return None;
        }
        let rel = body.strip_prefix("./").unwrap_or(body);
        // `a/b`, `../x`, or `name.ext` with a known extension; plain words are not paths.
        let has_slash = rel.contains('/');
        let ext_ok = rel
            .rsplit_once('.')
            .is_some_and(|(stem, e)| !stem.is_empty() && EXTS.contains(&e));
        if !(has_slash || ext_ok) || !rel.chars().any(|c| c.is_alphanumeric()) {
            return None;
        }
        format!("{}/{rel}", cwd.trim_end_matches('/'))
    };
    if abs.len() < 2 || !exists(&abs) {
        return None;
    }
    Some((abs, pos))
}

fn split_position(tok: &str) -> (&str, Option<String>) {
    // `src/main.rs:42` and `src/main.rs:42:7`
    let mut body = tok;
    let mut nums: Vec<&str> = Vec::new();
    for _ in 0..2 {
        match body.rsplit_once(':') {
            Some((head, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
                nums.push(n);
                body = head;
            }
            _ => break,
        }
    }
    if nums.is_empty() {
        (tok, None)
    } else {
        nums.reverse();
        (body, Some(nums.join(":")))
    }
}

fn percent_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn is_url(tok: &str) -> bool {
    let Some(rest) = tok
        .strip_prefix("https://")
        .or_else(|| tok.strip_prefix("http://"))
    else {
        return false;
    };
    rest.chars().next().is_some_and(char::is_alphanumeric)
}

/// Balanced trailing punctuation: `https://x.org/a_(b)` keeps its `)`, `(https://x.org)` does not.
fn trim_token(tok: &str) -> &str {
    let mut t = tok;
    loop {
        let Some(last) = t.chars().last() else {
            return t;
        };
        if !TRAIL.contains(&last) {
            return t;
        }
        let balanced = match last {
            ')' => t.matches('(').count() >= t.matches(')').count(),
            ']' => t.matches('[').count() >= t.matches(']').count(),
            _ => false,
        };
        if balanced {
            return t;
        }
        t = &t[..t.len() - last.len_utf8()];
    }
}

/// The OSC 8 target of one token, if it is a URL or a path that exists.
pub fn target_of(tok: &str) -> Option<String> {
    if is_url(tok) {
        return Some(tok.to_string());
    }
    let (abs, pos) = resolve_path(tok)?;
    let _ = pos;
    Some(format!("file://{}", percent_encode(&abs)))
}

/// Split `text` into spans, attaching a hyperlink to every URL and existing path in it. URLs
/// are drawn in `url_style`; paths keep `base`, so prose does not turn into a field of
/// underlines.
pub fn linkify(text: &str, base: Style, url_style: Style) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut plain_from = 0;
    let mut pos = 0;
    let bytes = text.len();
    // Candidate tokens are whitespace-separated words; cheap rejection first.
    while pos < bytes {
        let rest = &text[pos..];
        let ws = rest.len() - rest.trim_start().len();
        pos += ws;
        let rest = &text[pos..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        let word_at = pos;
        pos += end;
        if word.len() < 3 {
            continue;
        }
        let lead = word.len() - word.trim_start_matches(LEAD).len();
        let core_full = &word[lead..];
        let core = trim_token(core_full);
        if core.len() < 3 || !(core.contains('/') || core.contains('.')) {
            continue;
        }
        let Some(url) = target_of(core) else {
            continue;
        };
        let start = word_at + lead;
        if start > plain_from {
            out.push(Span::styled(text[plain_from..start].to_string(), base));
        }
        let st = if is_url(core) { url_style } else { base };
        out.push(Span::styled(core.to_string(), with_link(st, &url)));
        plain_from = start + core.len();
    }
    if plain_from < text.len() {
        out.push(Span::styled(text[plain_from..].to_string(), base));
    }
    if out.is_empty() {
        out.push(Span::styled(text.to_string(), base));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn urls_are_found_and_trailing_punctuation_stays_outside() {
        let st = Style::new();
        let spans = linkify(
            "see https://example.com/a_(b), and (https://x.org).",
            st,
            st,
        );
        assert_eq!(
            text(&spans),
            "see https://example.com/a_(b), and (https://x.org)."
        );
        let linked: Vec<&str> = spans
            .iter()
            .filter(|s| s.style.underline_color.is_some())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(linked, ["https://example.com/a_(b)", "https://x.org"]);
    }

    #[test]
    fn plain_words_are_not_links() {
        let st = Style::new();
        for t in [
            "hello world",
            "e.g. this",
            "a/b",
            "3.14 and v1.2.3",
            "http://",
        ] {
            let spans = linkify(t, st, st);
            assert!(
                spans.iter().all(|s| s.style.underline_color.is_none()),
                "{t:?}"
            );
            assert_eq!(text(&spans), t);
        }
    }

    #[test]
    fn existing_paths_link_with_line_numbers_split_off() {
        let dir = std::env::current_dir().expect("cwd");
        init(true, &dir.to_string_lossy());
        let st = Style::new();
        let spans = linkify("edit Cargo.toml:12:3 now", st, st);
        let linked: Vec<_> = spans
            .iter()
            .filter(|s| s.style.underline_color.is_some())
            .collect();
        assert_eq!(linked.len(), 1, "{spans:?}");
        assert_eq!(linked[0].content, "Cargo.toml:12:3");
        let id = id_of(linked[0].style.underline_color.unwrap()).unwrap();
        let url = url_of(id).unwrap();
        assert!(
            url.starts_with("file:///") && url.ends_with("/Cargo.toml"),
            "{url}"
        );
        // A path that is not there is plain text.
        let spans = linkify("see nothing/here.rs", st, st);
        assert!(spans.iter().all(|s| s.style.underline_color.is_none()));
    }

    #[test]
    fn apply_wraps_each_cell_and_clears_the_marker() {
        let mut b = Buffer::empty(Rect::new(0, 0, 8, 2));
        let st = with_link(Style::new(), "https://a.test/x");
        b.set_string(1, 0, "ab", st);
        b.set_string(1, 1, "cd", Style::new());
        apply_with(&mut b, true);
        let a = b[(1, 0)].symbol().to_string();
        assert!(
            a.starts_with("\x1b]8;id=o") && a.contains(";https://a.test/x\x1b\\a\x1b]8;;\x1b\\"),
            "{a:?}"
        );
        assert_eq!(b[(1, 0)].underline_color, Color::Reset);
        assert_eq!(b[(1, 1)].symbol(), "c");
        assert_eq!(b[(0, 0)].symbol(), " ");
        // Each wrapped cell still counts as one column to the diff.
        assert!(matches!(
            b[(1, 0)].diff_option,
            CellDiffOption::ForcedWidth(w) if w.get() == 1
        ));
    }

    #[test]
    fn disabled_frames_only_lose_the_marker() {
        let mut b = Buffer::empty(Rect::new(0, 0, 4, 1));
        b.set_string(0, 0, "ab", with_link(Style::new(), "https://a.test"));
        apply_with(&mut b, false);
        assert_eq!(b[(0, 0)].symbol(), "a");
        assert_eq!(b[(0, 0)].underline_color, Color::Reset);
    }

    #[test]
    fn detect_honours_the_override() {
        assert!(detect("xterm-256color", None));
        assert!(!detect("linux", None));
        assert!(!detect("dumb", None));
        assert!(!detect("xterm", Some("0")));
        assert!(detect("linux", Some("1")));
    }
}
