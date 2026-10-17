//! Small fzf-style subsequence scorer with match positions.
//!
//! A query matches when its characters appear in order in the text. Among all alignments the
//! best score wins: a hit at a word start (after a space, `/`, `_`, `-`, `.` or a lower to
//! upper transition) earns a bonus, hits that follow each other earn a bonus, and gaps cost.
//! Smart case: an all-lowercase query ignores case, a query with capitals does not.

const MATCH: i32 = 16;
const GAP_START: i32 = -3;
const GAP_EXT: i32 = -1;
const BONUS_BOUNDARY: i32 = 8;
const BONUS_CAMEL: i32 = 7;
const BONUS_NON_ALNUM: i32 = 6;
const BONUS_CONSECUTIVE: i32 = 5;
const FIRST_CHAR_MULT: i32 = 2;
const NEG: i32 = i32::MIN / 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    /// Char (not byte) indices of the matched characters in the text, ascending.
    pub indices: Vec<usize>,
}

fn is_sep(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '/' | '\\' | '_' | '-' | '.' | ':' | ',' | '(' | '[' | '{'
        )
}

fn bonus_at(chars: &[char], j: usize) -> i32 {
    let c = chars[j];
    if j == 0 {
        return if c.is_alphanumeric() {
            BONUS_BOUNDARY
        } else {
            BONUS_NON_ALNUM
        };
    }
    let p = chars[j - 1];
    if is_sep(p) && c.is_alphanumeric() {
        BONUS_BOUNDARY
    } else if p.is_lowercase() && c.is_uppercase() {
        BONUS_CAMEL
    } else if p.is_ascii_digit() != c.is_ascii_digit() && c.is_alphanumeric() && p.is_alphanumeric()
    {
        BONUS_CAMEL - 2
    } else if !c.is_alphanumeric() {
        BONUS_NON_ALNUM
    } else {
        0
    }
}

fn fold(c: char, case_sensitive: bool) -> char {
    if case_sensitive {
        c
    } else if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// A query prepared once for many texts: folded to its case mode, so a list of thousands of
/// paths is not re-folding the query for each one.
#[derive(Clone, Debug)]
pub struct Prepared {
    qf: Vec<char>,
    cs: bool,
}

impl Prepared {
    pub fn new(query: &str) -> Prepared {
        let cs = query.chars().any(char::is_uppercase);
        Prepared {
            qf: query.chars().map(|c| fold(c, cs)).collect(),
            cs,
        }
    }

    /// Whether the query's characters appear in `text` in order. This is the whole test for
    /// [`score`] returning `Some`, without scoring or allocating, so it is what to run first on
    /// a long list.
    pub fn is_subsequence(&self, text: &str) -> bool {
        let mut qi = 0;
        for c in text.chars() {
            if qi == self.qf.len() {
                break;
            }
            if fold(c, self.cs) == self.qf[qi] {
                qi += 1;
            }
        }
        qi == self.qf.len()
    }

    /// Best of the whole `path` and its last component, which starts at byte `name_start`, the
    /// component's score raised by `name_bonus`. One decode of the path serves both, and nothing
    /// is allocated per call. `None` when neither holds the query.
    pub fn score_path_only(&self, path: &str, name_start: usize, name_bonus: i32) -> Option<i32> {
        SCRATCH.with(|s| {
            let mut guard = s.borrow_mut();
            let Scratch {
                t,
                tf,
                cells,
                row_start,
            } = &mut *guard;
            t.clear();
            t.extend(path.chars());
            tf.clear();
            tf.extend(t.iter().map(|&c| fold(c, self.cs)));
            let full = table_on(cells, row_start, &self.qf, t, tf).map(|(s, _)| s);
            let at = path[..name_start.min(path.len())].chars().count();
            let name = table_on(cells, row_start, &self.qf, &t[at..], &tf[at..])
                .map(|(s, _)| s + name_bonus);
            match (full, name) {
                (Some(f), Some(n)) => Some(n.max(f)),
                (f, n) => f.or(n),
            }
        })
    }

    /// The score [`score`] would give, without the match positions: no allocation per call,
    /// which is what ranking a long list wants. `None` when the query is not a subsequence.
    pub fn score_only(&self, text: &str) -> Option<i32> {
        SCRATCH
            .with(|s| table(&mut s.borrow_mut(), &self.qf, self.cs, text).map(|(score, _)| score))
    }
}

/// One reachable cell of the scoring table: query character `i` matched at text column `j`.
#[derive(Clone, Copy)]
struct Cell {
    j: usize,
    h: i32,
    /// Index of the cell in the previous row this one extends.
    from: usize,
}

/// Buffers for [`score`], kept per thread so scoring a list does not allocate per entry.
#[derive(Default)]
struct Scratch {
    t: Vec<char>,
    tf: Vec<char>,
    cells: Vec<Cell>,
    /// `cells[row_start[i]..row_start[i + 1]]` is row `i`.
    row_start: Vec<usize>,
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::default();
}

/// Score a single term (no whitespace handling). `None` when it is not a subsequence.
pub fn score(query: &str, text: &str) -> Option<Match> {
    SCRATCH.with(|s| score_in(&mut s.borrow_mut(), query, text))
}

fn score_in(sc: &mut Scratch, query: &str, text: &str) -> Option<Match> {
    if query.is_empty() {
        return Some(Match {
            score: 0,
            indices: Vec::new(),
        });
    }
    let cs = query.chars().any(|c| c.is_uppercase());
    let qf: Vec<char> = query.chars().map(|c| fold(c, cs)).collect();
    let (bs, bi) = table(sc, &qf, cs, text)?;
    let m = qf.len();
    let mut indices = vec![0; m];
    let mut k = bi;
    for i in (0..m).rev() {
        indices[i] = sc.cells[k].j;
        if i > 0 {
            k = sc.cells[k].from;
        }
    }
    Some(Match { score: bs, indices })
}

/// The best alignment of the query (folded to `qf`) in the text, as a table over the columns
/// where each query character occurs. Every cell is the best score with that character matched
/// at that column; a cell extends the cell just before it (consecutive bonus) or the best
/// earlier one at a gap cost. Only columns holding the character are visited, so the work is
/// the number of occurrences, not the length of the text times the length of the query.
/// Returns the best score and the index of its last cell; the cells stay in `sc` for the
/// caller to walk back through.
fn table(sc: &mut Scratch, qf: &[char], cs: bool, text: &str) -> Option<(i32, usize)> {
    if qf.is_empty() {
        return Some((0, usize::MAX));
    }
    sc.t.clear();
    sc.t.extend(text.chars());
    sc.tf.clear();
    sc.tf.extend(sc.t.iter().map(|&c| fold(c, cs)));
    let Scratch {
        t,
        tf,
        cells,
        row_start,
    } = sc;
    table_on(cells, row_start, qf, t, tf)
}

/// [`table`] over text that is already decoded (`t`) and folded (`tf`).
fn table_on(
    cells: &mut Vec<Cell>,
    row_start: &mut Vec<usize>,
    qf: &[char],
    t: &[char],
    tf: &[char],
) -> Option<(i32, usize)> {
    if qf.len() > t.len() {
        return None;
    }
    // Quick reject: plain subsequence test.
    let mut qi = 0;
    for &c in tf {
        if qi < qf.len() && c == qf[qi] {
            qi += 1;
        }
    }
    if qi < qf.len() {
        return None;
    }

    let m = qf.len();
    cells.clear();
    row_start.clear();
    for i in 0..m {
        let start = cells.len();
        row_start.push(start);
        let prev_start = if i > 0 { row_start[i - 1] } else { 0 };
        // The gapped predecessor of column j is the best of h[k] + GAP_START + GAP_EXT * (j-2-k)
        // over earlier cells k <= j-2. Written as a(k) + GAP_EXT * j it is a running maximum of
        // a(k) = h[k] + GAP_START - GAP_EXT * (k+2); a later cell wins a tie.
        let mut best_a = NEG;
        let mut gap_idx = usize::MAX;
        let mut p = prev_start;
        for (j, &c) in tf.iter().enumerate().skip(i) {
            if c != qf[i] {
                continue;
            }
            let b = bonus_at(t, j);
            if i == 0 {
                cells.push(Cell {
                    j,
                    h: MATCH + b * FIRST_CHAR_MULT,
                    from: usize::MAX,
                });
                continue;
            }
            while p < start && cells[p].j + 2 <= j {
                let a = cells[p].h + GAP_START - GAP_EXT * (cells[p].j as i32 + 2);
                if a >= best_a {
                    best_a = a;
                    gap_idx = p;
                }
                p += 1;
            }
            let mut best = NEG;
            let mut best_from = usize::MAX;
            if p < start && cells[p].j + 1 == j {
                best = cells[p].h + BONUS_CONSECUTIVE;
                best_from = p;
            }
            if best_a > NEG {
                let gap = best_a + GAP_EXT * j as i32;
                if gap > best {
                    best = gap;
                    best_from = gap_idx;
                }
            }
            if best > NEG {
                cells.push(Cell {
                    j,
                    h: best + MATCH + b,
                    from: best_from,
                });
            }
        }
        if cells.len() == start {
            return None;
        }
    }
    row_start.push(cells.len());
    // The best end cell; the earliest column wins a tie.
    let last = row_start[m - 1]..row_start[m];
    let mut bi = usize::MAX;
    let mut bs = NEG;
    for k in last {
        if cells[k].h > bs {
            bs = cells[k].h;
            bi = k;
        }
    }
    if bi == usize::MAX {
        return None;
    }
    Some((bs, bi))
}

/// Score a whole query: whitespace separated terms must all match; scores add and indices are
/// merged. An empty query matches everything with score 0.
pub fn score_query(query: &str, text: &str) -> Option<Match> {
    let mut total = 0;
    let mut indices: Vec<usize> = Vec::new();
    for term in query.split_whitespace() {
        let m = score(term, text)?;
        total += m.score;
        indices.extend(m.indices);
    }
    indices.sort_unstable();
    indices.dedup();
    Some(Match {
        score: total,
        indices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(q: &str, t: &str) -> Option<Vec<usize>> {
        score(q, t).map(|m| m.indices)
    }

    #[test]
    fn subsequence_required_in_order() {
        assert!(score("abc", "a_b_c").is_some());
        assert!(score("abc", "acb").is_none());
        assert!(score("abcd", "abc").is_none());
        assert_eq!(score("", "anything").unwrap().indices, Vec::<usize>::new());
    }

    #[test]
    fn indices_point_at_the_matched_chars() {
        assert_eq!(idx("ssn", "Switch session").map(|v| v.len()), Some(3));
        let m = score("sess", "Switch session").unwrap();
        // the word-start alignment ("session") beats scattering across "Switch"
        assert_eq!(m.indices, vec![7, 8, 9, 10]);
    }

    #[test]
    fn word_starts_beat_mid_word_hits() {
        let a = score("ms", "Move session").unwrap().score;
        let b = score("ms", "Remise").unwrap().score;
        assert!(a > b, "{a} vs {b}");
    }

    #[test]
    fn consecutive_beats_scattered() {
        let a = score("sw", "switch").unwrap().score;
        let b = score("sw", "s....w").unwrap().score;
        assert!(a > b);
    }

    #[test]
    fn word_start_hits_rank_above_scattered_hits() {
        let at_word = score("th", "Exit the app").unwrap().score;
        let scattered = score("th", "Write heap snapshot").unwrap().score;
        assert!(at_word > scattered, "{at_word} vs {scattered}");
    }

    #[test]
    fn smart_case() {
        assert!(score("abc", "ABC").is_some());
        assert!(score("Abc", "abc").is_none());
        assert!(score("Abc", "xAbc").is_some());
    }

    #[test]
    fn camel_case_boundaries_get_a_bonus() {
        let a = score("fb", "fooBar").unwrap().score;
        let b = score("fb", "foobar").unwrap().score;
        assert!(a > b);
    }

    #[test]
    fn multi_term_queries_need_every_term() {
        let m = score_query("sw mod", "Switch model").unwrap();
        assert_eq!(m.indices, vec![0, 1, 7, 8, 9]);
        assert!(score_query("sw zzz", "Switch model").is_none());
        assert!(score_query("   ", "x").is_some());
    }

    #[test]
    fn unicode_and_long_inputs() {
        assert_eq!(idx("日語", "日本語"), Some(vec![0, 2]));
        assert!(score("é", "Cafe\u{301} é").is_some());
        let long = "a".repeat(5000);
        assert!(score("aaa", &long).is_some());
        assert!(score("ab", &long).is_none());
    }

    /// The table over every column, as the scorer was first written. The fast scorer must give
    /// the same score and the same indices on every input.
    fn reference(query: &str, text: &str) -> Option<Match> {
        let q: Vec<char> = query.chars().collect();
        if q.is_empty() {
            return Some(Match {
                score: 0,
                indices: Vec::new(),
            });
        }
        let t: Vec<char> = text.chars().collect();
        if q.len() > t.len() {
            return None;
        }
        let cs = q.iter().any(|c| c.is_uppercase());
        let qf: Vec<char> = q.iter().map(|&c| fold(c, cs)).collect();
        let tf: Vec<char> = t.iter().map(|&c| fold(c, cs)).collect();
        let mut qi = 0;
        for &c in &tf {
            if qi < qf.len() && c == qf[qi] {
                qi += 1;
            }
        }
        if qi < qf.len() {
            return None;
        }
        let (m, n) = (qf.len(), tf.len());
        let mut h = vec![vec![NEG; n]; m];
        let mut from = vec![vec![usize::MAX; n]; m];
        let bonus: Vec<i32> = (0..n).map(|j| bonus_at(&t, j)).collect();
        for i in 0..m {
            let mut gap_best = NEG;
            let mut gap_k = usize::MAX;
            for j in i..n {
                if i > 0 && j >= 2 {
                    let cand = h[i - 1][j - 2];
                    gap_best = gap_best.saturating_add(GAP_EXT).max(NEG);
                    if cand > NEG {
                        let c = cand + GAP_START;
                        if c >= gap_best {
                            gap_best = c;
                            gap_k = j - 2;
                        }
                    }
                }
                if tf[j] != qf[i] {
                    continue;
                }
                let b = bonus[j] * if i == 0 { FIRST_CHAR_MULT } else { 1 };
                if i == 0 {
                    h[0][j] = MATCH + b;
                    continue;
                }
                let mut best = NEG;
                let mut best_k = usize::MAX;
                if j >= 1 && h[i - 1][j - 1] > NEG {
                    best = h[i - 1][j - 1] + BONUS_CONSECUTIVE;
                    best_k = j - 1;
                }
                if gap_best > NEG && gap_best > best {
                    best = gap_best;
                    best_k = gap_k;
                }
                if best > NEG {
                    h[i][j] = best + MATCH + b;
                    from[i][j] = best_k;
                }
            }
        }
        let (mut bj, mut bs) = (usize::MAX, NEG);
        for (j, &v) in h[m - 1].iter().enumerate() {
            if v > bs {
                bs = v;
                bj = j;
            }
        }
        if bj == usize::MAX {
            return None;
        }
        let mut indices = vec![0; m];
        let mut j = bj;
        for i in (0..m).rev() {
            indices[i] = j;
            if i > 0 {
                j = from[i][j];
            }
        }
        Some(Match { score: bs, indices })
    }

    #[test]
    fn the_fast_scorer_agrees_with_the_full_table_on_random_input() {
        let alphabet: Vec<char> = "aabbcde_-./ AB9é日 ".chars().collect();
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..30_000 {
            let tl = 1 + (next() % 24) as usize;
            let text: String = (0..tl)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            let ql = (next() % 6) as usize;
            let query: String = (0..ql)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            assert_eq!(
                score(&query, &text),
                reference(&query, &text),
                "q {query:?} in {text:?}"
            );
        }
    }

    #[test]
    fn is_subsequence_is_exactly_when_score_finds_a_match() {
        for (q, t) in [
            ("abc", "a_b_c"),
            ("abc", "acb"),
            ("Abc", "abc"),
            ("é", "E"),
            ("", "x"),
            ("ab", "a"),
        ] {
            assert_eq!(
                Prepared::new(q).is_subsequence(t),
                score(q, t).is_some(),
                "{q:?} {t:?}"
            );
        }
    }
}
