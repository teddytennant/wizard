//! The `@` file picker: finding the token under the cursor, the file index and the ranking
//! (spec 4.8). The ranking is Grok's: nucleo with path bonuses, a minimum score of `7 + 14 * len`,
//! ties by path length then path; an empty query lists the top level; a query ending in `/` keeps
//! only directories; `@!` brings in hidden and ignored files.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::Path;

use super::fuzz::Query;
use crate::app::App;

/// Entries the index holds at most.
const CAP: usize = 100_000;
/// Results the picker keeps (the real one asks its matcher for the same).
const TOP_K: usize = 1000;

/// The `@` token the cursor is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtCtx {
    /// Byte range of the whole token, `@` included.
    pub range: Range<usize>,
    /// Text after `@` up to the cursor, `!` included.
    pub query: String,
}

impl AtCtx {
    pub fn hidden(&self) -> bool {
        self.query.starts_with('!')
    }

    pub fn dir_mode(&self) -> bool {
        self.query.ends_with('/')
    }

    pub fn matcher_query(&self) -> &str {
        self.query.strip_prefix('!').unwrap_or(&self.query)
    }

    /// Byte range of the path part of the token, after `@` and an optional `!`.
    pub fn path_range(&self) -> Range<usize> {
        self.range.start + 1 + usize::from(self.hidden())..self.range.end
    }
}

/// The token under `cursor`: the rightmost `@` before it that does not follow a letter, digit or
/// underscore, running to the next space, comma or semicolon.
pub fn detect(text: &str, cursor: usize) -> Option<AtCtx> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let at = text[..cursor].rfind('@')?;
    if text[..at]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let end = text[at + 1..]
        .char_indices()
        .find(|(_, c)| c.is_whitespace() || matches!(c, ',' | ';'))
        .map_or(text.len(), |(i, _)| at + 1 + i);
    if cursor > end {
        return None;
    }
    Some(AtCtx {
        range: at..end,
        query: text[at + 1..cursor].to_string(),
    })
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub is_dir: bool,
    pub hits: Vec<u32>,
}

pub struct Found {
    pub items: Vec<Hit>,
    /// Entries the index holds, the denominator of the count.
    pub total: usize,
}

/// Matches for `ctx` against the index.
pub fn search(app: &App, ctx: &AtCtx) -> Found {
    let hidden = ctx.hidden();
    let files: &[String] = if hidden {
        app.inp
            .hidden_index
            .get_or_init(|| index_all(&app.opts.cwd))
    } else {
        &app.files
    };
    let entry = |f: &String| (f.trim_end_matches('/').to_string(), f.ends_with('/'));
    let mq = ctx.matcher_query();
    let dir_mode = ctx.dir_mode();
    let mq = if dir_mode { &mq[..mq.len() - 1] } else { mq };
    if mq.is_empty() {
        // top level, sorted by name
        let mut v: Vec<Hit> = files
            .iter()
            .map(entry)
            .filter(|(p, d)| !p.contains('/') && (!dir_mode || *d))
            .map(|(path, is_dir)| Hit {
                path,
                is_dir,
                hits: Vec::new(),
            })
            .collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        let total = v.len();
        return Found { items: v, total };
    }
    let q = Query::new(mq);
    let min = 7 + mq.chars().count() as u32 * 14;
    let mut scored: Vec<(u32, String, bool)> = files
        .iter()
        .map(entry)
        .filter(|(_, d)| !dir_mode || *d)
        .filter_map(|(p, d)| {
            let sc = q.score(&p, true)?;
            (sc >= min).then_some((sc, p, d))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.len().cmp(&b.1.len()))
            .then(a.1.cmp(&b.1))
    });
    scored.truncate(TOP_K);
    let items = scored
        .into_iter()
        .map(|(_, path, is_dir)| Hit {
            hits: q.indices(&path, true).unwrap_or_default(),
            path,
            is_dir,
        })
        .collect();
    Found {
        items,
        total: files.len(),
    }
}

// ---- the index ----------------------------------------------------------------------------

fn hidden_component(p: &str) -> bool {
    p.split('/').any(|c| c.starts_with('.'))
}

/// Files and directories under `cwd` as the picker lists them: what git tracks or would track,
/// no hidden names; directories end in `/`. Outside a repository it walks the tree and skips
/// `target` and `node_modules`.
pub fn index(cwd: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output();
    let mut files: Vec<String> = Vec::new();
    match out {
        Ok(o) if o.status.success() => {
            for p in o.stdout.split(|b| *b == 0) {
                let Ok(p) = std::str::from_utf8(p) else {
                    continue;
                };
                // deleted-but-tracked paths are listed too
                if !p.is_empty() && !hidden_component(p) && cwd.join(p).is_file() {
                    files.push(p.to_string());
                }
            }
        }
        _ => walk(cwd, "", 0, true, &mut files),
    }
    with_dirs(files)
}

/// Everything under `cwd` but `.git`, ignore rules off: the index behind `@!`.
pub fn index_all(cwd: &Path) -> Vec<String> {
    let mut files = Vec::new();
    walk(cwd, "", 0, false, &mut files);
    with_dirs(files)
}

fn walk(dir: &Path, rel: &str, depth: usize, skip_hidden: bool, out: &mut Vec<String>) {
    if depth > 12 || out.len() > CAP {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut ents: Vec<_> = rd.flatten().collect();
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let name = e.file_name().to_string_lossy().to_string();
        if name == ".git"
            || (skip_hidden
                && (name.starts_with('.') || matches!(name.as_str(), "target" | "node_modules")))
        {
            continue;
        }
        let p = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&e.path(), &p, depth + 1, skip_hidden, out),
            Ok(t) if t.is_file() => out.push(p),
            _ => {}
        }
    }
}

/// The files plus every directory above them, directories marked with a trailing `/`.
fn with_dirs(files: Vec<String>) -> Vec<String> {
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        let mut at = 0;
        while let Some(i) = f[at..].find('/') {
            dirs.insert(f[..at + i].to_string());
            at += i + 1;
        }
    }
    let mut v: Vec<String> = dirs.into_iter().map(|d| format!("{d}/")).collect();
    v.extend(files);
    v.truncate(CAP);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_rules() {
        let c = detect("look at @src/ma", 15).unwrap();
        assert_eq!(c.range, 8..15);
        assert_eq!(c.query, "src/ma");
        assert!(detect("mail me@example.com", 19).is_none());
        assert!(detect("@foo bar", 8).is_none());
        let c = detect("@!.env", 6).unwrap();
        assert!(c.hidden());
        assert_eq!(c.matcher_query(), ".env");
        assert!(detect("@src/", 5).unwrap().dir_mode());
    }

    #[test]
    fn dirs_are_derived() {
        let v = with_dirs(vec!["a/b/c.rs".into(), "a/d.rs".into(), "e.rs".into()]);
        assert_eq!(v, vec!["a/", "a/b/", "a/b/c.rs", "a/d.rs", "e.rs"]);
    }
}
