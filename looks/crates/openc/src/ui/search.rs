// OWNER: transcript
//! Transcript search: matching rules and the result list. The transcript fills [`Search`] by
//! scanning the rendered rows, so a hit has a row and a column and highlights, counts and
//! jumps all agree on what was found.

use unicode_width::UnicodeWidthChar;

/// One match: block, row inside the block (blank lead rows counted, as in `Frame`), and the
/// display column where it starts. A match hidden in a folded block has `len == 0` and points
/// at the block's first row; jumping to it opens the block.
///
/// A `pending` hit stands for a block whose source holds the text but which was not laid out
/// when the search ran (laying out every block of a long transcript for every keystroke freezes
/// the screen). It points at the block's first row, counts as one, and is replaced by the real
/// hits, or dropped, when a jump reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hit {
    pub block: usize,
    pub row: usize,
    pub col: usize,
    pub len: usize,
    pub pending: bool,
}

#[derive(Default, Debug)]
pub struct Search {
    pub query: String,
    pub hits: Vec<Hit>,
    pub cur: usize,
    /// The hits were computed for another query, width, detail mode or transcript.
    pub(crate) stale: bool,
    pub(crate) width: usize,
    pub(crate) detail: bool,
    pub(crate) rev: u64,
}

impl Search {
    pub fn new(query: &str) -> Search {
        Search {
            query: query.to_string(),
            stale: true,
            ..Search::default()
        }
    }

    /// `(1-based position, total)`; position 0 when nothing matches.
    pub fn counts(&self) -> (usize, usize) {
        let n = self.hits.len();
        (if n == 0 { 0 } else { self.cur.min(n - 1) + 1 }, n)
    }

    /// Some hits are placeholders for blocks not laid out yet, so the total may still change.
    pub fn approximate(&self) -> bool {
        self.hits.iter().any(|h| h.pending)
    }

    pub fn current(&self) -> Option<Hit> {
        self.hits.get(self.cur).copied()
    }

    /// Characters to match, folded for the case rule.
    pub fn needle(&self) -> (Vec<char>, bool) {
        needle(&self.query)
    }
}

/// Smart case: a query with a capital letter matches exactly, anything else ignores case.
pub fn needle(q: &str) -> (Vec<char>, bool) {
    let sensitive = q.chars().any(char::is_uppercase);
    (q.chars().map(|c| fold(c, sensitive)).collect(), sensitive)
}

// One char to one char, so columns computed on the folded text are columns of the real text.
fn fold(c: char, sensitive: bool) -> char {
    if sensitive {
        c
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Non-overlapping matches of `q` in `text` as `(display column, display width)`.
pub fn occurrences(text: &str, q: &[char], sensitive: bool) -> Vec<(usize, usize)> {
    if q.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < q.len() {
        return Vec::new();
    }
    // Display column before each char.
    let mut cols = Vec::with_capacity(chars.len() + 1);
    let mut x = 0;
    for c in &chars {
        cols.push(x);
        x += if *c == '\t' {
            1
        } else {
            c.width().unwrap_or(0)
        };
    }
    cols.push(x);
    let mut out = Vec::new();
    let mut i = 0;
    while i + q.len() <= chars.len() {
        if chars[i..i + q.len()]
            .iter()
            .zip(q)
            .all(|(a, b)| fold(*a, sensitive) == *b)
        {
            out.push((cols[i], cols[i + q.len()] - cols[i]));
            i += q.len();
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn occ(text: &str, q: &str) -> Vec<(usize, usize)> {
        let (n, cs) = needle(q);
        occurrences(text, &n, cs)
    }

    #[test]
    fn smart_case() {
        assert_eq!(occ("Foo foo FOO", "foo"), [(0, 3), (4, 3), (8, 3)]);
        assert_eq!(occ("Foo foo FOO", "Foo"), [(0, 3)]);
    }

    #[test]
    fn columns_are_display_columns() {
        assert_eq!(occ("日本語 ab", "ab"), [(7, 2)]);
        assert_eq!(occ("日本", "本"), [(2, 2)]);
    }

    #[test]
    fn matches_do_not_overlap_and_empty_matches_nothing() {
        assert_eq!(occ("aaaa", "aa"), [(0, 2), (2, 2)]);
        assert!(occ("abc", "").is_empty());
        assert!(occ("ab", "abc").is_empty());
    }

    #[test]
    fn counts_clamp() {
        let mut s = Search::new("x");
        assert_eq!(s.counts(), (0, 0));
        s.hits = vec![
            Hit {
                block: 1,
                row: 0,
                col: 0,
                len: 1,
                pending: false,
            },
            Hit {
                block: 2,
                row: 0,
                col: 0,
                len: 1,
                pending: false,
            },
        ];
        s.cur = 1;
        assert_eq!(s.counts(), (2, 2));
    }
}
