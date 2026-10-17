//! The compiled grammars `tools/hljs-dump.mjs` writes, as the engine walks them.
//!
//! A language is a flat list of modes that refer to each other by index (root is 0). Everything
//! highlight.js keeps on a mode object between calls lives here too, in cells, because the
//! engine reads it that way: the end regex that `endSameAsBegin` rewrites, the text a heredoc
//! opener captured, the scan state of the mode's rule matcher.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde::Deserialize;

use super::jsre::{Hit, Re};

#[derive(Deserialize)]
struct RawLang {
    m: Vec<RawMode>,
    #[serde(default)]
    k: Vec<Vec<(String, Rel, Words)>>,
    #[serde(default)]
    ci: u8,
    #[serde(default)]
    ca: HashMap<String, String>,
    so: Option<String>,
    #[serde(default)]
    na: u8,
    #[serde(default)]
    ss: Vec<String>,
}

/// The words of a keyword group: space-separated, or a list when one of them has a space.
#[derive(Deserialize)]
#[serde(untagged)]
enum Words {
    Joined(String),
    List(Vec<String>),
}

/// A relevance. A keyword scored with a malformed `|n` is NaN in highlight.js and `null` in JSON.
#[derive(Clone, Copy)]
struct Rel(f64);

impl<'de> Deserialize<'de> for Rel {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Rel, D::Error> {
        Ok(Rel(Option::<f64>::deserialize(d)?.unwrap_or(f64::NAN)))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawSub {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize, Default)]
struct RawMode {
    c: Option<String>,
    b: Option<String>,
    e: Option<String>,
    i: Option<String>,
    t: Option<String>,
    k: Option<usize>,
    kp: Option<String>,
    #[serde(default)]
    x: Vec<usize>,
    s: Option<usize>,
    r: Option<Rel>,
    sl: Option<RawSub>,
    #[serde(default)]
    f: u32,
    ob: Option<String>,
    oe: Option<String>,
}

pub const SKIP: u32 = 1;
pub const EXCLUDE_BEGIN: u32 = 2;
pub const EXCLUDE_END: u32 = 4;
pub const RETURN_BEGIN: u32 = 8;
pub const RETURN_END: u32 = 16;
pub const ENDS_PARENT: u32 = 32;
pub const ENDS_WITH_PARENT: u32 = 64;
pub const END_SAME_AS_BEGIN: u32 = 128;
pub const BEFORE_BEGIN: u32 = 256;

/// The hand-ported `on:begin` and `on:end` callbacks (ported in `engine::Run::ignored_by_callbacks` and `end_of_mode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Callback {
    /// heredoc-style openers: remember group 1
    SameBegin,
    /// ...and ignore an end whose group 1 differs
    SameEnd,
    /// shebang: only at the very start of the text
    AtStart,
    /// JSX tag opener in javascript and typescript
    Jsx,
    /// mathematica: keep the match only if it is a system symbol
    Mathematica,
}

impl Callback {
    fn parse(s: &str) -> Option<Callback> {
        Some(match s {
            "same-begin" => Callback::SameBegin,
            "same-end" => Callback::SameEnd,
            "at-start" => Callback::AtStart,
            "jsx" => Callback::Jsx,
            "mathematica" => Callback::Mathematica,
            _ => return None,
        })
    }
}

pub enum SubLanguage {
    None,
    One(String),
    /// auto-detect among these, or among everything when empty
    Many(Vec<String>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RuleKind {
    Begin(usize),
    End,
    Illegal,
}

/// The cached next match of one regex (a begin, a terminator or an illegal pattern), valid for
/// one highlight run: every later search starting at or before `hit.start` gets the same answer.
#[derive(Default)]
pub struct Scan {
    pub run: u64,
    pub from: usize,
    pub hit: Option<Hit>,
}

/// A regex plus its per-run scan cache.
pub struct Rx {
    src: String,
    want_g1: bool,
    re: OnceCell<Option<Re>>,
    pub scan: RefCell<Scan>,
}

impl Rx {
    fn new(src: String, want_g1: bool) -> Rx {
        Rx {
            src,
            want_g1,
            re: OnceCell::new(),
            scan: RefCell::new(Scan::default()),
        }
    }

    pub fn source(&self) -> &str {
        &self.src
    }

    /// Compile on first use. A pattern that will not compile never matches.
    pub fn get(&self, case_insensitive: bool) -> Option<&Re> {
        self.re
            .get_or_init(|| Re::new(&self.src, case_insensitive, false, self.want_g1).ok())
            .as_ref()
    }
}

/// `ResumableMultiRegex`: the rules of a mode in order, and where a scan that ignored a match
/// resumes. highlight.js joins the rules into one alternation; here each rule is searched on
/// its own and the leftmost wins, ties going to the earlier rule, which is what the alternation
/// returns.
#[derive(Default)]
pub struct Matcher {
    pub rules: Vec<RuleKind>,
    /// how many rules are begin rules (`count` in hljs, used to wrap the resume index)
    pub count: usize,
    pub last_index: Cell<usize>,
    pub regex_index: Cell<usize>,
}

pub struct Mode {
    pub class_name: Option<String>,
    pub begin: Rx,
    pub end: Option<String>,
    pub end_re: RefCell<Option<Rc<Re>>>,
    pub term: Option<Rx>,
    pub illegal: Option<Rx>,
    pub keywords: Option<usize>,
    pub kw_pattern: Option<Rx>,
    pub contains: Vec<usize>,
    pub starts: Option<usize>,
    pub relevance: f64,
    pub sub: SubLanguage,
    pub flags: u32,
    pub on_begin: Option<Callback>,
    pub on_end: Option<Callback>,
    pub matcher: Matcher,
    /// `mode.data._beginMatch`
    pub begin_match: RefCell<Option<String>>,
    pub has_begin: bool,
}

pub struct Keyword {
    pub class: String,
    pub relevance: f64,
}

pub struct Language {
    pub name: String,
    pub case_insensitive: bool,
    pub class_aliases: HashMap<String, String>,
    pub modes: Vec<Mode>,
    pub keywords: Vec<HashMap<String, Keyword>>,
    pub superset_of: Option<String>,
    pub no_autodetect: bool,
    /// mathematica's `SYSTEM_SYMBOLS_SET`
    pub system_symbols: HashSet<String>,
    pub root: Rc<Frame>,
}

/// A mode in use: `Object.create(mode, {parent})` in highlight.js.
pub struct Frame {
    pub mode: usize,
    pub parent: Option<Rc<Frame>>,
}

impl Language {
    pub fn parse(name: &str, json: &str) -> Result<Language, String> {
        let raw: RawLang = serde_json::from_str(json).map_err(|e| format!("{name}: {e}"))?;
        let ci = raw.ci != 0;
        let keywords = raw
            .k
            .iter()
            .map(|set| {
                let mut map = HashMap::new();
                for (class, relevance, words) in set {
                    let list: Vec<&str> = match words {
                        Words::Joined(s) => s.split(' ').collect(),
                        Words::List(v) => v.iter().map(String::as_str).collect(),
                    };
                    for w in list {
                        map.insert(
                            w.to_string(),
                            Keyword {
                                class: class.clone(),
                                relevance: relevance.0,
                            },
                        );
                    }
                }
                map
            })
            .collect();
        let modes: Vec<Mode> = raw
            .m
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                let on_begin = r.ob.as_deref().and_then(Callback::parse);
                let on_end = r.oe.as_deref().and_then(Callback::parse);
                let end = r.e.clone();
                let terminator = match (&r.t, &end) {
                    (Some(t), _) => t.clone(),
                    (None, Some(e)) => e.clone(),
                    (None, None) => String::new(),
                };
                let mut matcher = Matcher::default();
                for &c in &r.x {
                    matcher.rules.push(RuleKind::Begin(c));
                    matcher.count += 1;
                }
                if !terminator.is_empty() && idx != 0 {
                    matcher.rules.push(RuleKind::End);
                }
                if r.i.is_some() {
                    matcher.rules.push(RuleKind::Illegal);
                }
                Mode {
                    class_name: r.c.clone(),
                    // `same-begin` modes read group 1 of the begin match
                    begin: Rx::new(
                        r.b.clone().unwrap_or_default(),
                        on_begin == Some(Callback::SameBegin),
                    ),
                    end,
                    end_re: RefCell::new(None),
                    term: (!terminator.is_empty() && idx != 0)
                        .then(|| Rx::new(terminator, on_end == Some(Callback::SameEnd))),
                    illegal: r.i.clone().map(|s| Rx::new(s, false)),
                    keywords: r.k,
                    kw_pattern: r
                        .k
                        .map(|_| Rx::new(r.kp.clone().unwrap_or_else(|| "\\w+".into()), false)),
                    contains: r.x.clone(),
                    starts: r.s,
                    relevance: r.r.map_or(1.0, |r| r.0),
                    sub: match &r.sl {
                        None => SubLanguage::None,
                        Some(RawSub::One(s)) => SubLanguage::One(s.clone()),
                        Some(RawSub::Many(v)) => SubLanguage::Many(v.clone()),
                    },
                    flags: r.f,
                    on_begin,
                    on_end,
                    matcher,
                    begin_match: RefCell::new(None),
                    has_begin: r.b.is_some(),
                }
            })
            .collect();
        Ok(Language {
            name: name.to_string(),
            case_insensitive: ci,
            class_aliases: raw.ca,
            modes,
            keywords,
            superset_of: raw.so,
            no_autodetect: raw.na != 0,
            system_symbols: raw.ss.into_iter().collect(),
            root: Rc::new(Frame {
                mode: 0,
                parent: None,
            }),
        })
    }

    pub fn mode(&self, i: usize) -> &Mode {
        &self.modes[i]
    }
}
