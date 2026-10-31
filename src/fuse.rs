//! The slash menu's ranking, the way Claude Code 2.1.314 does it: a Fuse.js 7 fuzzy search
//! (bitap, threshold 0.3, location 0, distance 100) over the command's name, display name, name
//! parts and aliases, then a stable sort on a seven part key, with commands whose descriptions
//! contain a word that starts with the query added at the end of the fuzzy matches.
//!
//! The bitap routine and the score arithmetic are Fuse's, so the order comes out the same for
//! every query. `tests/fixtures/slash-rank.json` holds what the real program showed for sixty
//! four queries and `tests/slash_rank.rs` holds this module to it.

use std::collections::HashMap;

const THRESHOLD: f64 = 0.3;
const DISTANCE: f64 = 100.0;
const MAX_BITS: usize = 32;
/// The bucket of a command found only through its description.
const DESCRIPTION_BUCKET: f64 = 10.0;

/// What the menu needs of a command.
pub struct Item<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub aliases: &'a [&'a str],
}

fn accuracy_and_distance(pattern_len: usize, errors: usize, current: i64, expected: i64) -> f64 {
    let accuracy = errors as f64 / pattern_len as f64;
    accuracy + (expected - current).abs() as f64 / DISTANCE
}

struct Chunk {
    pattern: Vec<char>,
    alphabet: HashMap<char, i32>,
    start: i64,
}

/// Fuse's `BitapSearch`, for a lower case pattern.
struct Bitap {
    pattern: Vec<char>,
    chunks: Vec<Chunk>,
}

impl Bitap {
    fn new(pattern: &str) -> Self {
        let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
        let mut chunks = Vec::new();
        let mut push = |p: &[char], start: usize| {
            let mut alphabet = HashMap::new();
            for (i, c) in p.iter().enumerate() {
                *alphabet.entry(*c).or_insert(0) |= 1i32 << (p.len() - i - 1);
            }
            chunks.push(Chunk {
                pattern: p.to_vec(),
                alphabet,
                start: start as i64,
            });
        };
        let m = pattern.len();
        if m > MAX_BITS {
            let rest = m % MAX_BITS;
            let mut at = 0;
            while at < m - rest {
                push(&pattern[at..at + MAX_BITS], at);
                at += MAX_BITS;
            }
            if rest > 0 {
                push(&pattern[m - MAX_BITS..], m - MAX_BITS);
            }
        } else if m > 0 {
            push(&pattern, 0);
        }
        Bitap { pattern, chunks }
    }

    /// `(is_match, score)` of the pattern in `text`.
    fn search_in(&self, text: &str) -> (bool, f64) {
        let text: Vec<char> = text.to_lowercase().chars().collect();
        if self.pattern == text {
            return (true, 0.0);
        }
        let mut total = 0.0;
        let mut any = false;
        for chunk in &self.chunks {
            let (hit, score) = bitap(&text, chunk);
            any |= hit;
            total += score;
        }
        (
            any,
            if any {
                total / self.chunks.len() as f64
            } else {
                1.0
            },
        )
    }
}

fn find_from(text: &[char], pattern: &[char], from: usize) -> Option<usize> {
    if pattern.is_empty() || from > text.len() || text.len() - from < pattern.len() {
        return None;
    }
    (from..=text.len() - pattern.len()).find(|&i| text[i..i + pattern.len()] == *pattern)
}

/// Fuse's `search` (the bitap core), with `location` 0 shifted by the chunk's start.
fn bitap(text: &[char], chunk: &Chunk) -> (bool, f64) {
    let pattern = &chunk.pattern;
    let m = pattern.len();
    let h = text.len() as i64;
    let f = chunk.start.clamp(0, h);
    let score_of = |errors: usize, current: i64| accuracy_and_distance(m, errors, current, f);
    let mut threshold = THRESHOLD;
    let mut best: i64 = f;
    while let Some(k) = find_from(text, pattern, best as usize) {
        threshold = threshold.min(score_of(0, k as i64));
        best = k as i64 + m as i64;
    }
    best = -1;
    let mut last: Vec<i32> = Vec::new();
    let mut score = 1.0;
    let mut upper = m as i64 + h;
    let goal = 1i32 << (m - 1);
    for w in 0..m {
        let mut lo = 0i64;
        let mut hi = upper;
        while lo < hi {
            if score_of(w, f + hi) <= threshold {
                lo = hi;
            } else {
                upper = hi;
            }
            hi = ((upper - lo) as f64 / 2.0 + lo as f64).floor() as i64;
        }
        upper = hi;
        let mut start = 1i64.max(f - hi + 1);
        let end = (f + hi).min(h) + m as i64;
        let mut row = vec![0i32; end as usize + 2];
        row[end as usize + 1] = (1i32.wrapping_shl(w as u32)).wrapping_sub(1);
        let prev = |i: i64| last.get(i as usize).copied().unwrap_or(0);
        let mut p = end;
        while p >= start {
            let q = p - 1;
            let mask = if q < h {
                chunk.alphabet.get(&text[q as usize]).copied().unwrap_or(0)
            } else {
                0
            };
            let mut v = ((row[p as usize + 1] << 1) | 1) & mask;
            if w > 0 {
                v |= ((prev(p + 1) | prev(p)) << 1) | 1 | prev(p + 1);
            }
            row[p as usize] = v;
            if v & goal != 0 {
                score = score_of(w, q);
                if score <= threshold {
                    threshold = score;
                    best = q;
                    if best <= f {
                        break;
                    }
                    start = 1i64.max(2 * f - best);
                }
            }
            p -= 1;
        }
        if score_of(w + 1, f) > threshold {
            break;
        }
        last = row;
    }
    (best >= 0, score.max(0.001))
}

/// Fuse's field length norm: one over the root of the number of words, to three places.
fn field_norm(value: &str) -> f64 {
    let words = value.split(' ').filter(|w| !w.is_empty()).count().max(1);
    let n = 1.0 / (words as f64).sqrt();
    (n * 1000.0 + 0.5).floor() / 1000.0
}

/// The values a command offers each searched key: (weight, values). Weights are Fuse's, before
/// they are divided by their sum.
fn keys(item: &Item) -> Vec<(f64, Vec<String>)> {
    let split = |s: &str| -> Vec<String> {
        s.split([':', '_', '-'])
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect()
    };
    let parts = split(item.name);
    let parts = if parts.len() > 1 { parts } else { Vec::new() };
    vec![
        (3.0, vec![item.name.to_string()]),
        (2.0, vec![item.name.to_string()]),
        (2.0, parts),
        (2.0, item.aliases.iter().map(|a| a.to_string()).collect()),
        (1.0, Vec::new()),
    ]
}

/// Fuse's score for each command the query matches, in Fuse's order (score, then position).
fn fuzzy(items: &[Item], query: &str) -> Vec<(usize, f64)> {
    let searcher = Bitap::new(query);
    let total: f64 = 3.0 + 2.0 + 2.0 + 2.0 + 1.0;
    let mut found = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let mut score = 1.0f64;
        let mut hit = false;
        for (weight, values) in keys(item) {
            let weight = weight / total;
            for value in values.iter().filter(|v| !v.trim().is_empty()) {
                let (matched, s) = searcher.search_in(value);
                if matched {
                    hit = true;
                    let base = if s == 0.0 { f64::EPSILON } else { s };
                    score *= base.powf(weight * field_norm(value));
                }
            }
        }
        if hit {
            found.push((idx, score));
        }
    }
    found.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    found
}

/// The words of a description a query can start: lower case, whitespace split, a word with
/// punctuation also whole without it and in its alphanumeric pieces. Sorted.
fn description_words(description: &str) -> Vec<String> {
    let mut words = std::collections::BTreeSet::new();
    for word in description.to_lowercase().split_whitespace() {
        if word
            .chars()
            .any(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit())
        {
            words.insert(
                word.chars()
                    .filter(char::is_ascii_alphanumeric)
                    .collect::<String>(),
            );
            for piece in word.split(|c: char| !c.is_ascii_alphanumeric()) {
                words.insert(piece.to_string());
            }
        } else {
            words.insert(word.to_string());
        }
    }
    words.remove("");
    words.into_iter().collect()
}

/// Positions in `items` for what is typed after the slash, best first.
pub fn rank(items: &[Item], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    struct Row {
        idx: usize,
        bucket: f64,
        hits: usize,
    }
    let mut rows: Vec<Row> = fuzzy(items, &q)
        .into_iter()
        .map(|(idx, s)| Row {
            idx,
            bucket: (s * 10.0).floor(),
            hits: 0,
        })
        .collect();
    let plain: String = q.chars().filter(char::is_ascii_alphanumeric).collect();
    if !plain.is_empty() {
        let found: std::collections::HashSet<usize> = rows.iter().map(|r| r.idx).collect();
        for (idx, item) in items.iter().enumerate() {
            if found.contains(&idx) {
                continue;
            }
            let words = description_words(item.description);
            let hits = words
                .iter()
                .skip_while(|w| **w < plain)
                .take_while(|w| w.starts_with(&plain))
                .count();
            if hits > 0 {
                rows.push(Row {
                    idx,
                    bucket: DESCRIPTION_BUCKET,
                    hits,
                });
            }
        }
    }
    let key = |r: &Row| -> [f64; 6] {
        let item = &items[r.idx];
        let name = item.name.to_lowercase();
        let exact = name == q;
        let alias_exact = item.aliases.iter().any(|a| a.to_lowercase() == q);
        let starts = if name.starts_with(&q) {
            name.chars().count() as f64
        } else {
            f64::INFINITY
        };
        let alias = item
            .aliases
            .iter()
            .map(|a| a.to_lowercase())
            .find(|a| a.starts_with(&q))
            .map_or(f64::INFINITY, |a| a.chars().count() as f64);
        [
            if exact { 0.0 } else { 1.0 },
            if alias_exact { 0.0 } else { 1.0 },
            starts,
            alias,
            r.bucket,
            -(r.hits as f64),
        ]
    };
    let mut keyed: Vec<([f64; 6], usize)> = rows.iter().map(|r| (key(r), r.idx)).collect();
    keyed.sort_by(|a, b| {
        a.0.iter()
            .zip(&b.0)
            .map(|(x, y)| x.total_cmp(y))
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    keyed.into_iter().map(|(_, idx)| idx).collect()
}

/// The other names Claude Code answers to for a command it lists. A name that is itself a
/// command in the menu is not an alias (`quit` in Wizard's menu is its own entry).
pub fn aliases_of(name: &str) -> &'static [&'static str] {
    match name {
        "bug" => &["share"],
        "clear" => &["reset", "new"],
        "config" => &["settings"],
        "mobile" => &["ios", "android"],
        "rename" => &["name"],
        "resume" => &["continue"],
        "tasks" => &["bashes"],
        "usage" => &["cost", "stats"],
        "plugin" => &["plugins", "marketplace"],
        "rewind" => &["checkpoint", "undo"],
        "exit" => &["quit"],
        "doctor" => &["checkup"],
        "list-agents" => &["peers"],
        "background" => &["bg"],
        "code-review" => &["review"],
        "permissions" => &["allowed-tools"],
        "loop" => &["proactive"],
        _ => &[],
    }
}

/// The first alias of `item` that starts with the query: the menu writes it after the name.
pub fn matched_alias(item: &Item, query: &str) -> Option<String> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    item.aliases
        .iter()
        .copied()
        .find(|a| a.to_lowercase().starts_with(&q))
        .filter(|a| *a != item.name)
        .map(String::from)
}

/// Each ranked command's sort key and whether its place was settled by position alone (tied on
/// the key and on Fuse's own score, or found through its description). For the test that works
/// out which order the real program holds its commands in.
#[doc(hidden)]
pub fn rank_keys(items: &[Item], query: &str) -> Vec<(usize, Vec<f64>, bool)> {
    let order = rank(items, query);
    let q = query.trim().to_lowercase();
    let fuzz: HashMap<usize, f64> = fuzzy(items, &q).into_iter().collect();
    order
        .iter()
        .map(|&idx| {
            let item = &items[idx];
            let name = item.name.to_lowercase();
            let starts = if name.starts_with(&q) {
                name.len() as f64
            } else {
                f64::INFINITY
            };
            let bucket = fuzz
                .get(&idx)
                .map_or(DESCRIPTION_BUCKET, |s| (s * 10.0).floor());
            let words = description_words(item.description);
            let plain: String = q.chars().filter(char::is_ascii_alphanumeric).collect();
            let hits = if fuzz.contains_key(&idx) {
                0
            } else {
                words
                    .iter()
                    .skip_while(|w| **w < plain)
                    .take_while(|w| w.starts_with(&plain))
                    .count()
            };
            let score = fuzz.get(&idx).copied().unwrap_or(f64::NAN);
            (
                idx,
                vec![starts, bucket, -(hits as f64), score],
                !fuzz.contains_key(&idx),
            )
        })
        .collect()
}
