// OWNER: autocomplete (slash commands, `/model` arguments, `@` files, tab paths)
//! The popup under the editor's bottom rule: no frame, no background (spec 5.4). It pushes the
//! footer down. Rows are `→ label  description`; the selected row is all `accent`.

use std::path::Path;

use agent_core::ModelOption;
use ratatui::text::{Line, Span};
use tuikit::width::display_width;

use super::{span, Cx, Lines};
use crate::commands::SlashItem;
use crate::theme::Tok;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Slash,
    ModelArg,
    File,
    Path,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub desc: String,
    /// What replaces the token being completed.
    pub value: String,
    pub dir: bool,
}

/// What accepting the highlighted row does to the editor text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Apply {
    /// Byte range of the editor text to replace.
    pub range: (usize, usize),
    pub text: String,
    /// A directory was completed: the popup stays open for the next level.
    pub keep_open: bool,
    /// `enter` on a slash command or on a `/model` argument.
    pub kind: Kind,
}

impl Apply {
    /// Whether the range still fits `text`: inside it, on character boundaries.
    pub fn fits(&self, text: &str) -> bool {
        let (a, b) = self.range;
        a <= b && b <= text.len() && text.is_char_boundary(a) && text.is_char_boundary(b)
    }
}

#[derive(Default)]
pub struct Autocomplete {
    kind: Option<Kind>,
    items: Vec<Item>,
    sel: usize,
    range: (usize, usize),
    /// Editor text the user dismissed the popup at; it stays closed until the text changes.
    dismissed: Option<String>,
    pub max_visible: usize,
    no_match: bool,
    index: Option<FileIndex>,
}

/// The `@` list prepared once: every directory the files sit in, then the files. Rebuilding this at every key cost 120 to 170 ms in a repo of
/// 20,000 files (`Vec::contains` over every directory for every path component).
struct FileIndex {
    key: (usize, u64),
    all: Vec<String>,
    /// `all` in lowercase, so a key press allocates nothing per entry.
    lower: Vec<String>,
}

/// A cheap identity for a file list: its length and 64 names spread over it. The list is replaced
/// whole (`Msg::Files`), so a list of the same length with other names still gets a new index.
fn file_key(files: &[String]) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let step = (files.len() / 64).max(1);
    for f in files.iter().step_by(step) {
        f.hash(&mut h);
    }
    (files.len(), h.finish())
}

impl FileIndex {
    fn build(files: &[String]) -> FileIndex {
        let mut seen = std::collections::HashSet::new();
        let mut all: Vec<String> = Vec::new();
        for f in files {
            // every `/` ends a directory: `a/b/c.txt` brings `a/` and `a/b/`
            for (i, _) in f.match_indices('/') {
                let d = &f[..=i];
                if seen.insert(d) {
                    all.push(d.to_string());
                }
            }
        }
        all.extend(files.iter().cloned());
        let lower = all.iter().map(|p| p.to_lowercase()).collect();
        FileIndex {
            key: file_key(files),
            all,
            lower,
        }
    }
}

/// The looks wizard's `/ui` switches to.
const LOOKS: [&str; 7] = [
    "wizard", "codex", "grok", "opencode", "pi", "jcode", "claude",
];

pub struct AcCtx<'a> {
    pub commands: &'a [SlashItem],
    pub files: &'a [String],
    pub models: &'a [ModelOption],
    pub cwd: &'a Path,
}

fn fuzzy_rank<T>(items: Vec<T>, q: &str, key: impl Fn(&T) -> String) -> Vec<T> {
    if q.is_empty() {
        return items;
    }
    let ql = q.to_lowercase();
    // An exact name comes first: `/mode` must not run `/model`, which scores the same.
    let mut scored: Vec<(bool, i32, usize, T)> = items
        .into_iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let k = key(&it).to_lowercase();
            tuikit::fuzzy::score(&ql, &k).map(|m| (k == ql, m.score, i, it))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    scored.into_iter().map(|(_, _, _, it)| it).collect()
}

impl Autocomplete {
    pub fn new() -> Self {
        Autocomplete {
            max_visible: 5,
            ..Default::default()
        }
    }

    pub fn is_open(&self) -> bool {
        self.kind.is_some()
    }

    pub fn close(&mut self, text: &str) {
        self.kind = None;
        self.items.clear();
        self.dismissed = Some(text.to_string());
    }

    pub fn reset(&mut self) {
        self.kind = None;
        self.items.clear();
        self.dismissed = None;
    }

    pub fn up(&mut self) {
        let n = self.items.len();
        if n > 0 {
            self.sel = (self.sel + n - 1) % n;
        }
    }

    pub fn down(&mut self) {
        let n = self.items.len();
        if n > 0 {
            self.sel = (self.sel + 1) % n;
        }
    }

    pub fn kind(&self) -> Option<&Kind> {
        self.kind.as_ref()
    }

    pub fn has_items(&self) -> bool {
        !self.items.is_empty()
    }

    /// Recompute from the editor text and cursor. Call after every edit.
    pub fn update(&mut self, text: &str, cursor: usize, ctx: &AcCtx) {
        if self.dismissed.as_deref() == Some(text) {
            return;
        }
        self.dismissed = None;
        let cursor = floor_boundary(text, cursor);
        let before = &text[..cursor];
        // slash command: the whole text so far is `/word`, or `/model <arg>`
        if let Some(rest) = before.strip_prefix('/') {
            if !rest.contains(char::is_whitespace) {
                let all: Vec<Item> = ctx
                    .commands
                    .iter()
                    .map(|c| Item {
                        label: c.name.clone(),
                        desc: if c.hint.is_empty() {
                            c.description.clone()
                        } else {
                            format!("{} — {}", c.hint, c.description)
                        },
                        value: format!("/{} ", c.name),
                        dir: false,
                    })
                    .collect();
                let items = fuzzy_rank(all, rest, |i| i.label.clone());
                self.set(Kind::Slash, items, (0, cursor), false);
                return;
            }
            if let Some(arg) = rest.strip_prefix("model ") {
                let start = cursor - arg.len();
                let all: Vec<Item> = ctx
                    .models
                    .iter()
                    .map(|m| Item {
                        label: if m.name.is_empty() {
                            m.id.clone()
                        } else {
                            m.name.clone()
                        },
                        desc: m.provider.clone(),
                        value: m.id.clone(),
                        dir: false,
                    })
                    .collect();
                let items = fuzzy_rank(all, arg, |i| format!("{} {}", i.label, i.desc));
                self.set(Kind::ModelArg, items, (start, cursor), false);
                return;
            }
            // `/ui <look>`: an argument like a model's, applied first and sent on a second enter
            if let Some(arg) = rest.strip_prefix("ui ") {
                let start = cursor - arg.len();
                let all: Vec<Item> = LOOKS
                    .iter()
                    .map(|l| Item {
                        label: l.to_string(),
                        desc: String::new(),
                        value: l.to_string(),
                        dir: false,
                    })
                    .collect();
                let items = fuzzy_rank(all, arg, |i| i.label.clone());
                self.set(Kind::ModelArg, items, (start, cursor), false);
                return;
            }
        }
        // `@` at a token start
        if let Some((start, q)) = at_token(before) {
            let items = self.file_items(q, ctx.files);
            self.set(Kind::File, items, (start, cursor), false);
            return;
        }
        if self.kind == Some(Kind::Path) {
            // A path popup outlives the key that opened it, but its rows and byte range describe
            // the text as it was then. After any edit they are rebuilt from the live text, or the
            // popup goes (one candidate left is not a list).
            let rebuilt = path_candidates(text, cursor, ctx.cwd).filter(|c| c.names.len() > 1);
            match rebuilt {
                Some(c) => {
                    let items = c.items();
                    self.set(Kind::Path, items, (c.start, cursor), false);
                }
                None => {
                    self.kind = None;
                    self.items.clear();
                }
            }
            return;
        }
        self.kind = None;
        self.items.clear();
    }

    fn set(&mut self, kind: Kind, items: Vec<Item>, range: (usize, usize), show_empty: bool) {
        let same = self.kind.as_ref() == Some(&kind);
        self.no_match = items.is_empty() && show_empty;
        self.kind = if items.is_empty() && !show_empty {
            None
        } else {
            Some(kind)
        };
        if !same {
            self.sel = 0;
        }
        self.sel = self.sel.min(items.len().saturating_sub(1));
        self.items = items;
        self.range = range;
        if self.kind == Some(Kind::Slash) {
            self.range = (0, range.1);
        }
    }

    /// `tab` with no popup: complete a path token against the file system. One candidate is
    /// inserted, several open the popup. Returns the replacement when one was inserted.
    pub fn tab_path(&mut self, text: &str, cursor: usize, cwd: &Path) -> Option<Apply> {
        let cursor = floor_boundary(text, cursor);
        let c = path_candidates(text, cursor, cwd)?;
        match c.names.len() {
            0 => None,
            1 => {
                let (n, d) = &c.names[0];
                Some(Apply {
                    range: (c.start, cursor),
                    text: format!("{}{n}{}", c.dir_part, if *d { "/" } else { "" }),
                    keep_open: false,
                    kind: Kind::Path,
                })
            }
            _ => {
                let items = c.items();
                self.set(Kind::Path, items, (c.start, cursor), false);
                None
            }
        }
    }

    /// The row under the highlight, as an edit to the editor text.
    pub fn selected(&self) -> Option<Apply> {
        let it = self.items.get(self.sel)?;
        let kind = self.kind.clone()?;
        let text = match kind {
            // a name with a space goes out quoted (`@"my file.txt"`), or the model reads `@my`
            // and a stray word. A directory stays open for the next level, so its quote does too.
            Kind::File => {
                let quote = it.value.contains(char::is_whitespace);
                match (it.dir, quote) {
                    (true, false) => format!("@{}", it.value),
                    (true, true) => format!("@\"{}", it.value),
                    (false, false) => format!("@{} ", it.value),
                    (false, true) => format!("@\"{}\" ", it.value),
                }
            }
            _ => it.value.clone(),
        };
        Some(Apply {
            range: self.range,
            text,
            keep_open: it.dir,
            kind,
        })
    }

    pub fn render(&self, cx: &Cx) -> Lines {
        let Some(kind) = &self.kind else {
            return Vec::new();
        };
        let th = cx.th();
        let muted = th.fg(Tok::Muted);
        if self.items.is_empty() {
            return if self.no_match && *kind == Kind::Slash {
                vec![Line::from(span("  No matching commands", muted))]
            } else {
                Vec::new()
            };
        }
        let n = self.items.len();
        let vis = self.max_visible.max(1);
        let start = if n <= vis {
            0
        } else {
            self.sel.saturating_sub(vis / 2).min(n - vis)
        };
        let w = cx.width as usize;
        let col = label_col(&self.items, self.kind.as_ref());
        let mut out: Lines = Vec::new();
        for (i, it) in self.items.iter().enumerate().skip(start).take(vis) {
            let selected = i == self.sel;
            let prefix = if selected { "→ " } else { "  " };
            let accent = th.fg(Tok::Accent);
            // pi-tui `SelectList.renderItem`
            let mut row: Option<Vec<Span<'static>>> = None;
            if !it.desc.is_empty() && w > 40 {
                let eff = col.min(w.saturating_sub(2 + 4)).max(1);
                let label = super::cut(&it.label, eff.saturating_sub(2).max(1));
                let lw = display_width(&label);
                let spacing = eff.saturating_sub(lw).max(1);
                let remaining = w as isize - (2 + lw + spacing) as isize - 2;
                if remaining > 10 {
                    let desc = super::cut(&it.desc, remaining as usize);
                    row = Some(if selected {
                        vec![span(
                            format!("{prefix}{label}{}{desc}", " ".repeat(spacing)),
                            accent,
                        )]
                    } else {
                        vec![
                            Span::raw(format!("{prefix}{label}")),
                            span(format!("{}{desc}", " ".repeat(spacing)), muted),
                        ]
                    });
                }
            }
            let spans = row.unwrap_or_else(|| {
                let label = super::cut(&it.label, w.saturating_sub(4).max(1));
                if selected {
                    vec![span(format!("{prefix}{label}"), accent)]
                } else {
                    vec![Span::raw(format!("{prefix}{label}"))]
                }
            });
            out.push(Line::from(spans));
        }
        if n > vis {
            out.push(Line::from(span(
                format!("  ({}/{})", self.sel + 1, n),
                muted,
            )));
        }
        out
    }
}

/// Slash commands size the label column to the rows shown, `clamp(widest + 2, 12, 32)`; every
/// other list uses pi-tui's fixed 32.
fn label_col(items: &[Item], kind: Option<&Kind>) -> usize {
    if kind != Some(&Kind::Slash) {
        return 32;
    }
    let longest = items
        .iter()
        .map(|i| display_width(&i.label))
        .max()
        .unwrap_or(0);
    (longest + 2).clamp(12, 32)
}

/// The largest index at most `i` that is a character boundary of `s` (and at most its length).
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Directory entries that complete the path token ending at the cursor.
struct PathCandidates {
    /// Byte index where the token starts.
    start: usize,
    /// The token up to and including its last `/`.
    dir_part: String,
    names: Vec<(String, bool)>,
}

impl PathCandidates {
    fn items(&self) -> Vec<Item> {
        self.names
            .iter()
            .map(|(n, d)| {
                let slash = if *d { "/" } else { "" };
                Item {
                    label: format!("{n}{slash}"),
                    desc: String::new(),
                    value: format!("{}{n}{slash}", self.dir_part),
                    dir: *d,
                }
            })
            .collect()
    }
}

fn path_candidates(text: &str, cursor: usize, cwd: &Path) -> Option<PathCandidates> {
    let before = &text[..cursor];
    let start = before.rfind(char::is_whitespace).map_or(0, |i| {
        i + before[i..].chars().next().map_or(1, char::len_utf8)
    });
    let tok = &before[start..];
    let (dir_part, name_part) = match tok.rfind('/') {
        Some(i) => (&tok[..=i], &tok[i + 1..]),
        None => ("", tok),
    };
    if tok.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let base = if let Some(r) = dir_part.strip_prefix("~/") {
        Path::new(&home).join(r)
    } else if dir_part.starts_with('/') {
        Path::new(dir_part).to_path_buf()
    } else {
        cwd.join(dir_part)
    };
    let mut names: Vec<(String, bool)> = std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            (n.starts_with(name_part) && (name_part.starts_with('.') || !n.starts_with('.')))
                .then(|| (n, e.path().is_dir()))
        })
        .collect();
    names.sort();
    Some(PathCandidates {
        start,
        dir_part: dir_part.to_string(),
        names,
    })
}

/// The `@query` token ending at the cursor: byte index of the `@` and the query after it. A name
/// with spaces is typed as `@"my fi`; the token runs to the cursor while the quote is open.
fn at_token(before: &str) -> Option<(usize, &str)> {
    let starts_token = |at: usize| {
        at == 0
            || before[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '<' | '`'))
    };
    if let Some(at) = before.rfind("@\"") {
        let q = &before[at + 2..];
        if !q.contains('"') && starts_token(at) {
            return Some((at, q));
        }
    }
    let at = before.rfind('@')?;
    let q = &before[at + 1..];
    if q.contains(char::is_whitespace) {
        return None;
    }
    starts_token(at).then_some((at, q.trim_start_matches('"')))
}

impl Autocomplete {
    /// Files and directories below the cwd for an `@` query, the way Pi lists them.
    fn file_items(&mut self, q: &str, files: &[String]) -> Vec<Item> {
        let key = file_key(files);
        if self.index.as_ref().is_none_or(|i| i.key != key) {
            self.index = Some(FileIndex::build(files));
        }
        let ix = self.index.as_ref().expect("just built");
        pi_file_items(q, &ix.all, &ix.lower)
            .iter()
            .map(|p| {
                let dir = p.ends_with('/');
                let bare = p.trim_end_matches('/');
                let base = bare.rsplit('/').next().unwrap_or(bare);
                Item {
                    desc: bare.to_string(),
                    label: if dir {
                        format!("{base}/")
                    } else {
                        base.to_string()
                    },
                    value: p.clone(),
                    dir,
                }
            })
            .collect()
    }
}

/// ICU's root collation for the characters paths are made of: punctuation, then digits, then
/// letters without regard to case, and a lowercase letter before the same uppercase one. This is
/// what `localeCompare` does in Pi's final tie-break.
fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    const PUNCT: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    let primary = |c: char| -> (u8, u32) {
        let l = c.to_lowercase().next().unwrap_or(c);
        match PUNCT.find(l) {
            Some(i) => (0, i as u32),
            None if l.is_ascii_digit() => (1, l as u32),
            None => (2, l as u32),
        }
    };
    a.chars()
        .map(primary)
        .cmp(b.chars().map(primary))
        .then_with(|| {
            // tertiary: lowercase first
            a.chars()
                .zip(b.chars())
                .find(|(x, y)| x != y)
                .map_or(std::cmp::Ordering::Equal, |(x, y)| {
                    y.is_lowercase().cmp(&x.is_lowercase())
                })
        })
}

/// `scoreEntry` of pi-tui's autocomplete: the filename against the query, then the whole path.
/// `path_l` is the path in lowercase and `query_l` the query in lowercase.
fn score_entry(path_l: &str, query_l: &str, is_dir: bool) -> u32 {
    let name = path_l
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path_l);
    let mut score = if name == query_l {
        100
    } else if name.starts_with(query_l) {
        80
    } else if name.contains(query_l) {
        50
    } else if path_l.contains(query_l) {
        30
    } else {
        0
    };
    if is_dir && score > 0 {
        score += 10;
    }
    score
}

/// Pi's `getFuzzyFileSuggestions` over a file list (`all` holds every directory with a trailing
/// `/` and every file, relative to the cwd). Pi asks `fd`, which matches the pattern as a regex
/// against the file name (against the whole path when the query has a `/`, as `a[\\/]b`) with
/// smart case, then keeps what `scoreEntry` scores above zero, sorts by score, depth, length and
/// `localeCompare`, and shows the first 20. A query like `src/ma` is scoped: `src/` is the
/// directory to look in when it is one, and `ma` the query.
fn pi_file_items(q: &str, all: &[String], lower: &[String]) -> Vec<String> {
    use std::cmp::Ordering;
    let norm = q.replace('\\', "/");
    let mut display_base = String::new();
    let mut scope = String::new();
    let mut query = norm.clone();
    if let Some(i) = norm.rfind('/') {
        let b = &norm[..=i];
        if b.starts_with('/') || b.starts_with("~/") {
            // another tree than the cwd's: the list has nothing to offer
            return Vec::new();
        }
        let dir = b.trim_start_matches("./");
        if dir.is_empty() || all.iter().any(|p| p == dir) {
            display_base = b.to_string();
            scope = dir.to_string();
            query = norm[i + 1..].to_string();
        }
    }
    // an entry as fd reports it from the directory it was asked about, with its lowercase form
    let scope_l = scope.to_lowercase();
    let entries = all.iter().zip(lower).filter_map(|(p, pl)| {
        let rel = p.strip_prefix(scope.as_str())?;
        let rel_l = pl.strip_prefix(scope_l.as_str())?;
        (!rel.is_empty()).then_some((rel, rel_l))
    });
    let full_path = query.contains('/');
    let pattern = if full_path {
        let trimmed = query.trim_matches('/');
        if trimmed.is_empty() {
            query.clone()
        } else {
            let mut p = trimmed
                .split('/')
                .filter(|s| !s.is_empty())
                .map(regex::escape)
                .collect::<Vec<_>>()
                .join("[\\\\/]");
            if query.ends_with('/') {
                p.push_str("[\\\\/]");
            }
            p
        }
    } else {
        query.clone()
    };
    let smart_case_insensitive = !query.chars().any(char::is_uppercase);
    let query_l = query.to_lowercase();
    // a query with no regex syntax is a plain substring test; the regex is for the rest
    let literal = !pattern.contains([
        '\\', '.', '+', '*', '?', '(', ')', '|', '[', ']', '{', '}', '^', '$',
    ]);
    let re = if query.is_empty() || (literal && !full_path) {
        None
    } else {
        match regex::RegexBuilder::new(&pattern)
            .case_insensitive(smart_case_insensitive)
            .build()
        {
            Ok(r) => Some(r),
            // fd fails on a pattern that is not a regex, and Pi shows nothing
            Err(_) => return Vec::new(),
        }
    };
    struct Hit<'a> {
        rel: &'a str,
        score: u32,
        depth: usize,
        len: usize,
    }
    let name_of = |p: &str| {
        p.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string()
    };
    let mut hits: Vec<Hit> = entries
        .filter(|(rel, rel_l)| {
            if query.is_empty() {
                return true;
            }
            match &re {
                Some(r) if full_path => r.is_match(rel),
                Some(r) => r.is_match(&name_of(rel)),
                None if smart_case_insensitive => rel_l
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .contains(&query_l),
                None => rel
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .contains(&query),
            }
        })
        .map(|(rel, rel_l)| Hit {
            rel,
            score: if query.is_empty() {
                1
            } else {
                score_entry(rel_l, &query_l, rel.ends_with('/'))
            },
            depth: rel.split('/').filter(|s| !s.is_empty()).count(),
            len: rel.encode_utf16().count(),
        })
        .filter(|h| h.score > 0)
        .collect();
    let order = |a: &Hit, b: &Hit| {
        b.score
            .cmp(&a.score)
            .then(a.depth.cmp(&b.depth))
            .then(a.len.cmp(&b.len))
            .then_with(|| match locale_cmp(a.rel, b.rel) {
                Ordering::Equal => a.rel.cmp(b.rel),
                o => o,
            })
    };
    // only the first 20 are shown, so the rest need no order
    if hits.len() > 20 {
        hits.select_nth_unstable_by(19, order);
        hits.truncate(20);
    }
    hits.sort_by(order);
    hits.into_iter()
        .take(20)
        .map(|h| format!("{display_base}{}", h.rel))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finding 3: `@` files and directories in Pi's order and count, against Pi's own completion
    /// provider run with `fd` over the same tree (`tools/pi-at-golden.mjs`, 28 queries).
    #[test]
    fn at_listings_are_the_ones_pis_provider_gives() {
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/pi-at-golden.json")).unwrap();
        // the list wizard gives is files; directories come from their paths
        let files: Vec<String> = golden["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .filter(|p| !p.ends_with('/'))
            .collect();
        let mut ac = Autocomplete::new();
        for case in golden["cases"].as_array().unwrap() {
            let q = case["q"].as_str().unwrap();
            let got: Vec<(String, String, String)> = ac
                .file_items(q, &files)
                .into_iter()
                .map(|i| (i.label, i.desc, i.value))
                .collect();
            let want: Vec<(String, String, String)> = case["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| {
                    let value = i["value"].as_str().unwrap();
                    // Pi quotes a path with a space, which is applied when the item is taken
                    let value = value.trim_start_matches('@').trim_matches('"').to_string();
                    (
                        i["label"].as_str().unwrap().to_string(),
                        i["description"].as_str().unwrap().to_string(),
                        value,
                    )
                })
                .collect();
            assert_eq!(got, want, "@{q}");
        }
    }

    #[test]
    fn locale_compare_puts_punctuation_before_digits_before_letters_and_lowercase_first() {
        use std::cmp::Ordering::*;
        assert_eq!(locale_cmp("a b/", "src/"), Less);
        assert_eq!(locale_cmp("_x", "1"), Less);
        assert_eq!(locale_cmp("1", "a"), Less);
        assert_eq!(locale_cmp("a", "A"), Less);
        assert_eq!(locale_cmp("A", "b"), Less);
        assert_eq!(locale_cmp("Makefile", "main.go"), Greater);
    }

    fn cmds() -> Vec<SlashItem> {
        [
            ("settings", "", "Open settings menu"),
            (
                "model",
                "<provider/model>",
                "Select model (opens selector UI)",
            ),
        ]
        .iter()
        .map(|(n, h, d)| SlashItem {
            name: n.to_string(),
            hint: h.to_string(),
            description: d.to_string(),
            builtin: true,
        })
        .collect()
    }

    fn ctx<'a>(c: &'a [SlashItem], f: &'a [String], m: &'a [ModelOption]) -> AcCtx<'a> {
        AcCtx {
            commands: c,
            files: f,
            models: m,
            cwd: Path::new("/"),
        }
    }

    #[test]
    fn slash_opens_and_filters() {
        let c = cmds();
        let mut a = Autocomplete::new();
        a.update("/", 1, &ctx(&c, &[], &[]));
        assert!(a.is_open());
        assert_eq!(a.items.len(), 2);
        a.update("/mo", 3, &ctx(&c, &[], &[]));
        assert_eq!(a.items[0].label, "model");
        assert_eq!(a.selected().unwrap().text, "/model ");
        a.update("hello", 5, &ctx(&c, &[], &[]));
        assert!(!a.is_open());
    }

    #[test]
    fn an_exact_name_outranks_a_longer_one() {
        let mut c = cmds();
        c.push(SlashItem {
            name: "mode".into(),
            hint: String::new(),
            description: "pick a mode".into(),
            builtin: false,
        });
        let mut a = Autocomplete::new();
        a.update("/mode", 5, &ctx(&c, &[], &[]));
        assert_eq!(a.selected().unwrap().text, "/mode ");
    }

    #[test]
    fn ui_offers_the_looks() {
        let c = cmds();
        let mut a = Autocomplete::new();
        a.update("/ui ", 4, &ctx(&c, &[], &[]));
        assert_eq!(a.items.len(), 7);
        a.update("/ui op", 6, &ctx(&c, &[], &[]));
        assert_eq!(a.selected().unwrap().text, "opencode");
    }

    #[test]
    fn escape_keeps_it_closed_until_the_text_changes() {
        let c = cmds();
        let mut a = Autocomplete::new();
        a.update("/", 1, &ctx(&c, &[], &[]));
        a.close("/");
        a.update("/", 1, &ctx(&c, &[], &[]));
        assert!(!a.is_open());
        a.update("/s", 2, &ctx(&c, &[], &[]));
        assert!(a.is_open());
    }

    #[test]
    fn at_lists_top_level_then_applies() {
        let c = cmds();
        let files: Vec<String> = ["src/main.rs", "README.md", "src/lib.rs"]
            .map(String::from)
            .to_vec();
        let mut a = Autocomplete::new();
        a.update("look at @", 9, &ctx(&c, &files, &[]));
        assert_eq!(a.items[0].label, "src/");
        let ap = a.selected().unwrap();
        assert_eq!(ap.text, "@src/");
        assert!(ap.keep_open);
        assert_eq!(ap.range, (8, 9));
    }

    #[test]
    fn label_column_is_clamped() {
        let mk = |l: &str| Item {
            label: l.into(),
            desc: String::new(),
            value: String::new(),
            dir: false,
        };
        assert_eq!(label_col(&[mk("a")], Some(&Kind::Slash)), 12);
        assert_eq!(label_col(&[mk(&"x".repeat(40))], Some(&Kind::Slash)), 32);
        assert_eq!(label_col(&[mk("a")], Some(&Kind::File)), 32);
    }
}
