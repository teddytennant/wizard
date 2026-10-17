//! A port of the scoring in `fuzzysort` 3.x, the library opencode's `DialogSelect` filters with,
//! so a filtered list comes out in the same order. Single terms follow the library's algorithm
//! (simple subsequence, then a strict word-start alignment with backtracking, substring
//! bonuses); several words must each match and share the score.

/// A match: `score` is normalized to the 0..1 range like `Result.score`, 1 being perfect.
#[derive(Clone, Debug, PartialEq)]
pub struct Fuzz {
    pub score: f64,
    /// Char indices of the matched characters in the target.
    pub indexes: Vec<usize>,
}

fn lower_chars(s: &str) -> Vec<char> {
    s.to_lowercase().chars().collect()
}

/// `prepareNextBeginningIndexes`: for every index, the next index that starts a word (after a
/// non-alphanumeric, at a lower-to-upper step, or a non-alphanumeric itself), else the length.
fn next_beginnings(target: &[char]) -> Vec<usize> {
    let n = target.len();
    let mut beginnings = Vec::new();
    let (mut was_upper, mut was_alnum) = (false, false);
    for (i, &c) in target.iter().enumerate() {
        let code = c as u32;
        let is_upper = (65..=90).contains(&code);
        let is_alnum = is_upper || (97..=122).contains(&code) || (48..=57).contains(&code);
        let is_beginning = (is_upper && !was_upper) || !was_alnum || !is_alnum;
        was_upper = is_upper;
        was_alnum = is_alnum;
        if is_beginning {
            beginnings.push(i);
        }
    }
    let mut next = vec![n; n];
    let mut li = 0usize;
    let mut last = beginnings.first().copied();
    for (i, slot) in next.iter_mut().enumerate() {
        match last {
            Some(l) if l > i => *slot = l,
            _ => {
                li += 1;
                last = beginnings.get(li).copied();
                *slot = last.unwrap_or(n);
            }
        }
    }
    next
}

/// `normalizeScore`: raw scores are zero or negative; map them into (0, 1].
fn normalize(raw: f64) -> f64 {
    if raw > 1.0 {
        return raw;
    }
    std::f64::consts::E.powf(((-raw + 1.0).powf(0.04307) - 1.0) * -2.0)
}

/// One term against one target. `None` when the term's characters are not in order.
fn algorithm(search: &[char], target: &[char], lower: &[char]) -> Option<(f64, Vec<usize>)> {
    let search_len = search.len();
    let target_len = lower.len();
    if search_len == 0 || target_len == 0 {
        return None;
    }
    // simple pass: first occurrence of each character in order
    let mut simple: Vec<usize> = Vec::with_capacity(search_len);
    let (mut si, mut ti) = (0usize, 0usize);
    loop {
        if search[si] == lower[ti] {
            simple.push(ti);
            si += 1;
            if si == search_len {
                break;
            }
        }
        ti += 1;
        if ti >= target_len {
            return None;
        }
    }

    // strict pass: only word starts, backing up when stuck
    let next = next_beginnings(target);
    let mut strict: Vec<usize> = Vec::with_capacity(search_len);
    let mut success_strict = false;
    let mut search_i = 0usize;
    let mut target_i = if simple[0] == 0 {
        0
    } else {
        next[simple[0] - 1]
    };
    let mut backtracks = 0;
    if target_i != target_len {
        loop {
            if target_i >= target_len {
                if search_i == 0 {
                    break;
                }
                backtracks += 1;
                if backtracks > 200 {
                    break;
                }
                search_i -= 1;
                let last = strict.pop().unwrap_or(0);
                target_i = next[last];
            } else if search[search_i] == lower[target_i] {
                strict.push(target_i);
                search_i += 1;
                if search_i == search_len {
                    success_strict = true;
                    break;
                }
                target_i += 1;
            } else {
                target_i = next[target_i];
            }
        }
    }

    // substring bonuses
    let find = |from: usize| -> Option<usize> {
        if search_len > target_len {
            return None;
        }
        (from..=target_len - search_len).find(|&i| lower[i..i + search_len] == *search)
    };
    let mut substring: Option<usize> = if search_len <= 1 {
        None
    } else {
        find(simple[0])
    };
    let mut sub_begin = substring.is_some_and(|i| i == 0 || next[i - 1] == i);
    if let (Some(si), false) = (substring, sub_begin) {
        let mut i = 0usize;
        while i < next.len() {
            if i > si && i + search_len <= target_len && lower[i..i + search_len] == *search {
                substring = Some(i);
                sub_begin = true;
                break;
            }
            i = next[i];
        }
    }

    let calc = |m: &[usize]| -> f64 {
        let mut score = 0.0f64;
        let mut extra_groups = 0.0f64;
        for i in 1..search_len {
            if m[i] != m[i - 1] + 1 {
                score -= m[i] as f64;
                extra_groups += 1.0;
            }
        }
        let unmatched = (m[search_len - 1] - m[0]) as f64 - (search_len as f64 - 1.0);
        score -= (12.0 + unmatched) * extra_groups;
        if m[0] != 0 {
            score -= (m[0] * m[0]) as f64 * 0.2;
        }
        if !success_strict {
            score *= 1000.0;
        } else {
            let mut unique = 1usize;
            let mut i = next[0];
            while i < target_len {
                unique += 1;
                i = next[i];
            }
            if unique > 24 {
                score *= ((unique - 24) * 10) as f64;
            }
        }
        let diff = (target_len as f64 - search_len as f64) / 2.0;
        score -= diff;
        let sq = 1.0 + (search_len * search_len) as f64;
        if substring.is_some() {
            score /= sq;
        }
        if sub_begin {
            score /= sq;
        }
        score - diff
    };

    let best: Vec<usize> = if !success_strict {
        match substring {
            Some(i) => (0..search_len).map(|k| i + k).collect(),
            None => simple,
        }
    } else if sub_begin {
        let i = substring.unwrap_or(0);
        (0..search_len).map(|k| i + k).collect()
    } else {
        strict
    };
    let raw = calc(&best);
    Some((raw, best))
}

/// Match `query` against `target`. Several whitespace-separated words must all match; the score
/// is their mean, docked when they come in a different order than in the target.
pub fn go(query: &str, target: &str) -> Option<Fuzz> {
    let words: Vec<Vec<char>> = query.split_whitespace().map(lower_chars).collect();
    if words.is_empty() || target.is_empty() {
        return None;
    }
    let chars: Vec<char> = target.chars().collect();
    let lower = lower_chars(target);
    if lower.len() != chars.len() {
        // a character whose lowercase is longer; fall back to comparing the lowered text only
        return go_lowered(&words, &lower, &lower);
    }
    go_lowered(&words, &chars, &lower)
}

fn go_lowered(words: &[Vec<char>], target: &[char], lower: &[char]) -> Option<Fuzz> {
    let mut score = 0.0f64;
    let mut first_last = 0usize;
    let mut all: Vec<usize> = Vec::new();
    for w in words {
        let (raw, idx) = algorithm(w, target, lower)?;
        score += raw / words.len() as f64;
        if idx[0] < first_last {
            score -= (first_last - idx[0]) as f64 * 2.0;
        }
        first_last = idx[0];
        all.extend(idx);
    }
    all.sort_unstable();
    all.dedup();
    Some(Fuzz {
        score: normalize(score),
        indexes: all,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(q: &str, items: &[&str]) -> Vec<String> {
        let mut v: Vec<(f64, usize, &str)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, t)| go(q, t).map(|m| (m.score, i, *t)))
            .collect();
        v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(a.1.cmp(&b.1)));
        v.into_iter().map(|x| x.2.to_string()).collect()
    }

    #[test]
    fn a_word_start_substring_beats_scattered_letters() {
        let r = rank(
            "th",
            &[
                "Exit the app",
                "Switch theme",
                "Lock theme mode",
                "Switch to light mode",
                "Write heap snapshot",
            ],
        );
        assert_eq!(r[0], "Exit the app");
        assert_eq!(r[1], "Switch theme");
        assert_eq!(r.last().unwrap(), "Write heap snapshot");
    }

    #[test]
    fn needs_the_characters_in_order() {
        assert!(go("xz", "Switch theme").is_none());
        assert!(go("eht", "Switch theme").is_none());
        assert!(go("sw", "Switch theme").is_some());
    }

    #[test]
    fn shorter_targets_win_ties() {
        let r = rank("mode", &["Switch to light mode", "Mode"]);
        assert_eq!(r[0], "Mode");
    }

    #[test]
    fn words_all_have_to_match() {
        assert!(go("light mode", "Switch to light mode").is_some());
        assert!(go("light xyz", "Switch to light mode").is_none());
    }

    #[test]
    fn scores_are_in_the_unit_range() {
        let m = go("theme", "Switch theme").unwrap();
        assert!(m.score > 0.0 && m.score <= 1.0, "{}", m.score);
        assert_eq!(m.indexes.len(), 5);
    }
}

/// Order results the way `fuzzysort.go` returns them: it pushes every match through a binary
/// min-heap on the score and polls them off into the result array from the back, so equal scores
/// come out in heap order, not input order. `scores[i]` is the score of input `i`; the answer is
/// the input indices, best first.
pub fn rank(scores: &[f64]) -> Vec<usize> {
    let n = scores.len();
    let mut heap: Vec<usize> = Vec::with_capacity(n);
    let sc = |i: usize| scores[i];
    // add
    for i in 0..n {
        let mut a = heap.len();
        heap.push(i);
        while a > 0 {
            let p = (a - 1) >> 1;
            if sc(i) < sc(heap[p]) {
                heap[a] = heap[p];
                a = p;
            } else {
                break;
            }
        }
        heap[a] = i;
    }
    let mut out = vec![0usize; n];
    let mut len = n;
    for slot in (0..n).rev() {
        // poll
        let top = heap[0];
        len -= 1;
        heap[0] = heap[len];
        // sift down: move the smaller child up to the bottom, then float the item back up
        let v = heap[0];
        let mut a = 0usize;
        let mut c = 1usize;
        while c < len {
            let s = c + 1;
            a = c;
            if s < len && sc(heap[s]) < sc(heap[c]) {
                a = s;
            }
            heap[(a.wrapping_sub(1)) >> 1] = heap[a];
            c = 1 + (a << 1);
        }
        if len > 0 {
            let mut f = (a.wrapping_sub(1)) >> 1;
            while a > 0 && sc(v) < sc(heap[f]) {
                heap[a] = heap[f];
                a = f;
                f = (a.wrapping_sub(1)) >> 1;
            }
            heap[a] = v;
        }
        out[slot] = top;
    }
    out
}

#[cfg(test)]
mod rank_tests {
    use super::rank;

    #[test]
    fn sorts_descending() {
        let s = [0.2, 0.9, 0.5, 0.7];
        assert_eq!(rank(&s), vec![1, 3, 2, 0]);
    }

    #[test]
    fn handles_one_and_none() {
        assert_eq!(rank(&[]), Vec::<usize>::new());
        assert_eq!(rank(&[0.3]), vec![0]);
    }
}
