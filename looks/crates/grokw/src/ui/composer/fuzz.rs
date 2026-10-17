//! Fuzzy matching with the same engine and settings Grok Build uses (nucleo, smart case, smart
//! normalization), so a query ranks rows the way the real popup does. Paths use nucleo's path
//! bonuses (a slash is a word boundary), everything else the plain config.

use std::cell::RefCell;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

thread_local! {
    static PLAIN: RefCell<Matcher> = RefCell::new(Matcher::new(Config::DEFAULT));
    static PATHS: RefCell<Matcher> = RefCell::new(Matcher::new(Config::DEFAULT.match_paths()));
}

/// A parsed query. Whitespace separates atoms, all of which must match.
pub struct Query(Pattern);

impl Query {
    pub fn new(q: &str) -> Query {
        Query(Pattern::parse(
            q.trim(),
            CaseMatching::Smart,
            Normalization::Smart,
        ))
    }

    pub fn is_empty(&self) -> bool {
        self.0.atoms.is_empty()
    }

    pub fn score(&self, text: &str, paths: bool) -> Option<u32> {
        let mut buf = Vec::new();
        let hay = Utf32Str::new(text, &mut buf);
        with(paths, |m| self.0.score(hay, m))
    }

    /// Char indices of `text` the query matched, `None` when it does not match.
    pub fn indices(&self, text: &str, paths: bool) -> Option<Vec<u32>> {
        let mut buf = Vec::new();
        let hay = Utf32Str::new(text, &mut buf);
        let mut out = Vec::new();
        with(paths, |m| self.0.indices(hay, m, &mut out))?;
        out.sort_unstable();
        out.dedup();
        Some(out)
    }
}

fn with<R>(paths: bool, f: impl FnOnce(&mut Matcher) -> R) -> R {
    if paths {
        PATHS.with(|m| f(&mut m.borrow_mut()))
    } else {
        PLAIN.with(|m| f(&mut m.borrow_mut()))
    }
}
