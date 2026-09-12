//! Code the model writes out in its reasoning, kept on disk instead of thrown
//! away.
//!
//! Reasoning does not come back. [`build_messages`](crate::llm) sends the
//! assistant's *text* and its tool calls; the thinking blocks that produced
//! them are dropped on the way out, which is what every provider that charges
//! for replayed reasoning wants and is fine for an argument the model made
//! once. It is not fine for a program. One benchmark trial wrote the same
//! 1,400-line interpreter out eleven times across half an hour of thinking and
//! never once called `write_file`: each step it reasoned its way to the file,
//! the file went nowhere, and the next step started again from the task.
//!
//! So a long fenced block that no tool call in the same step wrote is saved
//! here, and the model is told the path. It can move the file instead of
//! retyping it. Nothing else changes: a step with no long block in it writes
//! nothing, says nothing, and costs nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::llm::ToolCall;

/// Lines a fenced block needs before it is treated as a draft worth saving.
///
/// Thirty is above anything that is being shown rather than written: a
/// signature, a diff hunk, the three lines of a failing test. Below it the
/// model is explaining, and a file per explanation is litter.
const MIN_LINES: usize = 30;

/// Characters of a draft compared against a tool call's arguments to decide
/// whether that call already wrote it, whitespace removed.
const MATCH_CHARS: usize = 256;

/// Drafts named in one note before it stops listing them.
const MAX_LISTED: usize = 3;

/// One fenced block long enough to be worth a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// The fence's info string (`rust`, `js`), lowercased, when it had one.
    pub lang: Option<String>,
    /// The block's contents, without the fences.
    pub body: String,
    /// Lines in [`Self::body`].
    pub lines: usize,
}

impl Draft {
    /// File extension for the fence tag, `txt` when it names nothing known.
    fn extension(&self) -> &'static str {
        const KNOWN: [(&str, &str); 34] = [
            ("rust", "rs"),
            ("rs", "rs"),
            ("python", "py"),
            ("py", "py"),
            ("javascript", "js"),
            ("js", "js"),
            ("jsx", "jsx"),
            ("typescript", "ts"),
            ("ts", "ts"),
            ("tsx", "tsx"),
            ("c", "c"),
            ("h", "h"),
            ("cpp", "cpp"),
            ("c++", "cpp"),
            ("go", "go"),
            ("java", "java"),
            ("kotlin", "kt"),
            ("swift", "swift"),
            ("ruby", "rb"),
            ("rb", "rb"),
            ("php", "php"),
            ("lua", "lua"),
            ("sh", "sh"),
            ("bash", "sh"),
            ("zsh", "sh"),
            ("sql", "sql"),
            ("html", "html"),
            ("css", "css"),
            ("json", "json"),
            ("toml", "toml"),
            ("yaml", "yaml"),
            ("yml", "yaml"),
            ("zig", "zig"),
            ("haskell", "hs"),
        ];
        let Some(lang) = &self.lang else {
            return "txt";
        };
        KNOWN
            .iter()
            .find(|(tag, _)| *tag == lang)
            .map_or("txt", |(_, extension)| *extension)
    }

    /// What the note calls this block: `js` when the fence named a language,
    /// `code` when it did not.
    fn label(&self) -> &str {
        self.lang.as_deref().unwrap_or("code")
    }
}

/// A draft on disk.
#[derive(Debug, Clone)]
pub struct Saved {
    pub path: PathBuf,
    /// `false` when this exact draft was already on disk, which is how a model
    /// that writes the same block out twice is told about it once.
    pub fresh: bool,
}

/// Where one session's drafts land: `~/.wizard/drafts/<session>/`. Cheap to
/// share behind an `Arc`; the directory is created on the first save, so a
/// session that drafts nothing leaves nothing behind.
#[derive(Debug)]
pub struct DraftStore {
    dir: PathBuf,
}

impl DraftStore {
    /// The store for session `id`.
    pub fn open(session_id: &str) -> Result<Self> {
        Ok(Self::in_dir(Config::drafts_dir()?.join(session_id)))
    }

    /// A store rooted at `dir`, for callers that own their own root.
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Directory this store writes to (created lazily by [`Self::save`]).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write `draft` to disk, named after the hash of its own body so the same
    /// block drafted twice lands on the same file instead of accumulating
    /// copies.
    pub fn save(&self, draft: &Draft) -> Result<Saved> {
        let digest = Sha256::digest(draft.body.as_bytes());
        let name = format!("{:x}", digest);
        let path = self
            .dir
            .join(format!("{}.{}", &name[..12], draft.extension()));
        if path.exists() {
            return Ok(Saved { path, fresh: false });
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        std::fs::write(&path, &draft.body)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(Saved { path, fresh: true })
    }
}

/// Every long fenced block in `texts`, deduplicated.
///
/// `texts` is the step's reasoning followed by its reply, in that order, which
/// is the order the model produced them in. Split from [`unwritten`] because
/// the two run at different points in a step: the reasoning is scanned before
/// it is folded into the assistant message, and which calls the step made is
/// not finally known until after that.
pub fn blocks<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<Draft> {
    let mut drafts: Vec<Draft> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for text in texts {
        for draft in fenced_blocks(text) {
            if seen.insert(draft.body.clone()) {
                drafts.push(draft);
            }
        }
    }
    drafts
}

/// The drafts none of `calls` already put on disk.
pub fn unwritten(drafts: Vec<Draft>, calls: &[ToolCall]) -> Vec<Draft> {
    if drafts.is_empty() {
        return drafts;
    }
    let written = call_text(calls);
    drafts
        .into_iter()
        .filter(|draft| !already_written(&draft.body, &written))
        .collect()
}

/// [`blocks`] then [`unwritten`], which is what a caller with both in hand
/// wants.
pub fn extract<'a>(texts: impl IntoIterator<Item = &'a str>, calls: &[ToolCall]) -> Vec<Draft> {
    unwritten(blocks(texts), calls)
}

/// The note the model reads on its next step, or `None` when nothing new was
/// saved.
///
/// Only fresh saves are named. A block the model has already been told about
/// is on disk under the same path and the note that said so is still in the
/// history; repeating it every step would be the one thing this must not be,
/// which is a per-step tax.
pub fn note(saved: &[(Saved, Draft)]) -> Option<String> {
    let fresh: Vec<&(Saved, Draft)> = saved.iter().filter(|(save, _)| save.fresh).collect();
    let (first, rest) = fresh.split_first()?;
    let mut note = format!(
        "Saved from your reasoning, which is not carried into your next step: a {}-line \
         {} block is at {}. Move or copy that file into place instead of writing it out \
         again.",
        first.1.lines,
        first.1.label(),
        first.0.path.display()
    );
    for (save, draft) in rest.iter().take(MAX_LISTED - 1) {
        note.push_str(&format!(
            " A {}-line {} block is at {}.",
            draft.lines,
            draft.label(),
            save.path.display()
        ));
    }
    if rest.len() > MAX_LISTED - 1 {
        note.push_str(&format!(
            " {} more are in {}.",
            rest.len() - (MAX_LISTED - 1),
            first.0.path.parent().unwrap_or(&first.0.path).display()
        ));
    }
    Some(note)
}

/// Every fenced block in `text` of at least [`MIN_LINES`] lines.
///
/// Deliberately simple: a fence opens on a line of three or more backticks and
/// closes on the next line of at least as many, which is how every model emits
/// one. A block that never closes (the reply was cut off mid-draft) still
/// counts: a truncated program is worth more on disk than in nothing.
fn fenced_blocks(text: &str) -> Vec<Draft> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, Option<String>, Vec<&str>)> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let ticks = trimmed.chars().take_while(|c| *c == '`').count();
        match &mut open {
            Some((width, _, body)) => {
                if ticks >= *width && trimmed[ticks..].trim().is_empty() {
                    let (_, lang, body) = open.take().expect("a fence is open");
                    push_block(&mut blocks, lang, &body);
                } else {
                    body.push(line);
                }
            }
            None if ticks >= 3 => {
                let info = trimmed[ticks..].trim().to_lowercase();
                let lang = info
                    .split_whitespace()
                    .next()
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_string);
                open = Some((ticks, lang, Vec::new()));
            }
            None => {}
        }
    }
    if let Some((_, lang, body)) = open {
        push_block(&mut blocks, lang, &body);
    }
    blocks
}

/// Add one closed block to `blocks` when it is long enough to be a draft.
fn push_block(blocks: &mut Vec<Draft>, lang: Option<String>, body: &[&str]) {
    if body.len() < MIN_LINES {
        return;
    }
    let mut text = body.join("\n");
    text.push('\n');
    blocks.push(Draft {
        lang,
        body: text,
        lines: body.len(),
    });
}

/// Every string a call carries in its arguments, whitespace removed, which is
/// what a draft is compared against.
///
/// Whitespace goes because the model does not reproduce its own draft byte for
/// byte on the way into `write_file`: it reindents, it drops a blank line. What
/// it does not do is rewrite the code, so the non-whitespace characters match.
fn call_text(calls: &[ToolCall]) -> Vec<String> {
    let mut strings = Vec::new();
    for call in calls {
        collect_strings(&call.function.arguments, &mut strings);
    }
    strings
}

/// Push every string leaf of `value`, squeezed, onto `into`.
fn collect_strings(value: &Value, into: &mut Vec<String>) {
    match value {
        Value::String(text) => into.push(squeeze(text)),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, into)),
        Value::Object(map) => map.values().for_each(|item| collect_strings(item, into)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Whether one of `written` already carries this draft.
fn already_written(body: &str, written: &[String]) -> bool {
    let needle: String = squeeze(body).chars().take(MATCH_CHARS).collect();
    if needle.is_empty() {
        return true;
    }
    written.iter().any(|text| text.contains(&needle))
}

/// `text` with every whitespace character removed.
fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block(lang: &str, lines: usize) -> String {
        named_block(lang, lines, "x")
    }

    fn named_block(lang: &str, lines: usize, name: &str) -> String {
        let body: Vec<String> = (0..lines)
            .map(|n| format!("let {name}{n} = {n};"))
            .collect();
        format!("```{lang}\n{}\n```\n", body.join("\n"))
    }

    /// The case this exists for: a long program in the reasoning channel that
    /// no tool call wrote, saved with the extension its fence named.
    #[test]
    fn a_long_block_no_tool_wrote_becomes_a_draft() {
        let text = format!(
            "I'll write the interpreter.\n\n{}\nThat should do it.",
            block("javascript", 200)
        );
        let drafts = extract([text.as_str()], &[]);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].lines, 200);
        assert_eq!(drafts[0].extension(), "js");
        assert!(drafts[0].body.starts_with("let x0 = 0;"));

        let dir = tempfile::tempdir().expect("tempdir");
        let store = DraftStore::in_dir(dir.path());
        let saved = store.save(&drafts[0]).expect("saved");
        assert!(saved.fresh);
        assert_eq!(saved.path.extension().and_then(|e| e.to_str()), Some("js"));
        assert_eq!(
            std::fs::read_to_string(&saved.path).expect("read back"),
            drafts[0].body
        );

        // The same draft twice is the same file, and only the first is worth
        // telling the model about.
        let again = store.save(&drafts[0]).expect("saved");
        assert!(!again.fresh);
        assert_eq!(again.path, saved.path);
        assert!(note(&[(again, drafts[0].clone())]).is_none());
    }

    /// A short block is somebody explaining, not somebody writing a file.
    #[test]
    fn a_short_block_is_ignored() {
        let text = block("rust", MIN_LINES - 1);
        assert!(extract([text.as_str()], &[]).is_empty());
        let long = block("rust", MIN_LINES);
        assert_eq!(extract([long.as_str()], &[]).len(), 1);
    }

    /// The draft that went straight into `write_file` is already where it
    /// belongs. Saving a copy and telling the model about it would be noise on
    /// exactly the steps that went right.
    #[test]
    fn a_block_the_same_step_wrote_is_ignored() {
        let drafted = block("python", 80);
        let body = drafted
            .trim_start_matches("```python\n")
            .trim_end_matches("```\n")
            .to_string();
        let call = ToolCall::new(
            "write_file",
            json!({ "path": "/app/vm.py", "content": body }),
        );
        assert!(extract([drafted.as_str()], std::slice::from_ref(&call)).is_empty());

        // Reindented on the way into the call, which is what a model actually
        // does, and still the same program.
        let reindented = ToolCall::new(
            "write_file",
            json!({ "path": "/app/vm.py", "content": body.replace('\n', "\n    ") }),
        );
        assert!(extract([drafted.as_str()], &[reindented]).is_empty());

        // A different long block in the same step is still saved.
        let other = named_block("python", 90, "y");
        assert_eq!(extract([other.as_str()], &[call]).len(), 1);
    }

    /// An unfinished fence is the cutoff case: the reply stopped mid-program,
    /// which is the moment the draft is worth the most.
    #[test]
    fn an_unclosed_fence_still_counts() {
        let text = block("c", 60);
        let cut = &text[..text.len() - 5];
        let drafts = extract([cut], &[]);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].extension(), "c");
    }

    /// The note names the path and says why the file exists. Two drafts are
    /// one note, not two.
    #[test]
    fn the_note_names_every_fresh_draft() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DraftStore::in_dir(dir.path());
        let first = Draft {
            lang: Some("rust".to_string()),
            body: "fn main() {}\n".repeat(40),
            lines: 40,
        };
        let second = Draft {
            lang: None,
            body: "x\n".repeat(31),
            lines: 31,
        };
        let saved = vec![
            (store.save(&first).expect("saved"), first),
            (store.save(&second).expect("saved"), second),
        ];
        let note = note(&saved).expect("a note");
        assert!(note.contains("40-line rust block"), "{note}");
        assert!(note.contains("31-line code block"), "{note}");
        assert!(note.contains(".rs"), "{note}");
        assert!(note.contains(".txt"), "{note}");
        assert!(!note.contains('\n'), "one line: {note}");
    }
}
