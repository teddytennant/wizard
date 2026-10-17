//! `_highlight` from highlight.js 10.7.3 (lib/core.js), with the same control flow: the same
//! mode stack, buffer and cursor bookkeeping, and the same order of begin, end and ignore
//! decisions, because the colours are only as right as the exact choice of which rule wins.
//!
//! Offsets are UTF-8 bytes where hljs counts UTF-16 units. Nothing here depends on the unit,
//! only on being consistent.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use super::jsre::{Hit, Re};
use super::model::*;
use super::{get_language, language_exists, order};

thread_local! {
    static RUN: Cell<u64> = const { Cell::new(0) };
    /// `escape(lexeme)` regexes of `endSameAsBegin` modes, by lexeme.
    static LITERAL_ENDS: RefCell<HashMap<String, Option<Rc<Re>>>> = RefCell::new(HashMap::new());
}

// ---- the token tree (TokenTreeEmitter) -------------------------------------------------------

pub type NodeRef = Rc<RefCell<Node>>;

pub struct Node {
    pub kind: Option<String>,
    pub sublanguage: bool,
    pub children: Vec<Child>,
}

pub enum Child {
    Text(String),
    Node(NodeRef),
}

fn new_node(kind: Option<String>) -> NodeRef {
    Rc::new(RefCell::new(Node {
        kind,
        sublanguage: false,
        children: Vec::new(),
    }))
}

pub struct Emitter {
    pub root: NodeRef,
    stack: Vec<NodeRef>,
}

impl Emitter {
    fn new() -> Emitter {
        let root = new_node(None);
        Emitter {
            stack: vec![root.clone()],
            root,
        }
    }

    fn top(&self) -> &NodeRef {
        self.stack.last().expect("root stays on the stack")
    }

    fn add_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut top = self.top().borrow_mut();
        // adjacent text renders as one run, so merging it changes nothing
        if let Some(Child::Text(last)) = top.children.last_mut() {
            last.push_str(text);
        } else {
            top.children.push(Child::Text(text.to_string()));
        }
    }

    fn add_keyword(&mut self, text: &str, kind: &str) {
        if text.is_empty() {
            return;
        }
        self.open_node(kind);
        self.add_text(text);
        self.close_node();
    }

    fn open_node(&mut self, kind: &str) {
        let node = new_node(Some(kind.to_string()));
        self.top()
            .borrow_mut()
            .children
            .push(Child::Node(node.clone()));
        self.stack.push(node);
    }

    fn close_node(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
    }

    fn close_all(&mut self) {
        self.stack.truncate(1);
    }

    fn add_sublanguage(&mut self, other: Emitter, name: Option<String>) {
        {
            let mut n = other.root.borrow_mut();
            n.kind = name;
            n.sublanguage = true;
        }
        self.top()
            .borrow_mut()
            .children
            .push(Child::Node(other.root));
    }
}

// ---- one highlight run -----------------------------------------------------------------------

pub struct HResult {
    pub relevance: f64,
    pub emitter: Emitter,
    pub language: Option<String>,
    pub top: Option<Rc<Frame>>,
    /// the run stopped on an error and the text should be drawn unstyled
    pub errored: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Begin,
    End,
    Illegal,
}

struct Match {
    start: usize,
    end: usize,
    kind: Kind,
    /// the mode a begin match belongs to
    rule: usize,
    g1: Option<(usize, usize)>,
    position: usize,
}

enum Stop {
    Illegal,
    Other,
}

struct Run<'a> {
    lang: &'a Rc<Language>,
    text: &'a str,
    ignore_illegals: bool,
    run_id: u64,
    top: Rc<Frame>,
    emitter: Emitter,
    mode_buffer: String,
    relevance: f64,
    index: usize,
    iterations: usize,
    resume: bool,
    last: Option<(Kind, usize)>,
    continuations: HashMap<String, Option<Rc<Frame>>>,
}

fn next_char_len(s: &str, at: usize) -> usize {
    s.get(at..)
        .and_then(|r| r.chars().next())
        .map_or(1, char::len_utf8)
}

fn same_frame(a: &Option<Rc<Frame>>, b: &Option<Rc<Frame>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// `_highlight`. The language must exist.
pub fn highlight_run(
    lang: &Rc<Language>,
    language_name: &str,
    text: &str,
    ignore_illegals: bool,
    continuation: Option<Rc<Frame>>,
) -> HResult {
    let run_id = RUN.with(|r| {
        r.set(r.get() + 1);
        r.get()
    });
    let mut run = Run {
        lang,
        text,
        ignore_illegals,
        run_id,
        top: continuation.unwrap_or_else(|| lang.root.clone()),
        emitter: Emitter::new(),
        mode_buffer: String::new(),
        relevance: 0.0,
        index: 0,
        iterations: 0,
        resume: false,
        last: None,
        continuations: HashMap::new(),
    };
    run.process_continuations();
    match run.main_loop() {
        Ok(()) => {
            run.emitter.close_all();
            HResult {
                relevance: run.relevance.floor(),
                emitter: run.emitter,
                language: Some(language_name.to_string()),
                top: Some(run.top),
                errored: false,
            }
        }
        // the emitter keeps what it had; the caller decides whether that is used
        Err(Stop::Illegal) => HResult {
            relevance: 0.0,
            emitter: run.emitter,
            language: None,
            top: None,
            errored: true,
        },
        Err(Stop::Other) => HResult {
            relevance: 0.0,
            emitter: run.emitter,
            language: Some(language_name.to_string()),
            top: Some(run.top),
            errored: true,
        },
    }
}

/// `highlightAuto` over `subset` (or every registered language).
pub fn highlight_auto(code: &str, subset: Option<&[String]>) -> HResult {
    let names: Vec<String> = match subset {
        Some(s) => s.to_vec(),
        None => order().iter().map(|s| s.to_string()).collect(),
    };
    let mut plain = Emitter::new();
    plain.add_text(code);
    let mut results = vec![HResult {
        relevance: 0.0,
        emitter: plain,
        language: None,
        top: None,
        errored: false,
    }];
    for name in names {
        let Some(lang) = get_language(&name) else {
            continue;
        };
        if lang.no_autodetect {
            continue;
        }
        results.push(highlight_run(&lang, &name, code, false, None));
    }
    results.sort_by(|a, b| {
        if a.relevance != b.relevance {
            return b.relevance.total_cmp(&a.relevance);
        }
        if let (Some(an), Some(bn)) = (&a.language, &b.language) {
            if get_language(an)
                .and_then(|l| l.superset_of.clone())
                .as_deref()
                == Some(bn.as_str())
            {
                return std::cmp::Ordering::Greater;
            }
            if get_language(bn)
                .and_then(|l| l.superset_of.clone())
                .as_deref()
                == Some(an.as_str())
            {
                return std::cmp::Ordering::Less;
            }
        }
        std::cmp::Ordering::Equal
    });
    results.swap_remove(0)
}

impl<'a> Run<'a> {
    fn mode(&self, i: usize) -> &'a Mode {
        let lang: &'a Rc<Language> = self.lang;
        lang.mode(i)
    }

    fn top_mode(&self) -> &'a Mode {
        self.mode(self.top.mode)
    }

    fn alias<'s>(&'s self, class: &'s str) -> &'s str {
        self.lang
            .class_aliases
            .get(class)
            .map_or(class, String::as_str)
    }

    // -- scanning ---------------------------------------------------------------------------

    /// The next match of rule `j` of `mode` at or after `from`. A regex's answer holds for any
    /// later start up to its own start, so it is kept until the cursor passes it.
    fn rule_hit(&self, mode: usize, j: usize, from: usize) -> Option<Hit> {
        let m = self.mode(mode);
        let rx = match m.matcher.rules[j] {
            RuleKind::Begin(c) => &self.mode(c).begin,
            RuleKind::End => m.term.as_ref()?,
            RuleKind::Illegal => m.illegal.as_ref()?,
        };
        {
            let s = rx.scan.borrow();
            if s.run == self.run_id && from >= s.from && s.hit.is_none_or(|h| h.start >= from) {
                return s.hit;
            }
        }
        let hit = rx
            .get(self.lang.case_insensitive)
            .and_then(|re| re.find_at(self.text, from));
        *rx.scan.borrow_mut() = Scan {
            run: self.run_id,
            from,
            hit,
        };
        hit
    }

    /// The leftmost match over rules `start..`, earlier rule first on a tie, with the rule's
    /// position counted from `start` as a freshly built sub-matcher would.
    fn search(&self, mode: usize, start: usize, from: usize) -> Option<Match> {
        let rules = &self.mode(mode).matcher.rules;
        let mut best: Option<(usize, Hit)> = None;
        for j in start..rules.len() {
            if let Some(h) = self.rule_hit(mode, j, from) {
                if best.is_none_or(|(_, b)| h.start < b.start) {
                    best = Some((j, h));
                    if h.start == from {
                        break;
                    }
                }
            }
        }
        let (j, h) = best?;
        let (kind, rule) = match rules[j] {
            RuleKind::Begin(c) => (Kind::Begin, c),
            RuleKind::End => (Kind::End, mode),
            RuleKind::Illegal => (Kind::Illegal, mode),
        };
        Some(Match {
            start: h.start,
            end: h.end,
            kind,
            rule,
            g1: h.g1,
            position: j - start,
        })
    }

    /// `ResumableMultiRegex.exec`, including its habit of adding a position counted from a
    /// shorter rule list onto the resume index.
    fn exec(&self) -> Option<Match> {
        let mode = self.top.mode;
        let mt = &self.mode(mode).matcher;
        let resume_at = mt.regex_index.get();
        let last = mt.last_index.get();
        let mut result = self.search(mode, resume_at, last);
        if resume_at != 0 && result.as_ref().is_none_or(|r| r.start != last) {
            result = self.search(mode, 0, last + next_char_len(self.text, last));
        }
        if let Some(r) = &result {
            let next = resume_at + r.position + 1;
            mt.regex_index.set(if next == mt.count { 0 } else { next });
        }
        result
    }

    fn main_loop(&mut self) -> Result<(), Stop> {
        let text = self.text;
        loop {
            self.iterations += 1;
            let mt = &self.top_mode().matcher;
            if self.resume {
                self.resume = false;
            } else {
                mt.regex_index.set(0);
            }
            mt.last_index.set(self.index);
            let Some(m) = self.exec() else { break };
            let before = text.get(self.index..m.start).unwrap_or("");
            let processed = self.process_lexeme(before, Some(&m))?;
            self.index = m.start + processed;
        }
        self.process_lexeme(text.get(self.index..).unwrap_or(""), None)?;
        Ok(())
    }

    // -- the emitter-facing steps -----------------------------------------------------------

    fn process_keywords(&mut self) {
        let mb = std::mem::take(&mut self.mode_buffer);
        let mode = self.top_mode();
        let (Some(set), Some(pat)) = (mode.keywords, &mode.kw_pattern) else {
            self.emitter.add_text(&mb);
            return;
        };
        let ci = self.lang.case_insensitive;
        let words = &self.lang.keywords[set];
        let mut last_index = 0;
        let mut pos = 0;
        let mut buf = String::new();
        if let Some(re) = pat.get(ci) {
            while let Some(h) = re.find_at(&mb, pos) {
                buf.push_str(&mb[last_index..h.start]);
                let word = &mb[h.start..h.end];
                let key = if ci {
                    word.to_lowercase()
                } else {
                    word.to_string()
                };
                if let Some(kw) = words.get(&key) {
                    self.emitter.add_text(&buf);
                    buf.clear();
                    self.relevance += kw.relevance;
                    if kw.class.starts_with('_') {
                        // implied for relevance only, not drawn
                        buf.push_str(word);
                    } else {
                        let css = self.alias(&kw.class).to_string();
                        self.emitter.add_keyword(word, &css);
                    }
                } else {
                    buf.push_str(word);
                }
                last_index = h.end;
                // an empty match would never advance in JavaScript either
                pos = if h.end == h.start {
                    h.end + next_char_len(&mb, h.end)
                } else {
                    h.end
                };
            }
        }
        buf.push_str(mb.get(last_index..).unwrap_or(""));
        self.emitter.add_text(&buf);
    }

    fn process_sub_language(&mut self) {
        if self.mode_buffer.is_empty() {
            return;
        }
        let mb = std::mem::take(&mut self.mode_buffer);
        let mode = self.top_mode();
        let result = match &mode.sub {
            SubLanguage::One(name) => {
                if !language_exists(name) {
                    self.emitter.add_text(&mb);
                    return;
                }
                let lang = get_language(name).expect("language_exists");
                let cont = self.continuations.get(name).cloned().flatten();
                let r = highlight_run(&lang, name, &mb, true, cont);
                self.continuations.insert(name.clone(), r.top.clone());
                r
            }
            SubLanguage::Many(names) => {
                highlight_auto(&mb, (!names.is_empty()).then_some(names.as_slice()))
            }
            SubLanguage::None => unreachable!("checked by process_buffer"),
        };
        if mode.relevance > 0.0 {
            self.relevance += result.relevance;
        }
        self.emitter
            .add_sublanguage(result.emitter, result.language);
    }

    fn process_buffer(&mut self) {
        if matches!(self.top_mode().sub, SubLanguage::None) {
            self.process_keywords();
        } else {
            self.process_sub_language();
        }
        self.mode_buffer.clear();
    }

    fn start_new_mode(&mut self, idx: usize) {
        if let Some(c) = &self.mode(idx).class_name {
            let name = self.alias(c).to_string();
            self.emitter.open_node(&name);
        }
        self.top = Rc::new(Frame {
            mode: idx,
            parent: Some(self.top.clone()),
        });
    }

    fn process_continuations(&mut self) {
        let mut list = Vec::new();
        let mut cur = Some(self.top.clone());
        while let Some(f) = cur {
            if f.parent.is_none() {
                break;
            }
            if let Some(c) = &self.mode(f.mode).class_name {
                list.push(c.clone());
            }
            cur = f.parent.clone();
        }
        for c in list.iter().rev() {
            self.emitter.open_node(c);
        }
    }

    // -- ends -------------------------------------------------------------------------------

    /// `mode.endRe`, compiled on first use unless `endSameAsBegin` already replaced it.
    fn end_re(&self, mode: &Mode) -> Option<Rc<Re>> {
        if let Some(r) = mode.end_re.borrow().as_ref() {
            return Some(r.clone());
        }
        let src = mode.end.as_ref()?;
        let re = Re::new(src, self.lang.case_insensitive, true, false)
            .ok()
            .map(Rc::new)?;
        *mode.end_re.borrow_mut() = Some(re.clone());
        Some(re)
    }

    fn end_of_mode(&self, frame: &Rc<Frame>, m: &Match) -> Option<Rc<Frame>> {
        let mode = self.mode(frame.mode);
        let mut matched = self
            .end_re(mode)
            .is_some_and(|re| re.find_at(&self.text[m.start..], 0).is_some());
        if matched && mode.on_end == Some(Callback::SameEnd) {
            let g1 = m.g1.map(|(a, b)| &self.text[a..b]);
            if mode.begin_match.borrow().as_deref() != g1 {
                matched = false;
            }
        }
        if matched {
            let mut f = frame.clone();
            while self.mode(f.mode).flags & ENDS_PARENT != 0 {
                let Some(p) = f.parent.clone() else { break };
                f = p;
            }
            return Some(f);
        }
        // even when on:end ignored the match, a parent may still end here
        if mode.flags & ENDS_WITH_PARENT != 0 {
            return self.end_of_mode(frame.parent.as_ref()?, m);
        }
        None
    }

    // -- matches ----------------------------------------------------------------------------

    fn do_ignore(&mut self, lexeme: &str) -> usize {
        if self.top_mode().matcher.regex_index.get() == 0 {
            // no more rules to try here, so move the cursor one character
            let n = next_char_len(lexeme, 0).min(lexeme.len());
            self.mode_buffer.push_str(&lexeme[..n]);
            n.max(1)
        } else {
            // more rules to try at this very spot
            self.resume = true;
            0
        }
    }

    /// The callbacks hljs runs before a begin match is accepted; true means ignore the match.
    fn ignored_by_callbacks(&self, mode: &Mode, m: &Match) -> bool {
        if mode.flags & BEFORE_BEGIN != 0 && self.text[..m.start].ends_with('.') {
            return true;
        }
        match mode.on_begin {
            Some(Callback::SameBegin) => {
                *mode.begin_match.borrow_mut() = m.g1.map(|(a, b)| self.text[a..b].to_string());
                false
            }
            Some(Callback::AtStart) => m.start != 0,
            Some(Callback::Jsx) => {
                let next = self.text[m.end..].chars().next();
                match next {
                    Some('<') => true,
                    // `<tag>` is a tag only when `</tag` shows up later
                    Some('>') => {
                        let closing = format!("</{}", &self.text[m.start + 1..m.end]);
                        !self.text[m.end..].contains(&closing)
                    }
                    _ => false,
                }
            }
            Some(Callback::Mathematica) => !self
                .lang
                .system_symbols
                .contains(&self.text[m.start..m.end]),
            Some(Callback::SameEnd) | None => false,
        }
    }

    fn do_begin_match(&mut self, m: &Match) -> usize {
        let text = self.text;
        let lexeme = &text[m.start..m.end];
        let idx = m.rule;
        let new_mode = self.mode(idx);
        if self.ignored_by_callbacks(new_mode, m) {
            return self.do_ignore(lexeme);
        }
        if new_mode.flags & END_SAME_AS_BEGIN != 0 {
            // JavaScript's `escape()`: a backslash in front of every regex metacharacter
            let mut esc = String::with_capacity(lexeme.len() + 4);
            for c in lexeme.chars() {
                if "-/\\^$*+?.()|[]{}".contains(c) {
                    esc.push('\\');
                }
                esc.push(c);
            }
            let re = LITERAL_ENDS.with(|cache| {
                cache
                    .borrow_mut()
                    .entry(esc.clone())
                    .or_insert_with(|| Re::new(&esc, false, true, false).ok().map(Rc::new))
                    .clone()
            });
            *new_mode.end_re.borrow_mut() = re;
        }
        if new_mode.flags & SKIP != 0 {
            self.mode_buffer.push_str(lexeme);
        } else {
            if new_mode.flags & EXCLUDE_BEGIN != 0 {
                self.mode_buffer.push_str(lexeme);
            }
            self.process_buffer();
            if new_mode.flags & (RETURN_BEGIN | EXCLUDE_BEGIN) == 0 {
                self.mode_buffer = lexeme.to_string();
            }
        }
        self.start_new_mode(idx);
        if new_mode.flags & RETURN_BEGIN != 0 {
            0
        } else {
            lexeme.len()
        }
    }

    /// `None` is hljs's NO_MATCH: the end rule fired but the end regex does not match here.
    fn do_end_match(&mut self, m: &Match) -> Option<usize> {
        let text = self.text;
        let lexeme = &text[m.start..m.end];
        let end_mode = self.end_of_mode(&self.top.clone(), m)?;
        let origin = self.top.clone();
        let om = self.mode(origin.mode);
        if om.flags & SKIP != 0 {
            self.mode_buffer.push_str(lexeme);
        } else {
            if om.flags & (RETURN_END | EXCLUDE_END) == 0 {
                self.mode_buffer.push_str(lexeme);
            }
            self.process_buffer();
            if om.flags & EXCLUDE_END != 0 {
                self.mode_buffer = lexeme.to_string();
            }
        }
        loop {
            let tm = self.top_mode();
            if tm.class_name.is_some() {
                self.emitter.close_node();
            }
            if tm.flags & SKIP == 0 && matches!(tm.sub, SubLanguage::None) {
                self.relevance += tm.relevance;
            }
            let Some(p) = self.top.parent.clone() else {
                break;
            };
            self.top = p;
            if same_frame(&Some(self.top.clone()), &end_mode.parent) {
                break;
            }
        }
        let em = self.mode(end_mode.mode);
        if let Some(s) = em.starts {
            if em.flags & END_SAME_AS_BEGIN != 0 {
                let re = self.end_re(em);
                *self.mode(s).end_re.borrow_mut() = re;
            }
            self.start_new_mode(s);
        }
        Some(if om.flags & RETURN_END != 0 {
            0
        } else {
            lexeme.len()
        })
    }

    fn process_lexeme(&mut self, before: &str, m: Option<&Match>) -> Result<usize, Stop> {
        self.mode_buffer.push_str(before);
        let Some(m) = m else {
            self.process_buffer();
            return Ok(0);
        };
        let text = self.text;
        let lexeme = &text[m.start..m.end];
        // a zero-width match that is stuck: give the character it choked on back and move on
        if let Some((Kind::Begin, at)) = self.last {
            if m.kind == Kind::End && at == m.start && lexeme.is_empty() {
                let n = next_char_len(text, m.start);
                self.mode_buffer
                    .push_str(text.get(m.start..m.start + n).unwrap_or(""));
                return Ok(n);
            }
        }
        self.last = Some((m.kind, m.start));
        match m.kind {
            Kind::Begin => return Ok(self.do_begin_match(m)),
            Kind::Illegal if !self.ignore_illegals => return Err(Stop::Illegal),
            Kind::End => {
                if let Some(n) = self.do_end_match(m) {
                    return Ok(n);
                }
            }
            Kind::Illegal => {}
        }
        // an illegal match on `$` is zero-width and not a begin or end; step over it
        if m.kind == Kind::Illegal && lexeme.is_empty() {
            return Ok(next_char_len(text, m.start));
        }
        if self.iterations > 100_000 && self.iterations > m.start * 3 {
            return Err(Stop::Other);
        }
        // an end that fired but could not be completed lands here and is kept as text
        self.mode_buffer.push_str(lexeme);
        Ok(lexeme.len())
    }
}
