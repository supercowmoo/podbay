//! Just enough JSON (RFC 8259): the vendored seccomp profile, which podbay
//! edits and writes back, and the engine's `image inspect` output.
//!
//! Objects keep their keys in order, and numbers keep the exact text they
//! were written with, so a document comes back out as it went in.

use std::fmt::Write as _;
use std::ops::{Index, IndexMut};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// A number exactly as written, already checked to be valid JSON.
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

static NULL: Value = Value::Null;

/// Nesting deeper than this is refused rather than risking the stack.
const MAX_DEPTH: usize = 128;

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Self::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Self::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Vec<(String, Value)>> {
        match self {
            Self::Object(o) => Some(o),
            _ => None,
        }
    }

    /// A list of strings, as `image inspect` gives an entrypoint or
    /// command. `null` is an empty list.
    pub fn as_strings(&self) -> Option<Vec<String>> {
        match self {
            Self::Null => Some(Vec::new()),
            Self::Array(items) => items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => None,
        }
    }
}

impl Index<&str> for Value {
    type Output = Value;
    /// The member `key`, or null when there's none or this isn't an object.
    fn index(&self, key: &str) -> &Value {
        match self {
            Self::Object(o) => o.iter().find(|(k, _)| k == key).map_or(&NULL, |(_, v)| v),
            _ => &NULL,
        }
    }
}

impl IndexMut<&str> for Value {
    /// The member `key`, added as null if missing.
    ///
    /// # Panics
    /// If this isn't an object.
    fn index_mut(&mut self, key: &str) -> &mut Value {
        let Self::Object(o) = self else {
            panic!("indexing {self:?} with {key:?}");
        };
        let i = match o.iter().position(|(k, _)| k == key) {
            Some(i) => i,
            None => {
                o.push((key.to_owned(), Value::Null));
                o.len() - 1
            }
        };
        &mut o[i].1
    }
}

impl PartialEq<&str> for Value {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == Some(*other)
    }
}

impl PartialEq<str> for Value {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == Some(other)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

/// Parses a complete JSON document.
pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.whitespace();
    let value = p.value(0)?;
    p.whitespace();
    if p.pos != p.bytes.len() {
        return Err(p.error("unexpected text after the document"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        format!("invalid JSON at byte {}: {what}", self.pos)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("unknown word"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, String> {
        self.pos += 1;
        let mut members = Vec::new();
        self.whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a member name"));
            }
            let key = self.string()?;
            self.whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("expected ':'"));
            }
            self.pos += 1;
            self.whitespace();
            let value = self.value(depth + 1)?;
            members.push((key, value));
            self.whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.whitespace();
            items.push(self.value(depth + 1)?);
            self.whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        let digits = |p: &mut Self| {
            let from = p.pos;
            while matches!(p.peek(), Some(b'0'..=b'9')) {
                p.pos += 1;
            }
            p.pos > from
        };
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(self.error("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !digits(self) {
                return Err(self.error("expected a digit after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digits(self) {
                return Err(self.error("expected a digit in the exponent"));
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).expect("ASCII");
        Ok(Value::Number(text.to_owned()))
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1; // the opening quote
        let mut out = String::new();
        loop {
            let start = self.pos;
            while !matches!(self.peek(), Some(b'"' | b'\\') | None) {
                if self.bytes[self.pos] < 0x20 {
                    return Err(self.error("control character in a string"));
                }
                self.pos += 1;
            }
            // The input is a &str, so whole runs between quotes and escapes
            // are valid UTF-8.
            out.push_str(std::str::from_utf8(&self.bytes[start..self.pos]).expect("UTF-8"));
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = self.peek().ok_or_else(|| self.error("unexpected end"))?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.error("unknown escape")),
                    }
                }
                _ => return Err(self.error("unterminated string")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| self.error("expected four hex digits"))?;
        self.pos += 4;
        Ok(u32::from_str_radix(digits, 16).expect("hex digits"))
    }

    /// After `\u`: one code point, from a surrogate pair if it takes two.
    fn unicode_escape(&mut self) -> Result<char, String> {
        let first = self.hex4()?;
        let code = match first {
            0xd800..=0xdbff => {
                if !self.bytes[self.pos..].starts_with(b"\\u") {
                    return Err(self.error("unpaired surrogate"));
                }
                self.pos += 2;
                let second = self.hex4()?;
                if !(0xdc00..=0xdfff).contains(&second) {
                    return Err(self.error("unpaired surrogate"));
                }
                0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
            }
            0xdc00..=0xdfff => return Err(self.error("unpaired surrogate")),
            _ => first,
        };
        char::from_u32(code).ok_or_else(|| self.error("invalid code point"))
    }
}

/// Compact JSON, members in their order.
pub fn to_string(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(n),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(members) => {
            out.push('{');
            for (i, (key, item)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A JSON array of strings, such as a Containerfile's `CMD ["a", "b"]`.
pub fn string_array(items: &[String]) -> String {
    to_string(&Value::Array(
        items.iter().map(|s| Value::String(s.clone())).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for doc in [
            "null",
            "true",
            "[]",
            "{}",
            r#"{"a":[1,-2.5e10,0,0.25,1E+2],"b":{"c":null,"d":false},"e":"x"}"#,
            r#"["tab\tquote\"back\\slash\nnl","\u0001","é😀"]"#,
        ] {
            let value = parse(doc).unwrap();
            assert_eq!(parse(&to_string(&value)).unwrap(), value, "{doc}");
        }
        // Numbers keep their exact spelling.
        assert_eq!(to_string(&parse("[1E+2,0.10]").unwrap()), "[1E+2,0.10]");
    }

    #[test]
    fn escapes() {
        assert_eq!(parse(r#""é😀\/\b\f""#).unwrap(), "é😀/\u{8}\u{c}");
        assert_eq!(to_string(&Value::from("a\u{1}b")), r#""a\u0001b""#);
        for bad in [r#""\ud83d""#, r#""\ude00""#, r#""\x""#, r#""\u12""#] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_what_json_does() {
        for bad in [
            "",
            " ",
            "[",
            "]",
            "{",
            r#"{"a"}"#,
            r#"{"a":1,}"#,
            "[1,]",
            "01",
            "-",
            "1.",
            "1e",
            ".5",
            "+1",
            "nul",
            "tru",
            "'a'",
            "[1] 2",
            "\"a\nb\"",
            "NaN",
            "{a:1}",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} accepted");
        }
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn access() {
        let mut v = parse(r#"{"names":["a","b"],"action":"X","n":null}"#).unwrap();
        assert_eq!(v["action"], "X");
        assert_eq!(v["missing"], Value::Null);
        assert_eq!(v["names"]["nested"], Value::Null);
        assert_eq!(v["n"].as_strings(), Some(vec![]));
        assert_eq!(
            v["names"].as_strings(),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        v["names"].as_array_mut().unwrap().retain(|n| *n != "a");
        v["new"] = Value::from("y");
        assert_eq!(
            to_string(&v),
            r#"{"names":["b"],"action":"X","n":null,"new":"y"}"#
        );
        assert_eq!(string_array(&["a\"b".into()]), r#"["a\"b"]"#);
    }
}
