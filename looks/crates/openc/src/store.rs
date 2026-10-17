// OWNER: input
//! Small on-disk state: prompt history (shared by every session, this directory's entries
//! newest) and per-session drafts. Everything is best effort; a read-only home never breaks
//! the UI.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

const HISTORY_LINES: usize = 2000;

static OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Keep state somewhere else for the rest of the process. Tests use it so they never touch
/// the real history; the first call wins.
pub fn use_dir(dir: PathBuf) {
    let _ = OVERRIDE.set(dir);
}

fn dir() -> Option<PathBuf> {
    if let Some(d) = OVERRIDE.get() {
        return Some(d.clone());
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("openc"))
}

/// Prompts and drafts hold whatever you were about to send: tokens, pasted logs. Only the
/// owner reads them, whatever the umask or the home directory's mode says.
fn make_dir(d: &std::path::Path) {
    let _ = fs::DirBuilder::new().recursive(true).mode(0o700).create(d);
}

fn private_file() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.create(true).mode(0o600);
    o
}

/// Files made by an older version carry the default mode; close them on the way past.
fn tighten(f: &fs::File) {
    if let Ok(m) = f.metadata() {
        if m.permissions().mode() & 0o077 != 0 {
            let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
        }
    }
}

/// Most of the file read back. Lines are small, so this is far more than `HISTORY_LINES`.
const HISTORY_READ: u64 = 4 * 1024 * 1024;
/// Past this the file is cut back to its newest lines when a session starts.
const HISTORY_MAX: u64 = 16 * 1024 * 1024;
/// One entry is kept at most this long. A pasted file is not a prompt you will recall.
const ENTRY_MAX: usize = 32 * 1024;

pub fn load_history(cwd: &str) -> Vec<String> {
    let Some(path) = dir().map(|d| d.join("history.jsonl")) else {
        return Vec::new();
    };
    load_history_from(&path, cwd)
}

/// The newest lines of `path`, other directories' entries first and this one's last. A line
/// that is not valid UTF-8 or JSON (a write cut off by a kill or a full disk) is skipped, and
/// only that line.
fn load_history_from(path: &std::path::Path, cwd: &str) -> Vec<String> {
    let Some(bytes) = read_tail(path, HISTORY_READ) else {
        return Vec::new();
    };
    if fs::metadata(path).is_ok_and(|m| m.len() > HISTORY_MAX) {
        trim_history(path, &bytes);
    }
    let lines: Vec<&[u8]> = bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(HISTORY_LINES);
    let mut other = Vec::new();
    let mut here = Vec::new();
    for l in &lines[start..] {
        let l = String::from_utf8_lossy(l);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) else {
            continue;
        };
        let Some(t) = v.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        if v.get("cwd").and_then(|c| c.as_str()) == Some(cwd) {
            here.push(t.to_string());
        } else {
            other.push(t.to_string());
        }
    }
    other.extend(here);
    other
}

/// The last `max` bytes of the file, starting on a line boundary.
fn read_tail(path: &std::path::Path, max: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(max);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::with_capacity((len - from) as usize);
    f.read_to_end(&mut buf).ok()?;
    if from > 0 {
        // Landed mid-line: drop the partial first one.
        match buf.iter().position(|b| *b == b'\n') {
            Some(i) => buf.drain(..=i),
            None => buf.drain(..),
        };
    }
    Some(buf)
}

/// Rewrite the file as just its newest lines, through a temp file and a rename so a reader or
/// another session never sees half of it.
fn trim_history(path: &std::path::Path, tail: &[u8]) {
    let lines: Vec<&[u8]> = tail
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    let keep = &lines[lines.len().saturating_sub(HISTORY_LINES)..];
    let tmp = path.with_extension("jsonl.tmp");
    let mut out = Vec::new();
    for l in keep {
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    let written = private_file()
        .write(true)
        .truncate(true)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&out));
    if written.is_ok() && fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

pub fn append_history(cwd: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let Some(d) = dir() else { return };
    make_dir(&d);
    let path = d.join("history.jsonl");
    let Ok(mut f) = private_file().append(true).open(&path) else {
        return;
    };
    tighten(&f);
    let text = cap_entry(text);
    // One write, so concurrent sessions cannot interleave halves of two lines.
    let line = format!("{}\n", serde_json::json!({"cwd": cwd, "text": text}));
    let _ = f.write_all(line.as_bytes());
    // A long session grows the file past what a new one would tolerate: cut it back now, not
    // only when the next session starts.
    if f.metadata().is_ok_and(|m| m.len() > HISTORY_MAX) {
        if let Some(tail) = read_tail(&path, HISTORY_READ) {
            trim_history(&path, &tail);
        }
    }
}

fn cap_entry(text: &str) -> &str {
    if text.len() <= ENTRY_MAX {
        return text;
    }
    let mut end = ENTRY_MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn draft_path(session: &str) -> Option<PathBuf> {
    if session.is_empty() || session.contains('/') {
        return None;
    }
    dir().map(|d| d.join("drafts").join(session))
}

pub fn save_draft(session: &str, text: &str) {
    let Some(p) = draft_path(session) else { return };
    if text.trim().is_empty() {
        let _ = fs::remove_file(p);
        return;
    }
    if let Some(parent) = p.parent() {
        make_dir(parent);
    }
    if let Ok(mut f) = private_file().write(true).truncate(true).open(&p) {
        tighten(&f);
        let _ = f.write_all(text.as_bytes());
    }
}

/// Read and delete the draft saved for `session`.
pub fn take_draft(session: &str) -> Option<String> {
    let p = draft_path(session)?;
    let t = fs::read_to_string(&p).ok()?;
    let _ = fs::remove_file(p);
    (!t.trim().is_empty()).then_some(t)
}

const RECENT_COMMANDS: usize = 40;

/// Slash commands you ran, oldest first, so the popup can put the ones you use on top.
pub fn load_recent_commands() -> Vec<String> {
    let Some(p) = dir().map(|d| d.join("commands.recent")) else {
        return Vec::new();
    };
    fs::read_to_string(p)
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn note_command(name: &str) {
    let Some(d) = dir() else { return };
    let mut all = load_recent_commands();
    all.retain(|n| n != name);
    all.push(name.to_string());
    let skip = all.len().saturating_sub(RECENT_COMMANDS);
    make_dir(&d);
    let p = d.join("commands.recent");
    if let Ok(mut f) = private_file().write(true).truncate(true).open(&p) {
        tighten(&f);
        let _ = f.write_all((all[skip..].join("\n") + "\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("openc-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.join("history.jsonl")
    }

    fn line(cwd: &str, text: &str) -> Vec<u8> {
        format!("{}\n", serde_json::json!({"cwd": cwd, "text": text})).into_bytes()
    }

    #[test]
    fn one_cut_off_multibyte_line_costs_one_line_not_the_whole_history() {
        let p = tmpfile("utf8");
        let mut f = Vec::new();
        f.extend(line("/a", "first"));
        f.extend(line("/a", "second"));
        // A write killed inside a three-byte character, then a later session appends on.
        let mut cut = line("/a", "日本語");
        cut.truncate(cut.len() - 6);
        f.extend(cut);
        f.push(b'\n');
        f.extend(line("/a", "after"));
        fs::write(&p, f).unwrap();
        assert_eq!(load_history_from(&p, "/a"), ["first", "second", "after"]);
    }

    #[test]
    fn an_oversized_file_is_read_from_its_tail_and_cut_back() {
        let p = tmpfile("big");
        let mut f = Vec::new();
        for i in 0..40_000 {
            f.extend(line("/a", &format!("prompt {i} {}", "x".repeat(500))));
        }
        assert!(f.len() as u64 > HISTORY_MAX);
        fs::write(&p, f).unwrap();
        let got = load_history_from(&p, "/a");
        assert_eq!(got.len(), HISTORY_LINES);
        assert!(got.last().unwrap().starts_with("prompt 39999 "));
        let after = fs::metadata(&p).unwrap().len();
        assert!(
            after < HISTORY_MAX / 4,
            "trimmed to the newest lines: {after}"
        );
        assert_eq!(load_history_from(&p, "/a").len(), HISTORY_LINES);
    }

    #[test]
    fn this_directory_comes_last_and_one_entry_is_capped() {
        let p = tmpfile("order");
        let mut f = Vec::new();
        f.extend(line("/here", "mine"));
        f.extend(line("/other", "theirs"));
        fs::write(&p, f).unwrap();
        assert_eq!(load_history_from(&p, "/here"), ["theirs", "mine"]);
        let big = "é".repeat(ENTRY_MAX);
        let c = cap_entry(&big);
        assert!(c.len() <= ENTRY_MAX && big.starts_with(c));
    }
}
