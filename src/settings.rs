//! Reading `config.toml` and profiles: the `toml` crate parses them into a
//! tree with source positions, and this module reads typed values out of
//! it, so no serialization framework or code generation is needed.
//!
//! It is strict in the same ways as before: unknown keys and wrong types
//! are errors, reported like TOML syntax errors, with the line, the column
//! and the offending text marked.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

use toml::Spanned;
use toml::de::{DeTable, DeValue};

/// A problem at a place in the file, rendered for people.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// A parsed file and its text, for positions in errors.
pub struct Doc<'i> {
    text: &'i str,
    root: Spanned<DeTable<'i>>,
}

/// Parses TOML. Syntax errors, duplicate keys included, come from the
/// `toml` crate with their position.
pub fn parse(text: &str) -> Result<Doc<'_>> {
    match DeTable::parse(text) {
        Ok(root) => Ok(Doc { text, root }),
        Err(mut e) => {
            e.set_input(Some(text));
            Err(Error(e.to_string()))
        }
    }
}

impl<'i> Doc<'i> {
    pub fn root(&self) -> Table<'_, 'i> {
        Table {
            text: self.text,
            table: self.root.get_ref(),
            span: self.root.span(),
        }
    }
}

/// `message` with the place `span` points at, as the `toml` crate shows
/// its own errors.
fn at(text: &str, span: Range<usize>, message: &str) -> Error {
    let start = span.start.min(text.len());
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let line_no = text[..start].matches('\n').count() + 1;
    let column = text[line_start..start].chars().count() + 1;
    let end = span.end.clamp(start, line_end);
    let width = text[start..end].chars().count().max(1);
    let pad = " ".repeat(line_no.to_string().len());
    Error(format!(
        "TOML parse error at line {line_no}, column {column}\n\
         {pad} |\n\
         {line_no} | {}\n\
         {pad} | {}{}\n\
         {message}\n",
        &text[line_start..line_end],
        " ".repeat(column - 1),
        "^".repeat(width),
    ))
}

/// One value in the file, with where it is.
#[derive(Clone, Copy)]
pub struct Value<'a, 'i> {
    text: &'a str,
    value: &'a Spanned<DeValue<'i>>,
}

impl<'a, 'i> Value<'a, 'i> {
    /// An error pointing at this value.
    pub fn error(&self, message: &str) -> Error {
        at(self.text, self.value.span(), message)
    }

    /// "invalid type: integer `1`, expected a string", pointing here.
    pub fn invalid_type(&self, expected: &str) -> Error {
        let found = match self.value.get_ref() {
            DeValue::String(s) => format!("string {s:?}"),
            DeValue::Integer(i) => format!("integer `{i}`"),
            DeValue::Float(f) => format!("floating point `{f}`"),
            DeValue::Boolean(b) => format!("boolean `{b}`"),
            DeValue::Datetime(d) => format!("datetime `{d}`"),
            DeValue::Array(_) => "sequence".to_owned(),
            DeValue::Table(_) => "map".to_owned(),
        };
        self.error(&format!("invalid type: {found}, expected {expected}"))
    }

    pub fn as_str(&self) -> Option<&'a str> {
        self.value.get_ref().as_str()
    }

    pub fn string(&self) -> Result<String> {
        self.as_str()
            .map(str::to_owned)
            .ok_or_else(|| self.invalid_type("a string"))
    }

    pub fn as_array(&self) -> Option<Vec<Value<'a, 'i>>> {
        match self.value.get_ref() {
            DeValue::Array(items) => Some(
                items
                    .iter()
                    .map(|value| Value {
                        text: self.text,
                        value,
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    pub fn strings(&self) -> Result<Vec<String>> {
        self.as_array()
            .ok_or_else(|| self.invalid_type("a sequence"))?
            .iter()
            .map(Value::string)
            .collect()
    }

    /// An integer, if this is one that fits in an i64.
    pub fn as_integer(&self) -> Option<i64> {
        match self.value.get_ref() {
            DeValue::Integer(i) => i64::from_str_radix(i.as_str(), i.radix()).ok(),
            _ => None,
        }
    }

    pub fn is_integer(&self) -> bool {
        matches!(self.value.get_ref(), DeValue::Integer(_))
    }

    pub fn as_table(&self) -> Option<Table<'a, 'i>> {
        match self.value.get_ref() {
            DeValue::Table(table) => Some(Table {
                text: self.text,
                table,
                span: self.value.span(),
            }),
            _ => None,
        }
    }
}

/// One table in the file.
#[derive(Clone)]
pub struct Table<'a, 'i> {
    text: &'a str,
    table: &'a DeTable<'i>,
    span: Range<usize>,
}

/// "`a`, `b` or `c`" for error messages.
fn one_of(known: &[&str]) -> String {
    let quoted: Vec<String> = known.iter().map(|k| format!("`{k}`")).collect();
    match quoted.len() {
        0 => "nothing".to_owned(),
        1 => quoted[0].clone(),
        2 => format!("{} or {}", quoted[0], quoted[1]),
        _ => format!("one of {}", quoted.join(", ")),
    }
}

impl<'a, 'i> Table<'a, 'i> {
    /// Refuses keys other than `known`, pointing at the first one.
    pub fn only(&self, known: &[&str]) -> Result<()> {
        for (key, _) in self.table.iter() {
            let name: &str = key.get_ref();
            if !known.contains(&name) {
                return Err(at(
                    self.text,
                    key.span(),
                    &format!("unknown field `{name}`, expected {}", one_of(known)),
                ));
            }
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<Value<'a, 'i>> {
        self.table.get(key).map(|value| Value {
            text: self.text,
            value,
        })
    }

    pub fn has(&self, key: &str) -> bool {
        self.table.get(key).is_some()
    }

    fn missing(&self, key: &str) -> Error {
        at(
            self.text,
            self.span.clone(),
            &format!("missing field `{key}`"),
        )
    }

    pub fn string(&self, key: &str) -> Result<Option<String>> {
        self.get(key).map(|v| v.string()).transpose()
    }

    pub fn required_string(&self, key: &str) -> Result<String> {
        self.string(key)?.ok_or_else(|| self.missing(key))
    }

    pub fn bool(&self, key: &str) -> Result<Option<bool>> {
        self.get(key)
            .map(|v| match v.value.get_ref() {
                DeValue::Boolean(b) => Ok(*b),
                _ => Err(v.invalid_type("a boolean")),
            })
            .transpose()
    }

    /// A list of strings; absent is empty.
    pub fn strings(&self, key: &str) -> Result<Vec<String>> {
        Ok(self
            .get(key)
            .map(|v| v.strings())
            .transpose()?
            .unwrap_or_default())
    }

    pub fn u32(&self, key: &str) -> Result<Option<u32>> {
        self.get(key)
            .map(|v| {
                if !v.is_integer() {
                    return Err(v.invalid_type("u32"));
                }
                v.as_integer()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| {
                        v.error("invalid value: expected a whole number from 0 to 4294967295")
                    })
            })
            .transpose()
    }

    /// A number, whole or not.
    pub fn f64(&self, key: &str) -> Result<Option<f64>> {
        self.get(key)
            .map(|v| match v.value.get_ref() {
                DeValue::Float(f) => f
                    .as_str()
                    .parse::<f64>()
                    .map_err(|_| v.error("invalid number")),
                DeValue::Integer(_) => v
                    .as_integer()
                    .map(|n| n as f64)
                    .ok_or_else(|| v.error("invalid number")),
                _ => Err(v.invalid_type("f64")),
            })
            .transpose()
    }

    pub fn table(&self, key: &str) -> Result<Option<Table<'a, 'i>>> {
        self.get(key)
            .map(|v| v.as_table().ok_or_else(|| v.invalid_type("a map")))
            .transpose()
    }

    /// An array of tables, `[[key]]`; absent is empty.
    pub fn tables(&self, key: &str) -> Result<Vec<Table<'a, 'i>>> {
        let Some(v) = self.get(key) else {
            return Ok(Vec::new());
        };
        v.as_array()
            .ok_or_else(|| v.invalid_type("a sequence"))?
            .iter()
            .map(|item| item.as_table().ok_or_else(|| item.invalid_type("a map")))
            .collect()
    }

    /// Every key with its value, in file order.
    pub fn entries(&self) -> Vec<(String, Value<'a, 'i>)> {
        self.table
            .iter()
            .map(|(k, v)| {
                (
                    k.get_ref().to_string(),
                    Value {
                        text: self.text,
                        value: v,
                    },
                )
            })
            .collect()
    }

    /// A table of strings, such as `[env]`; absent is empty.
    pub fn string_map(&self, key: &str) -> Result<BTreeMap<String, String>> {
        let Some(table) = self.table(key)? else {
            return Ok(BTreeMap::new());
        };
        table
            .table
            .iter()
            .map(|(k, v)| {
                let value = Value {
                    text: self.text,
                    value: v,
                };
                Ok((k.get_ref().to_string(), value.string()?))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str, read: impl Fn(Table) -> Result<()>) -> String {
        let doc = parse(text).unwrap();
        read(doc.root()).unwrap_err().to_string()
    }

    #[test]
    fn errors_point_at_the_problem() {
        assert_eq!(
            err("shell = \"zsh\"\nmode = \"wr\"\n", |t| t.only(&["shell"])),
            "TOML parse error at line 2, column 1\n  |\n2 | mode = \"wr\"\n  | ^^^^\n\
             unknown field `mode`, expected `shell`\n"
        );
        assert!(
            err("image = 3\n", |t| t.string("image").map(drop))
                .ends_with("  |         ^\ninvalid type: integer `3`, expected a string\n")
        );
        assert!(
            err("x = 1\n", |t| t.only(&["a", "b", "c"]))
                .contains("unknown field `x`, expected one of `a`, `b`, `c`")
        );
        let e = err("[provision]\nwatch = []\n", |t| {
            t.table("provision")?
                .unwrap()
                .required_string("command")
                .map(drop)
        });
        assert!(
            e.contains("line 1, column 1") && e.contains("missing field `command`"),
            "{e}"
        );
        // Wide line numbers widen the gutter.
        let text = "\n".repeat(11) + "x = 1\n";
        assert!(err(&text, |t| t.only(&[])).contains("   |\n12 | x = 1\n   | ^\n"));
    }

    #[test]
    fn syntax_errors_come_from_toml() {
        let Err(e) = parse("shell = \"a\"\nshell = \"b\"\n") else {
            panic!()
        };
        assert!(e.to_string().contains("duplicate key"), "{e}");
        assert!(parse("shell = \"zsh\n").is_err());
    }

    #[test]
    fn values() {
        let doc = parse(
            "n = 0x10\nf = 1.5\ni = 4\nbig = 5000000000\nneg = -1\nb = true\n\
             s = [\"a\", \"b\"]\n[env]\nK = \"v\"\n[[m]]\nx = 1\n[[m]]\nx = 2\n",
        )
        .unwrap();
        let t = doc.root();
        assert_eq!(t.u32("n").unwrap(), Some(16));
        assert_eq!(t.f64("f").unwrap(), Some(1.5));
        assert_eq!(t.f64("i").unwrap(), Some(4.0));
        assert!(t.u32("big").is_err());
        assert!(t.u32("neg").is_err());
        assert!(t.u32("f").is_err());
        assert_eq!(t.bool("b").unwrap(), Some(true));
        assert!(t.bool("n").is_err());
        assert_eq!(t.strings("s").unwrap(), ["a", "b"]);
        assert_eq!(t.strings("absent").unwrap(), Vec::<String>::new());
        assert!(t.strings("n").is_err());
        assert_eq!(t.string_map("env").unwrap()["K"], "v");
        assert_eq!(t.tables("m").unwrap().len(), 2);
        assert!(t.tables("env").is_err());
        assert!(t.has("b") && !t.has("absent"));
    }
}
