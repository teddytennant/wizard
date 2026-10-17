//! File tools: `read`, `write`, `edit`, `apply_patch`.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use agent_core::{ToolCall, ToolStatus};
use ratatui::style::Style;
use ratatui::text::Span;
use serde_json::{Map, Value};
use tuikit::diff::{self, DiffOptions, DiffView};
use tuikit::syntax;
use tuikit::width::clip_spans;

use super::rows::{block, block_width, inline_block, BlockSpec, Rows};
use super::text::{fmt_path, input_args, num_in, str_in};
use super::{denied, inline};
use crate::ui::session::{Block, RenderCx};

const PATH_KEYS: [&str; 4] = ["filePath", "path", "file_path", "file"];

fn path_of(c: &ToolCall) -> String {
    str_in(&c.input, &PATH_KEYS)
        .map(String::from)
        .unwrap_or_else(|| c.title.clone())
}

/// opencode's argument list for a read: `[offset=2, limit=2]`. Wizard's `start_line` and
/// `end_line` are the same window spelled as a range.
fn read_args(input: &Value) -> String {
    let Some(map) = input.as_object() else {
        return String::new();
    };
    let start = num_in(input, &["offset", "start_line"]);
    let mut out = Map::new();
    for (k, v) in map {
        match k.as_str() {
            k if PATH_KEYS.contains(&k) => {}
            "start_line" => {
                out.insert("offset".into(), v.clone());
            }
            "end_line" => {
                let limit = v
                    .as_i64()
                    .map(|e| e - start.unwrap_or(1) + 1)
                    .filter(|l| *l > 0)
                    .map_or_else(|| v.clone(), Value::from);
                out.insert("limit".into(), limit);
            }
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    input_args(&Value::Object(out), &[])
}

/// Instruction files a read pulled in, which opencode lists as `↳ Loaded <path>`. They ride
/// in the output as `Instructions from: <path>` inside a system reminder.
fn loaded(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix("Instructions from: "))
        .map(|p| p.trim().to_string())
        .collect()
}

pub fn read(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let p = path_of(c);
    let args = read_args(&c.input);
    let text = format!("Read {} {}", fmt_path(&p, cx.cwd), args)
        .trim_end()
        .to_string();
    let mut i = inline(c, cx, "→", text, "Reading file…", !p.is_empty());
    i.spinner = c.status == ToolStatus::Running;
    if c.status == ToolStatus::Completed {
        i.extra = loaded(c.output.as_deref().unwrap_or(""))
            .iter()
            .map(|p| format!("↳ Loaded {}", fmt_path(p, cx.cwd)))
            .collect();
    }
    vec![inline_block(i, cx)]
}

// ---- write ---------------------------------------------------------------------------------

pub fn write(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let p = path_of(c);
    let code = str_in(&c.input, &["content"])
        .map(String::from)
        .or_else(|| c.diff.as_ref().map(|d| d.new.clone()));
    let inline_row = |c: &ToolCall| {
        let i = inline(
            c,
            cx,
            "←",
            format!("Write {}", fmt_path(&p, cx.cwd)),
            "Preparing write…",
            !p.is_empty(),
        );
        vec![inline_block(i, cx)]
    };
    let Some(code) = code.filter(|_| c.status == ToolStatus::Completed) else {
        return inline_row(c);
    };
    vec![block(
        BlockSpec {
            title: Some(format!("# Wrote {}", fmt_path(&p, cx.cwd))),
            spinner_title: false,
            children: vec![code_rows(&code, &p, cx)],
            error: None,
            click: None,
        },
        cx,
    )]
}

/// opencode's `<line_number minWidth=3 paddingRight=1>`: the gutter is `max(3, digits + 2)`
/// cells, the number right-aligned in all but the last one.
fn code_rows(code: &str, path: &str, cx: &RenderCx) -> Rows {
    let t = cx.theme;
    let mut hl = syntax::highlight(syntax::find_syntax_for_path(path), code, t);
    // opencode splits on `\n`, so content ending in a newline shows one more, empty, line
    if code.ends_with('\n') {
        hl.push(Vec::new());
    }
    let gutter = hl.len().to_string().len() + 2;
    let gutter = gutter.max(3);
    let w = block_width(cx).saturating_sub(gutter);
    hl.into_iter()
        .enumerate()
        .map(|(i, spans)| {
            let mut line = vec![Span::styled(
                format!("{:>width$} ", i + 1, width = gutter - 1),
                Style::new().fg(t.text_muted),
            )];
            line.extend(clip_spans(spans, w));
            line
        })
        .collect()
}

// ---- edit ----------------------------------------------------------------------------------

type Pair = (String, String);

thread_local! {
    /// Full-file diffs rebuilt from disk, keyed by call id and the edit itself. Calls repaint
    /// while a turn is open, and the file may have changed again by the next repaint.
    static EXPANDED: RefCell<HashMap<(String, u64), Option<Pair>>> = RefCell::new(HashMap::new());
}

/// Drop every rebuilt diff; called when the transcript is replaced.
pub fn reset_expanded() {
    EXPANDED.with(|m| m.borrow_mut().clear());
}

fn hash_of<T: Hash>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// Largest file the diff expansion will read. Past this the snippet diff is shown instead.
const EXPAND_MAX_BYTES: u64 = 1 << 20;

/// Read `path` for the diff expansion. The path comes from the model's tool input and this runs
/// while drawing, so it must never block or read without a bound: only a regular file that
/// resolves to somewhere under `cwd` and is at most [`EXPAND_MAX_BYTES`]. A FIFO would park the
/// UI thread in `open`, `/dev/zero` would read until memory ran out.
fn read_regular_under(path: &str, cwd: &str) -> Option<String> {
    tuikit::fsread::read_regular_under(path, cwd, EXPAND_MAX_BYTES)
}

/// Wizard's `edit_file` carries only the replaced snippet, so a diff of it alone has no
/// context and counts lines from 1. When the file is still on disk and holds the new text,
/// rebuild the file as it was and diff the two whole files, which is what opencode's diff
/// shows. The occurrence is the one on the line wizard reported (`(line N)`), or all of them
/// for `replace_all`.
fn expand_snippet(c: &ToolCall, cwd: &str) -> Option<Pair> {
    let d = c.diff.as_ref()?;
    let old = d.old.as_deref()?;
    let snippet_edit = c.input.get("old_string").is_some() && c.input.get("new_string").is_some();
    if !snippet_edit || d.new.is_empty() {
        return None;
    }
    let abs = if d.path.starts_with('/') {
        d.path.clone()
    } else {
        format!("{}/{}", cwd.trim_end_matches('/'), d.path)
    };
    let cur = read_regular_under(&abs, cwd)?;
    let at: Vec<usize> = cur.match_indices(d.new.as_str()).map(|(i, _)| i).collect();
    if at.is_empty() {
        return None;
    }
    let all = c.input.get("replace_all").and_then(Value::as_bool) == Some(true);
    let line = c
        .output
        .as_deref()
        .and_then(|o| o.rsplit_once("(line "))
        .and_then(|(_, r)| r.split(')').next())
        .and_then(|n| n.trim().parse::<usize>().ok());
    let line_of = |byte: usize| cur[..byte].matches('\n').count() + 1;
    let pick: Vec<usize> = if all {
        at
    } else {
        let want = line.and_then(|l| at.iter().copied().find(|b| line_of(*b) == l));
        vec![want.unwrap_or(at[0])]
    };
    let mut before = String::new();
    let mut last = 0;
    for b in pick {
        before.push_str(&cur[last..b]);
        before.push_str(old);
        last = b + d.new.len();
    }
    before.push_str(&cur[last..]);
    Some((before, cur))
}

/// `(old, new)` text of the call's diff, whole files when they can be had.
fn diff_texts(c: &ToolCall, cwd: &str) -> Option<Pair> {
    let d = c.diff.as_ref()?;
    let key = (c.id.clone(), hash_of(&(&d.path, &d.old, &d.new)));
    let hit = EXPANDED.with(|m| m.borrow().get(&key).cloned());
    let full = match hit {
        Some(v) => v,
        None => {
            let v = expand_snippet(c, cwd);
            EXPANDED.with(|m| {
                let mut m = m.borrow_mut();
                if m.len() > 256 {
                    m.clear();
                }
                m.insert(key, v.clone());
            });
            v
        }
    };
    full.or_else(|| Some((d.old.clone().unwrap_or_default(), d.new.clone())))
}

fn diff_rows(old: &str, new: &str, path: &str, cx: &RenderCx) -> Rows {
    let opts = DiffOptions {
        view: DiffView::auto(cx.width),
        context: 4,
        word_highlight: false,
        lang: path.rsplit_once('.').map(|(_, e)| e.to_string()),
        ..Default::default()
    };
    // The diff sits in a box with one column of padding.
    let w = block_width(cx).saturating_sub(1) as u16;
    diff::from_texts(old, new, w, cx.theme, &opts)
        .into_iter()
        .map(|l| {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(l.spans);
            spans
        })
        .collect()
}

pub fn edit(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let p = path_of(c);
    let replace_all = c
        .input
        .get("replaceAll")
        .or(c.input.get("replace_all"))
        .filter(|v| v.is_boolean() || v.is_number() || v.is_string());
    let texts = if c.status == ToolStatus::Completed {
        diff_texts(c, cx.cwd)
    } else {
        None
    };
    let Some((old, new)) = texts else {
        let mut args = Map::new();
        if let Some(v) = replace_all {
            args.insert("replaceAll".into(), v.clone());
        }
        let text = format!(
            "Edit {} {}",
            fmt_path(&p, cx.cwd),
            input_args(&Value::Object(args), &[])
        )
        .trim_end()
        .to_string();
        let i = inline(c, cx, "←", text, "Preparing edit…", !p.is_empty());
        return vec![inline_block(i, cx)];
    };
    vec![block(
        BlockSpec {
            title: Some(format!("← Edit {}", fmt_path(&p, cx.cwd))),
            spinner_title: false,
            children: vec![diff_rows(&old, &new, &p, cx)],
            error: None,
            click: None,
        },
        cx,
    )]
}

// ---- apply_patch ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Kind {
    Add,
    Delete,
    Update,
    Move,
}

#[derive(Debug)]
struct PatchFile {
    kind: Kind,
    path: String,
    to: Option<String>,
    old: String,
    new: String,
}

/// Parse the `*** Begin Patch` format apply_patch tools take.
fn parse_patch(text: &str) -> Vec<PatchFile> {
    let mut files: Vec<PatchFile> = Vec::new();
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("*** Add File: ") {
            files.push(PatchFile {
                kind: Kind::Add,
                path: p.trim().into(),
                to: None,
                old: String::new(),
                new: String::new(),
            });
        } else if let Some(p) = line.strip_prefix("*** Delete File: ") {
            files.push(PatchFile {
                kind: Kind::Delete,
                path: p.trim().into(),
                to: None,
                old: String::new(),
                new: String::new(),
            });
        } else if let Some(p) = line.strip_prefix("*** Update File: ") {
            files.push(PatchFile {
                kind: Kind::Update,
                path: p.trim().into(),
                to: None,
                old: String::new(),
                new: String::new(),
            });
        } else if let Some(p) = line.strip_prefix("*** Move to: ") {
            if let Some(f) = files.last_mut() {
                f.kind = Kind::Move;
                f.to = Some(p.trim().into());
            }
        } else if line.starts_with("*** ") || line.starts_with("@@") {
            continue;
        } else if let Some(f) = files.last_mut() {
            let (body, side) = match line.chars().next() {
                Some('+') => (&line[1..], 'n'),
                Some('-') => (&line[1..], 'o'),
                Some(' ') => (&line[1..], 'b'),
                _ => (line, 'b'),
            };
            if side != 'n' {
                f.old.push_str(body);
                f.old.push('\n');
            }
            if side != 'o' {
                f.new.push_str(body);
                f.new.push('\n');
            }
        }
    }
    files
}

fn patch_text(c: &ToolCall) -> Option<&str> {
    str_in(&c.input, &["patchText", "patch", "input"])
}

pub fn apply_patch(c: &ToolCall, cx: &RenderCx) -> Vec<Block> {
    let files = if c.status == ToolStatus::Completed {
        patch_text(c).map(parse_patch).unwrap_or_default()
    } else {
        Vec::new()
    };
    if files.is_empty() {
        // opencode passes `complete={false}`: the row stays a pending line until the block form
        // can be built, and `Patch failed` when it errors.
        let mut i = inline(c, cx, "%", "Patch".into(), "Preparing patch…", false);
        i.failure = Some("Patch failed");
        if denied(c) {
            i.phase = super::rows::Phase::Denied;
        }
        return vec![inline_block(i, cx)];
    }
    files
        .into_iter()
        .map(|f| {
            let rel = fmt_path(&f.path, cx.cwd);
            let (title, body) = match f.kind {
                Kind::Delete => (format!("# Deleted {rel}"), Vec::new()),
                Kind::Add => (
                    format!("# Created {rel}"),
                    vec![diff_rows("", &f.new, &f.path, cx)],
                ),
                Kind::Move => {
                    let to = fmt_path(f.to.as_deref().unwrap_or(""), cx.cwd);
                    (
                        format!("# Moved {rel} → {to}"),
                        vec![diff_rows(&f.old, &f.new, &f.path, cx)],
                    )
                }
                Kind::Update => (
                    format!("← Patched {rel}"),
                    vec![diff_rows(&f.old, &f.new, &f.path, cx)],
                ),
            };
            block(
                BlockSpec {
                    title: Some(title),
                    spinner_title: false,
                    children: body,
                    error: None,
                    click: None,
                },
                cx,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("openw-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mkfifo(p: &std::path::Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// Finding 5: a FIFO named by the model used to park the draw thread in `open` for good.
    #[test]
    fn a_fifo_is_not_read_while_drawing() {
        let d = scratch("fifo");
        let p = d.join("pipe");
        mkfifo(&p);
        let (tx, rx) = std::sync::mpsc::channel();
        let (path, cwd) = (
            p.to_string_lossy().into_owned(),
            d.to_string_lossy().into_owned(),
        );
        std::thread::spawn(move || {
            let _ = tx.send(read_regular_under(&path, &cwd));
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(got, Ok(None), "the read blocked or returned data");
    }

    #[test]
    fn only_small_regular_files_under_the_cwd_are_read() {
        let d = scratch("under");
        let cwd = d.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("a.txt"), "hi").unwrap();
        std::fs::write(d.join("outside.txt"), "no").unwrap();
        std::fs::write(
            cwd.join("big.txt"),
            vec![b'x'; (EXPAND_MAX_BYTES + 1) as usize],
        )
        .unwrap();
        std::os::unix::fs::symlink(d.join("outside.txt"), cwd.join("link.txt")).unwrap();
        let (c, p) = (cwd.to_string_lossy().into_owned(), |n: &str| {
            cwd.join(n).to_string_lossy().into_owned()
        });
        assert_eq!(read_regular_under(&p("a.txt"), &c).as_deref(), Some("hi"));
        assert_eq!(
            read_regular_under(&p("big.txt"), &c),
            None,
            "over the size cap"
        );
        assert_eq!(
            read_regular_under(&p("link.txt"), &c),
            None,
            "symlink out of the cwd"
        );
        assert_eq!(
            read_regular_under(&p("../outside.txt"), &c),
            None,
            "dot-dot out of the cwd"
        );
        assert_eq!(read_regular_under("/dev/zero", &c), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn read_window_is_spelled_like_opencode() {
        let v = serde_json::json!({"path": "a.rs", "start_line": 2, "end_line": 3});
        assert_eq!(read_args(&v), "[offset=2, limit=2]");
        let v = serde_json::json!({"path": "a.rs"});
        assert_eq!(read_args(&v), "");
        let v = serde_json::json!({"filePath": "a.rs", "offset": 5, "limit": 7});
        assert_eq!(read_args(&v), "[offset=5, limit=7]");
    }

    #[test]
    fn loaded_files_come_from_the_reminder() {
        let out = "<content>\n1: hi\n</content>\n\n<system-reminder>\nInstructions from: /p/sub/AGENTS.md\nBe brief.\n</system-reminder>";
        assert_eq!(loaded(out), vec!["/p/sub/AGENTS.md"]);
    }

    #[test]
    fn patch_text_parses_into_files() {
        let p = "*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n*** Update File: b.rs\n@@\n ctx\n-old\n+new\n*** Move to: c.rs\n*** Delete File: d.txt\n*** End Patch";
        let f = parse_patch(p);
        assert_eq!(f.len(), 3);
        assert_eq!(
            (f[0].kind == Kind::Add, f[0].new.as_str()),
            (true, "one\ntwo\n")
        );
        assert_eq!(f[1].kind, Kind::Move);
        assert_eq!(f[1].old, "ctx\nold\n");
        assert_eq!(f[1].new, "ctx\nnew\n");
        assert_eq!(f[2].kind, Kind::Delete);
    }
}
