//! A port of the fuzzysort 4 matcher opencode's autocomplete uses (the copy bundled in the
//! 1.18.34 binary), so `/mo` ranks commands the way the reference does. Only the single-term
//! path is ported: the popup closes at the first whitespace, so a search never has a space.
//! Indices are char positions where the original uses UTF-16 units; they only matter for ASCII.

const NEG_INF: f64 = f64::NEG_INFINITY;

/// A target prepared once: lower-cased chars, a character-class bitmask for the quick reject
/// and the table of word-start positions.
pub struct Prepared {
    target: String,
    lower: Vec<char>,
    bits: u32,
    next_beginning: Vec<usize>,
}

fn flag(c: char) -> u32 {
    let m = c as u32;
    let w = if (97..=122).contains(&m) {
        m - 97
    } else if (48..=57).contains(&m) {
        26
    } else if m <= 127 {
        30
    } else {
        31
    };
    1 << w
}

fn lower_chars(s: &str) -> Vec<char> {
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

fn bits_of(chars: &[char]) -> u32 {
    chars
        .iter()
        .filter(|c| **c != ' ')
        .fold(0, |a, c| a | flag(*c))
}

/// Positions where a word starts: the first char, an upper case char after a non-upper one, and
/// anything that is not alphanumeric or follows something that is not.
fn beginnings(chars: &[char]) -> Vec<usize> {
    let (mut was_upper, mut was_alnum) = (false, false);
    let mut out = Vec::new();
    for (i, c) in chars.iter().enumerate() {
        let upper = c.is_ascii_uppercase();
        let alnum = c.is_ascii_alphanumeric();
        if (upper && !was_upper) || !was_alnum || !alnum {
            out.push(i);
        }
        was_upper = upper;
        was_alnum = alnum;
    }
    out
}

/// `next[i]` is the first word start after `i`, or the length when there is none.
fn next_beginnings(chars: &[char]) -> Vec<usize> {
    let n = chars.len();
    let b = beginnings(chars);
    let mut out = Vec::with_capacity(n);
    let mut k = 0usize;
    let mut f = b.first().copied();
    for i in 0..n {
        match f {
            Some(v) if v > i => out.push(v),
            _ => {
                k += 1;
                f = b.get(k).copied();
                out.push(f.unwrap_or(n));
            }
        }
    }
    out
}

impl Prepared {
    pub fn new(target: &str) -> Prepared {
        let chars: Vec<char> = target.chars().collect();
        let lower = lower_chars(target);
        Prepared {
            target: target.to_string(),
            bits: bits_of(&lower),
            next_beginning: next_beginnings(&chars),
            lower,
        }
    }

    pub fn target(&self) -> &str {
        &self.target
    }
}

/// A search term, prepared the same way.
pub struct Search {
    lower: Vec<char>,
    bits: u32,
}

impl Search {
    pub fn new(s: &str) -> Search {
        let lower = lower_chars(s.trim());
        Search {
            bits: bits_of(&lower),
            lower,
        }
    }
}

/// Raw (non-positive) score of `search` against `p`, or `None` when it is not a subsequence.
/// 0 is a perfect match; more negative is worse.
pub fn score(search: &Search, p: &Prepared) -> Option<f64> {
    let q = &search.lower;
    let t = &p.lower;
    let (sl, tl) = (q.len(), t.len());
    if sl == 0 {
        return None;
    }
    // simple walk: leftmost subsequence
    let mut simple: Vec<usize> = Vec::with_capacity(sl);
    let (mut si, mut ti) = (0usize, 0usize);
    loop {
        if tl == 0 {
            return None;
        }
        if q[si] == t[ti] {
            simple.push(ti);
            si += 1;
            if si == sl {
                break;
            }
        }
        ti += 1;
        if ti >= tl {
            return None;
        }
    }
    // strict walk: only word starts and consecutive chars count
    let nb = &p.next_beginning;
    let mut strict: Vec<usize> = Vec::with_capacity(sl);
    let mut success_strict = false;
    let mut si = 0usize;
    ti = if simple[0] == 0 { 0 } else { nb[simple[0] - 1] };
    let mut backtrack = 0;
    if ti != tl {
        loop {
            if ti >= tl {
                if si == 0 {
                    break;
                }
                backtrack += 1;
                if backtrack > 200 {
                    break;
                }
                si -= 1;
                let last = strict.pop().expect("strict has si entries");
                ti = nb[last];
            } else if q[si] == t[ti] {
                strict.push(ti);
                si += 1;
                if si == sl {
                    success_strict = true;
                    break;
                }
                ti += 1;
            } else {
                ti = nb[ti];
            }
        }
    }
    // substring test
    let mut sub: Option<usize> = if sl <= 1 {
        None
    } else {
        (simple[0]..=tl.saturating_sub(sl)).find(|&i| t[i..i + sl] == q[..])
    };
    let mut sub_begin = match sub {
        Some(i) => i == 0 || nb[i - 1] == i,
        None => false,
    };
    if let (Some(si0), false) = (sub, sub_begin) {
        let mut i = 0usize;
        while i < nb.len() {
            if i > si0 && i + sl <= tl && t[i..i + sl] == q[..] {
                sub = Some(i);
                sub_begin = true;
                break;
            }
            i = nb[i];
        }
    }
    let is_sub = sub.is_some();
    let calc = |m: &[usize]| -> f64 {
        let mut s = 0.0f64;
        let mut groups = 0.0f64;
        for i in 1..sl {
            if m[i] != m[i - 1] + 1 {
                s -= m[i] as f64;
                groups += 1.0;
            }
        }
        let unmatched = m[sl - 1] as f64 - m[0] as f64 - (sl as f64 - 1.0);
        s -= (12.0 + unmatched) * groups;
        if m[0] != 0 {
            s -= (m[0] * m[0]) as f64 * 0.2;
        }
        if !success_strict {
            s *= 1000.0;
        } else {
            let mut unique = 1usize;
            let mut i = nb[0];
            while i < tl {
                unique += 1;
                i = nb[i];
            }
            if unique > 24 {
                s *= ((unique - 24) * 10) as f64;
            }
        }
        let len_pen = (tl as f64 - sl as f64) / 2.0;
        s -= len_pen;
        let bonus = 1.0 + (sl * sl) as f64;
        if is_sub {
            s /= bonus;
        }
        if sub_begin {
            s /= bonus;
        }
        s - len_pen
    };
    let substr_run = |start: usize| -> Vec<usize> { (0..sl).map(|i| start + i).collect() };
    let best: Vec<usize> = if !success_strict {
        match sub {
            Some(s) => substr_run(s),
            None => simple,
        }
    } else if sub_begin {
        substr_run(sub.expect("sub_begin implies sub"))
    } else {
        strict
    };
    Some(calc(&best))
}

/// Quick reject on the character-class masks, then [`score`].
pub fn single(search: &Search, p: &Prepared) -> Option<f64> {
    if search.bits & p.bits != search.bits {
        return None;
    }
    score(search, p)
}

/// Public 0..1 score from a raw one (the `score` getter).
pub fn normalize(raw: f64) -> f64 {
    if raw == NEG_INF {
        0.0
    } else if raw > 1.0 {
        raw
    } else {
        (((-raw + 1.0).powf(0.04307) - 1.0) * -2.0).exp()
    }
}

/// Inverse of [`normalize`]: what a `scoreFn` result is stored as.
pub fn denormalize(v: f64) -> f64 {
    if v == 0.0 {
        NEG_INF
    } else if v > 1.0 {
        v
    } else {
        1.0 - ((v.ln() / -2.0 + 1.0).powf(23.218017181332716))
    }
}

/// The keys of one object: `None` for a missing or empty key.
pub type KeyScores = Vec<Option<f64>>;

/// How `go` folds several keys into one raw score.
fn combine(scores: &KeyScores) -> Option<f64> {
    if scores.iter().all(|s| s.is_none()) {
        return None;
    }
    let mut h = NEG_INF;
    for s in scores {
        let s = s.unwrap_or(NEG_INF);
        if s > -1000.0 && h > NEG_INF {
            let g = (h + s) / 4.0;
            if g > h {
                h = g;
            }
        }
        if s > h {
            h = s;
        }
    }
    Some(h)
}

/// The library's binary min-heap, ported line for line so equal scores come out in the same
/// order. Payloads are indices into the caller's slice.
struct Heap {
    x: Vec<(f64, usize)>,
    d: usize,
}

impl Heap {
    fn new() -> Self {
        Heap {
            x: Vec::new(),
            d: 0,
        }
    }

    fn set(&mut self, i: usize, v: (f64, usize)) {
        if i == self.x.len() {
            self.x.push(v);
        } else {
            self.x[i] = v;
        }
    }

    fn add(&mut self, v: (f64, usize)) {
        let mut u = self.d as i64;
        self.set(self.d, v);
        self.d += 1;
        let mut m = (u - 1) >> 1;
        while u > 0 && v.0 < self.x[m as usize].0 {
            self.x[u as usize] = self.x[m as usize];
            u = m;
            m = (u - 1) >> 1;
        }
        self.x[u as usize] = v;
    }

    fn sift(&mut self) {
        let mv = self.x[0];
        let (mut u, mut w) = (0i64, 1i64);
        let d = self.d as i64;
        while w < d {
            let e = w + 1;
            u = w;
            if e < d && self.x[e as usize].0 < self.x[w as usize].0 {
                u = e;
            }
            self.x[((u - 1) >> 1) as usize] = self.x[u as usize];
            w = 1 + (u << 1);
        }
        let mut r = (u - 1) >> 1;
        while u > 0 && mv.0 < self.x[r as usize].0 {
            self.x[u as usize] = self.x[r as usize];
            u = r;
            r = (u - 1) >> 1;
        }
        self.x[u as usize] = mv;
    }

    fn poll(&mut self) -> Option<(f64, usize)> {
        if self.d == 0 {
            return None;
        }
        let top = self.x[0];
        self.d -= 1;
        self.x[0] = self.x[self.d];
        self.sift();
        Some(top)
    }

    fn replace_top(&mut self, v: (f64, usize)) {
        self.x[0] = v;
        self.sift();
    }
}

/// `fuzzysort.go(search, objs, { keys, threshold, limit, scoreFn })`. `keys[i]` holds object
/// `i`'s prepared keys (`None` for a missing or empty one). `score_fn` gets the object index and
/// the combined 0..1 score and returns the final score, or 0 to drop the object. Best first.
pub fn go(
    search: &str,
    keys: &[Vec<Option<&Prepared>>],
    threshold: f64,
    limit: usize,
    mut score_fn: impl FnMut(usize, f64) -> f64,
) -> Vec<usize> {
    let s = Search::new(search);
    if s.lower.is_empty() {
        return Vec::new();
    }
    let min = denormalize(threshold);
    let mut heap = Heap::new();
    for (i, ks) in keys.iter().enumerate() {
        let all_bits = ks.iter().flatten().fold(0u32, |a, p| a | p.bits);
        if s.bits & all_bits != s.bits {
            continue;
        }
        let scores: KeyScores = ks.iter().map(|k| k.and_then(|p| score(&s, p))).collect();
        let Some(h) = combine(&scores) else { continue };
        let out = score_fn(i, normalize(h));
        if out == 0.0 || out.is_nan() {
            continue;
        }
        let raw = denormalize(out);
        if raw < min {
            continue;
        }
        if heap.d < limit {
            heap.add((raw, i));
        } else if raw > heap.x[0].0 {
            heap.replace_top((raw, i));
        }
    }
    let mut out = vec![0usize; heap.d];
    for slot in (0..out.len()).rev() {
        out[slot] = heap.poll().expect("heap has d entries").1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(q: &str, items: &[&[&str]]) -> Vec<usize> {
        let prepared: Vec<Vec<Option<Prepared>>> = items
            .iter()
            .map(|ks| {
                ks.iter()
                    .map(|k| (!k.is_empty()).then(|| Prepared::new(k)))
                    .collect()
            })
            .collect();
        let refs: Vec<Vec<Option<&Prepared>>> = prepared
            .iter()
            .map(|v| v.iter().map(|o| o.as_ref()).collect())
            .collect();
        go(q, &refs, 0.0, 10, |_, s| s)
    }

    #[test]
    fn exact_and_prefix_beat_scattered_matches() {
        let one = |s: &'static str| -> Vec<&'static str> { vec![s] };
        let a = one("/models");
        let b = one("/mcps");
        let c = one("/themes");
        let r = rank("mo", &[&a, &b, &c]);
        // "/models" starts with mo after the slash; "/mcps" has m...c no o; "/themes" m then e..
        assert_eq!(r, vec![0]);
    }

    #[test]
    fn single_key_scores_follow_the_library() {
        // perfect word-start prefix of a short target scores close to zero
        let s = Search::new("mo");
        let p = Prepared::new("/models");
        let raw = single(&s, &p).unwrap();
        assert!(raw < 0.0 && raw > -3.0, "{raw}");
        assert!(normalize(raw) > 0.85);
        let far = single(&s, &Prepared::new("Move to another project dir")).unwrap();
        assert!(far < raw);
    }

    #[test]
    fn missing_letters_do_not_match() {
        let s = Search::new("zq");
        assert!(single(&s, &Prepared::new("/models")).is_none());
    }

    #[test]
    fn normalize_round_trips() {
        for v in [0.2, 0.5, 0.9, 1.0] {
            assert!((normalize(denormalize(v)) - v).abs() < 1e-9, "{v}");
        }
        assert_eq!(normalize(denormalize(2.0)), 2.0);
    }

    #[test]
    fn heap_orders_best_first() {
        let keys: Vec<Vec<Option<Prepared>>> = ["abc", "xabc", "a_b_c", "abcabc"]
            .iter()
            .map(|t| vec![Some(Prepared::new(t))])
            .collect();
        let refs: Vec<Vec<Option<&Prepared>>> = keys
            .iter()
            .map(|v| v.iter().map(|o| o.as_ref()).collect())
            .collect();
        let order = go("abc", &refs, 0.0, 10, |_, s| s);
        assert_eq!(order[0], 0);
        assert_eq!(order.len(), 4);
    }
}
