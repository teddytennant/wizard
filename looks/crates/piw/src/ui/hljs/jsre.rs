//! highlight.js grammars are JavaScript regex sources. This turns one into a Rust pattern and
//! wraps the two engines that can run it: `regex` when the pattern has no look-around or
//! back-reference, `fancy_regex` when it does.
//!
//! The differences that change a match, all of them from the non-unicode `RegExp` hljs builds
//! with flags `m` (and `i`):
//! - `\w`, `\d` and `\b` are ASCII only; `\s` is JavaScript's own whitespace set.
//! - `.` stops at `\n`, `\r`, U+2028 and U+2029, and `[^]` matches anything.
//! - `{` and `}` that do not form a quantifier, a bare `]`, and `\` before a letter that means
//!   nothing are all literals. Rust rejects them.
//! - Inside a class `[`, `&`, `~` and `-` are literals; Rust treats `[`, `&&`, `~~` and `--`
//!   as set syntax.
//! - `$` and `^` follow Rust's multi-line rule, line breaks being `\n` only; JavaScript's also
//!   break at `\r`. Pi removes `\r` before it highlights anything, so none reaches here.
//! - Named groups become numbered ones (hljs counts groups, never looks at names), and
//!   `\k<name>` becomes `\N`.

use std::fmt::Write;

/// JavaScript's `\s`: WhiteSpace and LineTerminator, spelled for a Rust class body.
const WS: &str = "\\x{9}-\\x{d}\\x{20}\\x{a0}\\x{1680}\\x{2000}-\\x{200a}\\x{2028}\\x{2029}\\x{202f}\\x{205f}\\x{3000}\\x{feff}";
const WORD: &str = "A-Za-z0-9_";

enum Esc {
    /// One character.
    Ch(char),
    /// A class shorthand: the body for use inside `[...]`, and whether it is negated.
    Set(&'static str, bool),
    /// `\b` or `\B`.
    Boundary(bool),
    Backref(usize),
    Named(String),
}

struct Parser<'a> {
    c: &'a [char],
    i: usize,
    names: Vec<String>,
    groups: usize,
    /// target `fancy_regex`, which has no `(?-u:\b)`: spell word boundaries with look-around
    fancy: bool,
}

fn hex(c: &[char], at: usize, n: usize) -> Option<u32> {
    let s: String = c.get(at..at + n)?.iter().collect();
    if s.chars().all(|x| x.is_ascii_hexdigit()) {
        u32::from_str_radix(&s, 16).ok()
    } else {
        None
    }
}

/// A character as Rust pattern text, safe both inside and outside a class.
fn lit(ch: char, out: &mut String) {
    if ch.is_ascii_alphanumeric() || ch == '_' || !ch.is_ascii() && !ch.is_control() {
        out.push(ch);
    } else {
        let _ = write!(out, "\\x{{{:x}}}", ch as u32);
    }
}

impl Parser<'_> {
    fn peek(&self, off: usize) -> Option<char> {
        self.c.get(self.i + off).copied()
    }

    /// After a backslash: decode the escape and advance past it.
    fn escape(&mut self, in_class: bool) -> Result<Esc, String> {
        let Some(ch) = self.peek(0) else {
            return Err("trailing backslash".into());
        };
        self.i += 1;
        Ok(match ch {
            'w' => Esc::Set(WORD, false),
            'W' => Esc::Set(WORD, true),
            'd' => Esc::Set("0-9", false),
            'D' => Esc::Set("0-9", true),
            's' => Esc::Set(WS, false),
            'S' => Esc::Set(WS, true),
            'b' if in_class => Esc::Ch('\u{8}'),
            'b' => Esc::Boundary(true),
            'B' => Esc::Boundary(false),
            'n' => Esc::Ch('\n'),
            'r' => Esc::Ch('\r'),
            't' => Esc::Ch('\t'),
            'f' => Esc::Ch('\u{c}'),
            'v' => Esc::Ch('\u{b}'),
            '0' => Esc::Ch('\0'),
            '1'..='9' if in_class => {
                Esc::Ch(char::from_u32(ch as u32 - '0' as u32).unwrap_or('\0'))
            }
            '1'..='9' => {
                let mut n = ch as usize - '0' as usize;
                while let Some(d) = self.peek(0).and_then(|d| d.to_digit(10)) {
                    n = n * 10 + d as usize;
                    self.i += 1;
                }
                Esc::Backref(n)
            }
            'k' if self.peek(0) == Some('<') && !in_class => {
                let end = self.c[self.i..]
                    .iter()
                    .position(|&x| x == '>')
                    .ok_or("unterminated \\k")?;
                let name: String = self.c[self.i + 1..self.i + end].iter().collect();
                self.i += end + 1;
                Esc::Named(name)
            }
            'x' => match hex(self.c, self.i, 2) {
                Some(v) => {
                    self.i += 2;
                    Esc::Ch(char::from_u32(v).unwrap_or('\u{fffd}'))
                }
                None => Esc::Ch('x'),
            },
            'u' => match hex(self.c, self.i, 4) {
                Some(v) => {
                    self.i += 4;
                    // a surrogate pair written as two escapes is one character
                    if (0xd800..0xdc00).contains(&v)
                        && self.peek(0) == Some('\\')
                        && self.peek(1) == Some('u')
                    {
                        if let Some(lo) =
                            hex(self.c, self.i + 2, 4).filter(|l| (0xdc00..0xe000).contains(l))
                        {
                            self.i += 6;
                            let cp = 0x10000 + ((v - 0xd800) << 10) + (lo - 0xdc00);
                            return Ok(Esc::Ch(char::from_u32(cp).unwrap_or('\u{fffd}')));
                        }
                    }
                    Esc::Ch(char::from_u32(v).unwrap_or('\u{fffd}'))
                }
                None => Esc::Ch('u'),
            },
            'c' => match self.peek(0).filter(|l| l.is_ascii_alphabetic()) {
                Some(l) => {
                    self.i += 1;
                    Esc::Ch(char::from_u32(l as u32 % 32).unwrap_or('\0'))
                }
                None => Esc::Ch('\\'),
            },
            other => Esc::Ch(other),
        })
    }

    fn class(&mut self, out: &mut String) -> Result<(), String> {
        // `[` already consumed
        let negate = self.peek(0) == Some('^');
        if negate {
            self.i += 1;
        }
        if self.peek(0) == Some(']') {
            // `[]` never matches and `[^]` always does
            self.i += 1;
            out.push_str(if negate {
                "[\\x{0}-\\x{10ffff}]"
            } else {
                "[^\\x{0}-\\x{10ffff}]"
            });
            return Ok(());
        }
        let mut body = String::new();
        loop {
            let Some(ch) = self.peek(0) else {
                return Err("unterminated class".into());
            };
            self.i += 1;
            if ch == ']' {
                break;
            }
            let first = if ch == '\\' {
                match self.escape(true)? {
                    Esc::Ch(x) => Atom::Ch(x),
                    Esc::Set(s, neg) => Atom::Set(s, neg),
                    _ => return Err("bad escape in class".into()),
                }
            } else {
                Atom::Ch(ch)
            };
            // a range needs a character on both sides of the dash; anything else makes the dash literal
            if let Atom::Ch(lo) = first {
                if self.peek(0) == Some('-') && self.peek(1).is_some_and(|n| n != ']') {
                    let save = self.i;
                    self.i += 1;
                    let n = self.peek(0).unwrap();
                    self.i += 1;
                    let hi = if n == '\\' {
                        match self.escape(true)? {
                            Esc::Ch(x) => Some(x),
                            _ => None,
                        }
                    } else {
                        Some(n)
                    };
                    if let Some(hi) = hi {
                        class_char(lo, &mut body);
                        body.push('-');
                        class_char(hi, &mut body);
                        continue;
                    }
                    self.i = save;
                }
            }
            match first {
                Atom::Ch(x) => class_char(x, &mut body),
                Atom::Set(s, false) => body.push_str(s),
                Atom::Set(s, true) => {
                    let _ = write!(body, "[^{s}]");
                }
            }
        }
        if body.is_empty() {
            return Err("empty class".into());
        }
        out.push('[');
        if negate {
            out.push('^');
        }
        out.push_str(&body);
        out.push(']');
        Ok(())
    }

    fn run(&mut self) -> Result<String, String> {
        let mut out = String::with_capacity(self.c.len() + 16);
        while let Some(ch) = self.peek(0) {
            self.i += 1;
            match ch {
                '\\' => match self.escape(false)? {
                    Esc::Ch(x) => lit(x, &mut out),
                    Esc::Set(s, false) => {
                        let _ = write!(out, "[{s}]");
                    }
                    Esc::Set(s, true) => {
                        let _ = write!(out, "[^{s}]");
                    }
                    Esc::Boundary(b) => out.push_str(match (self.fancy, b) {
                        (false, true) => "(?-u:\\b)",
                        (false, false) => "(?-u:\\B)",
                        (true, true) => "(?:(?<=[A-Za-z0-9_])(?![A-Za-z0-9_])|(?<![A-Za-z0-9_])(?=[A-Za-z0-9_]))",
                        (true, false) => "(?:(?<=[A-Za-z0-9_])(?=[A-Za-z0-9_])|(?<![A-Za-z0-9_])(?![A-Za-z0-9_]))",
                    }),
                    Esc::Backref(n) => {
                        let _ = write!(out, "\\{n}");
                    }
                    Esc::Named(name) => {
                        let n = self
                            .names
                            .iter()
                            .position(|x| *x == name)
                            .ok_or("unknown group name")?;
                        let _ = write!(out, "\\{}", n + 1);
                    }
                },
                '[' => self.class(&mut out)?,
                '.' => out.push_str("[^\\n\\r\\x{2028}\\x{2029}]"),
                '(' => {
                    if self.peek(0) == Some('?') {
                        match (self.peek(1), self.peek(2)) {
                            (Some(':' | '=' | '!'), _) => {
                                out.push_str("(?");
                                out.push(self.peek(1).unwrap());
                                self.i += 2;
                            }
                            (Some('<'), Some('=' | '!')) => {
                                out.push_str("(?<");
                                out.push(self.peek(2).unwrap());
                                self.i += 3;
                            }
                            (Some('<'), _) => {
                                let end = self.c[self.i..]
                                    .iter()
                                    .position(|&x| x == '>')
                                    .ok_or("unterminated group name")?;
                                let name: String = self.c[self.i + 2..self.i + end].iter().collect();
                                self.i += end + 1;
                                self.groups += 1;
                                // index by group number so \k<name> finds it
                                self.names.resize(self.groups, String::new());
                                self.names[self.groups - 1] = name;
                                out.push('(');
                            }
                            _ => return Err("unknown group syntax".into()),
                        }
                    } else {
                        self.groups += 1;
                        out.push('(');
                    }
                }
                '{' => {
                    // a quantifier is {n}, {n,} or {n,m}; anything else is a literal brace
                    let rest = &self.c[self.i..];
                    let digits = rest.iter().take_while(|d| d.is_ascii_digit()).count();
                    let mut j = digits;
                    if digits > 0 && rest.get(j) == Some(&',') {
                        j += 1;
                        j += rest[j..].iter().take_while(|d| d.is_ascii_digit()).count();
                    }
                    if digits > 0 && rest.get(j) == Some(&'}') {
                        out.push('{');
                        out.extend(&rest[..=j]);
                        self.i += j + 1;
                    } else {
                        out.push_str("\\{");
                    }
                }
                '}' => out.push_str("\\}"),
                ']' => out.push_str("\\]"),
                other => lit_or_syntax(other, &mut out),
            }
        }
        Ok(out)
    }
}

enum Atom {
    Ch(char),
    Set(&'static str, bool),
}

/// A literal inside `[...]`. Everything that is not a letter or digit goes in as a code point,
/// so `[`, `&`, `~`, `-` and `^` cannot be read as class syntax.
fn class_char(ch: char, out: &mut String) {
    lit(ch, out);
}

/// Outside a class the characters that are syntax in both dialects pass through untouched.
fn lit_or_syntax(ch: char, out: &mut String) {
    match ch {
        '^' | '$' | '*' | '+' | '?' | '|' | ')' => out.push(ch),
        _ if ch.is_ascii_alphanumeric() || ch == '_' || !ch.is_ascii() => out.push(ch),
        // space, quotes, `#`, `<`, `>` and the like mean themselves
        _ => lit(ch, out),
    }
}

/// Translate a JavaScript regex source into Rust pattern text.
pub fn translate(src: &str, fancy: bool) -> Result<String, String> {
    let c: Vec<char> = src.chars().collect();
    let mut p = Parser {
        c: &c,
        i: 0,
        names: Vec::new(),
        groups: 0,
        fancy,
    };
    p.run()
}

/// A match: byte offsets, and the first capture group when the caller asked for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub start: usize,
    pub end: usize,
    pub g1: Option<(usize, usize)>,
}

enum Inner {
    Std(regex::Regex),
    Fancy(Box<fancy_regex::Regex>),
}

pub struct Re {
    inner: Inner,
    want_g1: bool,
}

impl Re {
    /// `anchored` makes it match only at the start of the haystack (hljs's `startsWith`).
    pub fn new(
        src: &str,
        case_insensitive: bool,
        anchored: bool,
        want_g1: bool,
    ) -> Result<Re, String> {
        let flags = if case_insensitive { "(?mi)" } else { "(?m)" };
        let wrap = |t: String| {
            if anchored {
                format!("{flags}\\A(?:{t})")
            } else {
                format!("{flags}{t}")
            }
        };
        let pat = wrap(translate(src, false)?);
        let inner = match regex::RegexBuilder::new(&pat)
            .size_limit(64 << 20)
            .dfa_size_limit(64 << 20)
            .build()
        {
            Ok(r) => Inner::Std(r),
            Err(_) => {
                let pat = wrap(translate(src, true)?);
                Inner::Fancy(Box::new(
                    fancy_regex::RegexBuilder::new(&pat)
                        .build()
                        .map_err(|e| format!("{e}: {pat}"))?,
                ))
            }
        };
        Ok(Re { inner, want_g1 })
    }

    /// Leftmost match starting at or after `pos`, with the text before `pos` visible to
    /// look-behind and `\b`.
    pub fn find_at(&self, text: &str, pos: usize) -> Option<Hit> {
        if pos > text.len() {
            return None;
        }
        match &self.inner {
            Inner::Std(r) => {
                if self.want_g1 {
                    let c = r.captures_at(text, pos)?;
                    let m = c.get(0)?;
                    Some(Hit {
                        start: m.start(),
                        end: m.end(),
                        g1: c.get(1).map(|g| (g.start(), g.end())),
                    })
                } else {
                    let m = r.find_at(text, pos)?;
                    Some(Hit {
                        start: m.start(),
                        end: m.end(),
                        g1: None,
                    })
                }
            }
            Inner::Fancy(r) => {
                // a backtrack-limit error is a rule that does not match
                let c = r.captures_from_pos(text, pos).ok()??;
                let m = c.get(0)?;
                Some(Hit {
                    start: m.start(),
                    end: m.end(),
                    g1: c.get(1).map(|g| (g.start(), g.end())),
                })
            }
        }
    }
}
