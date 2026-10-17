//! What the resume dialog needs from the transcripts on disk: one row per session with its
//! title, first prompt, message count and branch, across one directory or all of them, plus a
//! preview, rename and delete. Everything is read from `<config>/projects/<slug>/<id>.jsonl`.

use crate::history::{self, classify, one_line, text_of_user, UserText};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionDetail {
    pub id: String,
    /// Custom title, else the model's title, else the first prompt.
    pub title: String,
    pub first_prompt: String,
    /// Working directory recorded in the transcript. The slug on disk is lossy, so this is
    /// the real path when the file has one.
    pub cwd: String,
    pub branch: String,
    /// Unix seconds of the last write.
    pub updated: i64,
    pub bytes: u64,
    /// Prompts plus assistant text replies. Counted by substring, so it can be a few off, and
    /// extrapolated from both ends of a large file (`approx`).
    pub messages: usize,
    pub approx: bool,
    pub path: PathBuf,
}

/// Newest sessions first. `only_cwd` limits the listing to one directory; `None` walks every
/// project. At most `limit` files are read, newest by modification time.
pub fn list_details(root: &Path, only_cwd: Option<&Path>, limit: usize) -> Vec<SessionDetail> {
    let mut out = Vec::new();
    list_details_with(root, only_cwd, limit, &mut |mut chunk| {
        out.append(&mut chunk)
    });
    out
}

/// Like [`list_details`], but hands rows over in chunks of 16 as they are read, so a dialog
/// can fill in while the rest of a large history is still being scanned.
pub fn list_details_with(
    root: &Path,
    only_cwd: Option<&Path>,
    limit: usize,
    emit: &mut dyn FnMut(Vec<SessionDetail>),
) {
    let dirs: Vec<PathBuf> = match only_cwd {
        Some(c) => history::project_dirs_in(root, c),
        None => fs::read_dir(root)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut files: Vec<(i64, u64, PathBuf)> = Vec::new();
    for d in dirs {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            let t = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            files.push((t, md.len(), p));
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut chunk = Vec::new();
    let mut n = 0;
    for (t, len, p) in files {
        if n >= limit {
            break;
        }
        if let Some(d) = detail(&p, t, len) {
            chunk.push(d);
            n += 1;
            if chunk.len() == 16 {
                emit(std::mem::take(&mut chunk));
            }
        }
    }
    if !chunk.is_empty() {
        emit(chunk);
    }
}

/// Files up to this size are read whole; bigger ones from both ends only.
const WHOLE: u64 = 2 * 1024 * 1024;
const END: u64 = 384 * 1024;

#[derive(Default)]
struct Acc {
    cwd: String,
    branch: String,
    first_prompt: String,
    custom: Option<String>,
    ai: Option<String>,
    messages: usize,
}

impl Acc {
    fn line(&mut self, line: &str) {
        let side = line.contains("\"isSidechain\":true");
        // Cheap checks before paying for a JSON parse of a multi-megabyte tool result.
        let is_user = line.contains("\"type\":\"user\"");
        let is_asst = line.contains("\"type\":\"assistant\"");
        if is_user
            && !side
            && !line.contains("\"isMeta\":true")
            && !line.contains("\"tool_result\"")
        {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                return;
            };
            if self.cwd.is_empty() {
                if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                    self.cwd = c.to_string();
                }
            }
            if let Some(b) = v.get("gitBranch").and_then(Value::as_str) {
                self.branch = b.to_string();
            }
            if let Some(UserText::Prompt(p)) = text_of_user(&v).map(|t| classify(&t)) {
                self.messages += 1;
                if self.first_prompt.is_empty() {
                    self.first_prompt = one_line(&p, 200);
                }
            }
        } else if is_asst && !side && line.contains("\"type\":\"text\"") {
            self.messages += 1;
        } else if line.contains("\"type\":\"custom-title\"") {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                self.custom = v
                    .get("customTitle")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(self.custom.take());
            }
        } else if line.contains("\"type\":\"ai-title\"") {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                self.ai = v
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(self.ai.take());
            }
        }
    }
}

fn read_end(f: &mut File, from: u64, len: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut buf = vec![0u8; len];
    if f.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Read one transcript. `None` when it holds no conversation.
pub fn detail(path: &Path, updated: i64, bytes: u64) -> Option<SessionDetail> {
    let id = path.file_stem()?.to_str()?.to_string();
    let mut f = File::open(path).ok()?;
    let mut acc = Acc::default();
    let mut approx = false;
    if bytes <= WHOLE {
        let mut r = BufReader::new(f);
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => acc.line(&line),
            }
        }
    } else {
        // A long session: both ends say what it is about, and the middle is assumed to look
        // like them for the count.
        approx = true;
        let head = read_end(&mut f, 0, END as usize);
        let tail = read_end(&mut f, bytes - END, END as usize);
        let mut seen = 0usize;
        for l in head.lines().take(head.lines().count().saturating_sub(1)) {
            acc.line(l);
        }
        let after_head = acc.messages;
        seen += head.len();
        for l in tail.lines().skip(1) {
            acc.line(l);
        }
        seen += tail.len();
        let sampled = acc.messages.max(after_head);
        acc.messages = (sampled as u64 * bytes / seen.max(1) as u64) as usize;
    }
    if acc.messages == 0 {
        return None;
    }
    let title = acc
        .custom
        .or(acc.ai)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| acc.first_prompt.clone());
    let title = one_line(&title, 100);
    Some(SessionDetail {
        id,
        title: if title.is_empty() {
            "(untitled)".into()
        } else {
            title
        },
        first_prompt: acc.first_prompt,
        cwd: acc.cwd,
        branch: acc.branch,
        updated,
        bytes,
        messages: acc.messages,
        approx,
        path: path.to_path_buf(),
    })
}

/// The first prompt and the last `tail` messages of a transcript, each cut to `max` characters.
/// `(is_user, text)`; a gap between the first and the rest is the caller's to draw.
pub fn preview(path: &Path, tail: usize, max: usize) -> Vec<(bool, String)> {
    let Ok(f) = File::open(path) else {
        return Vec::new();
    };
    let mut first: Option<(bool, String)> = None;
    let mut last: std::collections::VecDeque<(bool, String)> = std::collections::VecDeque::new();
    let mut n = 0usize;
    let mut r = BufReader::new(f);
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let is_user = line.contains("\"type\":\"user\"");
        let is_asst = line.contains("\"type\":\"assistant\"");
        if !(is_user || is_asst)
            || line.contains("\"isSidechain\":true")
            || line.contains("\"isMeta\":true")
            || (is_user && line.contains("\"tool_result\""))
        {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let msg = if is_user {
            match text_of_user(&v).map(|t| classify(&t)) {
                Some(UserText::Prompt(p)) => Some((true, p)),
                _ => None,
            }
        } else {
            let t = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .map(|bs| {
                    bs.iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|t| !t.trim().is_empty());
            t.map(|t| (false, t))
        };
        let Some((u, t)) = msg else { continue };
        let item = (u, one_line(&t, max));
        n += 1;
        if first.is_none() {
            first = Some(item.clone());
        }
        last.push_back(item);
        while last.len() > tail {
            last.pop_front();
        }
    }
    let mut out = Vec::new();
    // The first message goes on its own only when the tail does not already hold it.
    if let Some(f) = first.filter(|_| n > tail) {
        out.push(f);
    }
    out.extend(last);
    out
}

/// Set the session's title the way Claude Code does: a `custom-title` line at the end.
pub fn rename(path: &Path, id: &str, title: &str) -> std::io::Result<()> {
    let title = title.trim();
    let line = serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": id});
    let mut f = fs::OpenOptions::new().append(true).open(path)?;
    // A transcript that ends mid-line (a killed `claude`) must not swallow the new line.
    f.write_all(format!("\n{line}\n").as_bytes())
}

pub fn delete(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, id: &str, lines: &[&str]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join(format!("{id}.jsonl"));
        fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    const U1: &str = r#"{"type":"user","cwd":"/w/a","gitBranch":"main","message":{"role":"user","content":"fix the parser"}}"#;
    const A1: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done, it was a typo."}]}}"#;
    const TR: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}}"#;
    const U2: &str =
        r#"{"type":"user","gitBranch":"fix","message":{"role":"user","content":"thanks"}}"#;

    #[test]
    fn counts_real_messages_and_reads_cwd_branch_and_first_prompt() {
        let t = std::env::temp_dir().join(format!("openc-sess-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        write(&t.join("-w-a"), "s1", &[U1, A1, TR, U2]);
        write(&t.join("-w-b"), "s2", &[r#"{"type":"summary"}"#]);
        let all = list_details(&t, None, 10);
        assert_eq!(all.len(), 1, "a file with no conversation is left out");
        let d = &all[0];
        assert_eq!(
            (d.messages, d.cwd.as_str(), d.branch.as_str()),
            (3, "/w/a", "fix")
        );
        assert_eq!(d.first_prompt, "fix the parser");
        assert_eq!(d.title, "fix the parser");
        let _ = fs::remove_dir_all(&t);
    }

    #[test]
    fn rename_wins_over_the_first_prompt_and_survives_a_torn_last_line() {
        let t = std::env::temp_dir().join(format!("openc-sess-r-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        let p = write(&t.join("-w-a"), "s1", &[U1, A1]);
        // Torn write: no trailing newline.
        let mut s = fs::read_to_string(&p).unwrap();
        s.pop();
        fs::write(&p, s).unwrap();
        rename(&p, "s1", "Parser typo").unwrap();
        let d = detail(&p, 0, 0).unwrap();
        assert_eq!(d.title, "Parser typo");
        // The older listing reads the same title.
        let l = history::list_sessions_in(&t, Path::new("/w/a"), 5);
        assert_eq!(l[0].title, "Parser typo");
        let _ = fs::remove_dir_all(&t);
    }

    #[test]
    fn preview_keeps_the_first_message_and_the_last_few() {
        let t = std::env::temp_dir().join(format!("openc-sess-p-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        let mut lines = vec![U1.to_string()];
        for i in 0..10 {
            lines.push(format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"reply {i}"}}]}}}}"#
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let p = write(&t.join("-w-a"), "s1", &refs);
        let pv = preview(&p, 3, 80);
        assert_eq!(pv.first().unwrap().1, "fix the parser");
        assert_eq!(pv.last().unwrap().1, "reply 9");
        assert_eq!(pv.len(), 4);
        let _ = fs::remove_dir_all(&t);
    }
}
