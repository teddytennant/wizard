//! Reading a workflow script without running it.
//!
//! A script starts with `export const meta = {...}`, a pure literal, and the
//! rest is the body. Claude Code parses that with a JavaScript parser. Wizard
//! does not carry one outside the QuickJS feature, so this reads the literal
//! with a small recursive-descent reader that accepts exactly what a literal
//! can be: objects, arrays, strings, numbers, `true`, `false`, `null`,
//! comments and trailing commas. Anything else is the same refusal Claude
//! gives, in the same words where the binary has them.

use serde_json::{Map, Number, Value};

/// Largest script accepted, in bytes.
pub const MAX_SCRIPT_BYTES: usize = 1024 * 1024;

const FIRST_STATEMENT: &str =
    "`export const meta = { name, description, phases }` must be the FIRST statement in the script";

/// One entry of `meta.phases`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhaseMeta {
    pub title: String,
    pub detail: Option<String>,
    pub model: Option<String>,
}

/// What a script says about itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub name: String,
    pub description: String,
    pub title: Option<String>,
    pub when_to_use: Option<String>,
    pub phases: Vec<PhaseMeta>,
}

/// A script split into its `meta` and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub meta: Meta,
    pub body: String,
}

/// Split `script` into `meta` and body, or say why it cannot be a workflow.
pub fn parse(script: &str) -> Result<Parsed, String> {
    if script.len() > MAX_SCRIPT_BYTES {
        return Err(format!("Script exceeds {MAX_SCRIPT_BYTES} bytes"));
    }
    if let Some(bad) = script
        .chars()
        .find(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(format!(
            "workflow script contains disallowed control characters (U+{:04X}).",
            bad as u32
        ));
    }
    let mut reader = Reader::new(script);
    reader.skip_trivia();
    if !reader.eat_word("export") {
        return Err(FIRST_STATEMENT.to_string());
    }
    reader.skip_trivia();
    if !reader.eat_word("const") {
        return Err(FIRST_STATEMENT.to_string());
    }
    reader.skip_trivia();
    if !reader.eat_word("meta") {
        return Err(FIRST_STATEMENT.to_string());
    }
    reader.skip_trivia();
    if !reader.eat('=') {
        return Err(FIRST_STATEMENT.to_string());
    }
    reader.skip_trivia();
    if reader.peek() != Some('{') {
        return Err(FIRST_STATEMENT.to_string());
    }
    let literal = reader
        .value()
        .map_err(|why| format!("meta must be a pure literal: {why}"))?;
    let meta = meta_of(&literal)?;
    reader.skip_inline_trivia();
    reader.eat(';');
    let body = script[reader.pos..]
        .trim_start_matches(|c: char| c == ';' || c.is_whitespace())
        .to_string();
    Ok(Parsed { meta, body })
}

fn meta_of(value: &Value) -> Result<Meta, String> {
    let object = value.as_object().ok_or(FIRST_STATEMENT)?;
    let name = match object.get("name") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => return Err("meta.name must be a non-empty string".into()),
    };
    let description = match object.get("description") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => return Err("meta.description must be a non-empty string".into()),
    };
    let text = |key: &str| match object.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    let mut phases = Vec::new();
    if let Some(Value::Array(items)) = object.get("phases") {
        for item in items {
            if let Some(Value::String(title)) = item.get("title") {
                let field = |key: &str| match item.get(key) {
                    Some(Value::String(s)) => Some(s.clone()),
                    _ => None,
                };
                phases.push(PhaseMeta {
                    title: title.clone(),
                    detail: field("detail"),
                    model: field("model"),
                });
            }
        }
    }
    Ok(Meta {
        name,
        description,
        title: text("title"),
        when_to_use: object
            .get("whenToUse")
            .and_then(Value::as_str)
            .map(str::to_string),
        phases,
    })
}

struct Reader<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(src: &'a str) -> Self {
        let mut reader = Reader { src, pos: 0 };
        if src.starts_with("#!") {
            reader.pos = src.find('\n').unwrap_or(src.len());
        }
        reader
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let rest = self.rest();
        if !rest.starts_with(word) {
            return false;
        }
        let after = rest[word.len()..].chars().next();
        if after.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$') {
            return false;
        }
        self.pos += word.len();
        true
    }

    /// Whitespace and comments, across lines.
    fn skip_trivia(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start();
            self.pos += rest.len() - trimmed.len();
            if trimmed.starts_with("//") {
                self.pos += trimmed.find('\n').unwrap_or(trimmed.len());
            } else if trimmed.starts_with("/*") {
                match trimmed[2..].find("*/") {
                    Some(end) => self.pos += end + 4,
                    None => self.pos = self.src.len(),
                }
            } else {
                return;
            }
        }
    }

    /// Spaces and comments that end on the same line, so a `;` after the
    /// literal is found but the next statement is not eaten.
    fn skip_inline_trivia(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start_matches([' ', '\t']);
            self.pos += rest.len() - trimmed.len();
            if trimmed.starts_with("/*")
                && let Some(end) = trimmed[2..].find("*/")
                && !trimmed[..end + 4].contains('\n')
            {
                self.pos += end + 4;
                continue;
            }
            return;
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.skip_trivia();
        match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some(q @ ('"' | '\'')) => self.string(q).map(Value::String),
            Some('`') => self.template(),
            Some(c) if c.is_ascii_digit() || c == '-' || c == '.' => self.number(),
            Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {
                if self.eat_word("true") {
                    Ok(Value::Bool(true))
                } else if self.eat_word("false") {
                    Ok(Value::Bool(false))
                } else if self.eat_word("null") {
                    Ok(Value::Null)
                } else {
                    Err("non-literal node type in meta: Identifier".into())
                }
            }
            Some('.') => Err("spread not allowed in meta".into()),
            Some(c) => Err(format!("non-literal node type in meta: `{c}`")),
            None => Err("unexpected end of script".into()),
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.eat('{');
        let mut map = Map::new();
        loop {
            self.skip_trivia();
            if self.eat('}') {
                return Ok(Value::Object(map));
            }
            if self.rest().starts_with("...") {
                return Err("spread not allowed in meta".into());
            }
            let key = match self.peek() {
                Some(q @ ('"' | '\'')) => self.string(q)?,
                Some('[') => return Err("computed keys not allowed in meta".into()),
                Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {
                    let start = self.pos;
                    while self
                        .peek()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
                    {
                        self.pos += self.peek().map_or(0, char::len_utf8);
                    }
                    self.src[start..self.pos].to_string()
                }
                Some(c) if c.is_ascii_digit() => {
                    let number = self.number()?;
                    number.to_string()
                }
                _ => return Err("only plain properties allowed in meta".into()),
            };
            if matches!(key.as_str(), "__proto__" | "constructor" | "prototype") {
                return Err(format!("reserved key name not allowed in meta: {key}"));
            }
            self.skip_trivia();
            match self.peek() {
                Some(':') => self.pos += 1,
                Some('(') => return Err("methods/accessors not allowed in meta".into()),
                // `{ name }` shorthand reads a variable.
                Some(',' | '}') => return Err("non-literal node type in meta: Identifier".into()),
                _ => return Err("only plain properties allowed in meta".into()),
            }
            let value = self.value()?;
            map.insert(key, value);
            self.skip_trivia();
            if self.eat(',') {
                continue;
            }
            self.skip_trivia();
            if self.eat('}') {
                return Ok(Value::Object(map));
            }
            return Err("expected `,` or `}`".into());
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.eat('[');
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            if self.eat(']') {
                return Ok(Value::Array(items));
            }
            if self.peek() == Some(',') {
                return Err("sparse arrays not allowed".into());
            }
            if self.rest().starts_with("...") {
                return Err("spread not allowed in meta".into());
            }
            items.push(self.value()?);
            self.skip_trivia();
            if self.eat(',') {
                continue;
            }
            self.skip_trivia();
            if self.eat(']') {
                return Ok(Value::Array(items));
            }
            return Err("expected `,` or `]`".into());
        }
    }

    fn string(&mut self, quote: char) -> Result<String, String> {
        self.eat(quote);
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err("unterminated string".into());
            };
            self.pos += c.len_utf8();
            match c {
                c if c == quote => return Ok(out),
                '\n' => return Err("unterminated string".into()),
                '\\' => out.push(self.escape()?),
                c => out.push(c),
            }
        }
    }

    fn escape(&mut self) -> Result<char, String> {
        let Some(c) = self.peek() else {
            return Err("unterminated string".into());
        };
        self.pos += c.len_utf8();
        Ok(match c {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'v' => '\u{b}',
            '0' => '\0',
            'x' => self.hex(2)?,
            'u' => {
                if self.eat('{') {
                    let end = self.rest().find('}').ok_or("bad \\u escape")?;
                    let code = u32::from_str_radix(&self.rest()[..end], 16)
                        .map_err(|_| "bad \\u escape".to_string())?;
                    self.pos += end + 1;
                    char::from_u32(code).ok_or("bad \\u escape")?
                } else {
                    self.hex(4)?
                }
            }
            c => c,
        })
    }

    fn hex(&mut self, digits: usize) -> Result<char, String> {
        let text = self.rest().get(..digits).ok_or("bad escape")?;
        let code = u32::from_str_radix(text, 16).map_err(|_| "bad escape".to_string())?;
        self.pos += digits;
        char::from_u32(code).ok_or_else(|| "bad escape".to_string())
    }

    fn template(&mut self) -> Result<Value, String> {
        self.eat('`');
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err("unterminated template".into());
            };
            self.pos += c.len_utf8();
            match c {
                '`' => return Ok(Value::String(out)),
                '$' if self.peek() == Some('{') => {
                    return Err("template interpolation not allowed in meta".into());
                }
                '\\' => out.push(self.escape()?),
                c => out.push(c),
            }
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        self.eat('-');
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
        {
            // A sign belongs to the number only right after an exponent.
            let prev = self.src[start..self.pos].chars().last();
            let c = self.peek().unwrap_or(' ');
            if matches!(c, '+' | '-') && !matches!(prev, Some('e' | 'E')) {
                break;
            }
            self.pos += c.len_utf8();
        }
        let text = &self.src[start..self.pos];
        if text == "-" {
            return Err("only negative-number unary allowed in meta".into());
        }
        let parsed: f64 = if let Some(hex) = text.strip_prefix("0x") {
            u64::from_str_radix(hex, 16).map_err(|_| format!("bad number {text}"))? as f64
        } else {
            text.parse().map_err(|_| format!("bad number {text}"))?
        };
        if parsed.fract() == 0.0 && parsed.abs() < 9e15 {
            Ok(Value::Number(Number::from(parsed as i64)))
        } else {
            Number::from_f64(parsed)
                .map(Value::Number)
                .ok_or_else(|| format!("bad number {text}"))
        }
    }
}

/// `code` with the inside of strings, templates and comments blanked to
/// spaces, same length, so a search for `Date.now(` does not match a prompt
/// that mentions it.
pub fn blank_literals(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match (c, next) {
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                out.push_str("  ");
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    out.push(blank(chars[i]));
                    i += 1;
                }
                if i < chars.len() {
                    out.push_str("  ");
                    i += 2;
                }
            }
            ('"' | '\'' | '`', _) => {
                let quote = c;
                out.push(quote);
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' {
                        out.push(' ');
                        i += 1;
                        if i < chars.len() {
                            out.push(blank(chars[i]));
                            i += 1;
                        }
                        continue;
                    }
                    if quote != '`' && chars[i] == '\n' {
                        break;
                    }
                    out.push(blank(chars[i]));
                    i += 1;
                }
                if i < chars.len() && chars[i] == quote {
                    out.push(quote);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The message Claude gives for a script that reads the clock or rolls dice.
pub const NONDETERMINISM: &str = "workflow scripts must be deterministic: Date.now()/Math.random()/new Date() are unavailable (breaks resume). Stamp results after the workflow returns, or pass timestamps via args.";

/// What the body does that no workflow script may, found without running it.
pub fn forbidden_in_body(body: &str) -> Option<String> {
    let code = blank_literals(body);
    let hit = |pattern: &str| {
        regex::Regex::new(pattern)
            .map(|re| re.is_match(&code))
            .unwrap_or(false)
    };
    if hit(r"\bimport\s*\(") {
        return Some("import() is not available in workflow scripts.".into());
    }
    if hit(r"\bDate\s*\.\s*now\s*\(")
        || hit(r"\bMath\s*\.\s*random\s*\(")
        || hit(r"\bnew\s+Date\s*\(\s*\)")
    {
        return Some(NONDETERMINISM.into());
    }
    if hit(r"(?m)^\s*with\s*\(") {
        return Some("'with' statements are not supported in workflow scripts.".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"export const meta = {
  name: 'find-flaky-tests',
  description: "Find flaky tests",
  whenToUse: `when tests flake`,
  phases: [
    { title: 'Scan', detail: 'grep logs' }, // comment
    { title: 'Fix', model: 'x', },
  ],
}
phase('Scan')
return 1
"#;

    #[test]
    fn reads_meta_and_leaves_the_body() {
        let parsed = parse(GOOD).unwrap();
        assert_eq!(parsed.meta.name, "find-flaky-tests");
        assert_eq!(parsed.meta.description, "Find flaky tests");
        assert_eq!(parsed.meta.when_to_use.as_deref(), Some("when tests flake"));
        assert_eq!(parsed.meta.phases.len(), 2);
        assert_eq!(parsed.meta.phases[0].detail.as_deref(), Some("grep logs"));
        assert_eq!(parsed.meta.phases[1].model.as_deref(), Some("x"));
        assert!(parsed.body.starts_with("phase('Scan')"));
    }

    #[test]
    fn a_leading_comment_and_a_semicolon_are_fine() {
        let parsed = parse(
            "// header\n/* more */ export const meta = {name:'a',description:'b'};\nreturn 2",
        )
        .unwrap();
        assert_eq!(parsed.body, "return 2");
    }

    #[test]
    fn meta_must_come_first() {
        let err = parse("const x = 1\nexport const meta = {name:'a',description:'b'}").unwrap_err();
        assert!(err.contains("must be the FIRST statement"), "{err}");
        assert!(parse("").unwrap_err().contains("FIRST statement"));
    }

    #[test]
    fn meta_must_be_a_pure_literal() {
        let cases = [
            (
                "{name: NAME, description: 'd'}",
                "non-literal node type in meta",
            ),
            ("{name, description: 'd'}", "non-literal node type in meta"),
            (
                "{name: `a${1}`, description: 'd'}",
                "template interpolation not allowed in meta",
            ),
            (
                "{...x, name: 'a', description: 'd'}",
                "spread not allowed in meta",
            ),
            (
                "{[k]: 1, name: 'a', description: 'd'}",
                "computed keys not allowed in meta",
            ),
            (
                "{name: 'a', description: 'd', f() {}}",
                "methods/accessors not allowed in meta",
            ),
            (
                "{name: 'a', description: 'd', p: [1,,2]}",
                "sparse arrays not allowed",
            ),
            (
                "{name: 'a', description: f()}",
                "non-literal node type in meta",
            ),
        ];
        for (literal, want) in cases {
            let err = parse(&format!("export const meta = {literal}\n")).unwrap_err();
            assert!(
                err.starts_with("meta must be a pure literal: "),
                "{literal}: {err}"
            );
            assert!(err.contains(want), "{literal}: {err}");
        }
    }

    #[test]
    fn name_and_description_are_required_non_empty_strings() {
        let err = parse("export const meta = {description: 'd'}").unwrap_err();
        assert_eq!(err, "meta.name must be a non-empty string");
        let err = parse("export const meta = {name: 'n', description: ''}").unwrap_err();
        assert_eq!(err, "meta.description must be a non-empty string");
    }

    #[test]
    fn numbers_and_escapes_read_like_javascript() {
        let parsed = parse(
            "export const meta = {name: 'a\\n\\u0041', description: 'd', n: -1.5e1, h: 0x10}\n",
        )
        .unwrap();
        assert_eq!(parsed.meta.name, "a\nA");
    }

    #[test]
    fn control_characters_are_refused() {
        let err = parse("export const meta = {name:'a',description:'b'}\n\u{1b}[31m").unwrap_err();
        assert!(err.contains("disallowed control characters"), "{err}");
    }

    #[test]
    fn blanking_keeps_length_and_hides_literals() {
        let code = "a('Date.now()') // Math.random()\n/* new Date() */ b(`x\ny`)";
        let blanked = blank_literals(code);
        assert_eq!(blanked.chars().count(), code.chars().count());
        assert!(!blanked.contains("Date"));
        assert!(!blanked.contains("random"));
        assert!(blanked.contains("a(") && blanked.contains("b("));
    }

    #[test]
    fn the_body_may_not_read_the_clock_or_import() {
        assert_eq!(
            forbidden_in_body("const t = Date.now()").as_deref(),
            Some(NONDETERMINISM)
        );
        assert_eq!(
            forbidden_in_body("x = Math.random()").as_deref(),
            Some(NONDETERMINISM)
        );
        assert_eq!(
            forbidden_in_body("x = new Date()").as_deref(),
            Some(NONDETERMINISM)
        );
        assert!(forbidden_in_body("x = new Date(args.t)").is_none());
        assert!(
            forbidden_in_body("await import('fs')")
                .unwrap()
                .starts_with("import()")
        );
        assert!(forbidden_in_body("agent('use Date.now() here')").is_none());
        assert!(forbidden_in_body("// Math.random()\nreturn 1").is_none());
    }
}
