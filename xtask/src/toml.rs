//! The scenario-file reader: a deliberately small subset of TOML.
//!
//! A real TOML crate would be a new dependency of the acceptance gate, and the
//! gate is the one place in this repository where "fewer moving parts" beats
//! "more features". The scenario files are written by hand against this
//! grammar, so the subset is chosen to be exactly what they use:
//!
//! * `key = value` where a value is a string, integer, float, boolean, or an
//!   array of values;
//! * basic strings with the usual escapes, literal `'…'` strings, and `"""`
//!   multi-line strings (which is how a JSON payload is inlined);
//! * `[table]` and `[[array of tables]]`, nested at most one level
//!   (`[[route.reply]]`).
//!
//! Anything else is a parse error rather than a silent misreading — an
//! acceptance gate that quietly ignores a mistyped key reports green for the
//! wrong reason, which is the failure mode this whole crate exists to prevent.

use std::fmt;

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<Value>),
}

impl Value {
    fn type_name(&self) -> &'static str {
        match self {
            Self::Str(_) => "string",
            Self::Int(_) => "integer",
            Self::Float(_) => "float",
            Self::Bool(_) => "boolean",
            Self::Array(_) => "array",
        }
    }
}

/// A table, keeping insertion order so that error messages can name keys in the
/// order the author wrote them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Table {
    pairs: Vec<(String, Value)>,
    subs: Vec<(String, Vec<Table>)>,
}

impl Table {
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        self.pairs.iter().find(|(name, _)| name == key).map(|(_, value)| value)
    }

    /// Every `[[key]]` entry, in file order.
    pub(crate) fn tables(&self, key: &str) -> &[Table] {
        self.subs
            .iter()
            .find(|(name, _)| name == key)
            .map_or(&[][..], |(_, tables)| tables.as_slice())
    }

    /// A single `[key]` table.
    pub(crate) fn table(&self, key: &str) -> Option<&Table> {
        self.tables(key).first()
    }

    pub(crate) fn str_opt(&self, key: &str) -> Result<Option<&str>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s.as_str())),
            Some(other) => bail!("`{key}` must be a string, not a {}", other.type_name()),
        }
    }

    pub(crate) fn string(&self, key: &str, default: &str) -> Result<String> {
        Ok(self.str_opt(key)?.unwrap_or(default).to_owned())
    }

    pub(crate) fn required_string(&self, key: &str) -> Result<String> {
        match self.str_opt(key)? {
            Some(value) => Ok(value.to_owned()),
            None => bail!("`{key}` is required"),
        }
    }

    pub(crate) fn int(&self, key: &str, default: i64) -> Result<i64> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Int(n)) => Ok(*n),
            Some(other) => bail!("`{key}` must be an integer, not a {}", other.type_name()),
        }
    }

    pub(crate) fn boolean(&self, key: &str, default: bool) -> Result<bool> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => bail!("`{key}` must be a boolean, not a {}", other.type_name()),
        }
    }

    pub(crate) fn str_array(&self, key: &str) -> Result<Vec<String>> {
        let Some(value) = self.get(key) else { return Ok(Vec::new()) };
        let Value::Array(items) = value else {
            bail!("`{key}` must be an array, not a {}", value.type_name());
        };
        items
            .iter()
            .map(|item| match item {
                Value::Str(s) => Ok(s.clone()),
                other => bail!("`{key}` must hold strings, found a {}", other.type_name()),
            })
            .collect()
    }

    /// `key = [["a", "b"], ["c", "d"]]` — used for header lists, where order and
    /// duplicates both matter and a table would destroy both.
    pub(crate) fn pair_array(&self, key: &str) -> Result<Vec<(String, String)>> {
        let Some(value) = self.get(key) else { return Ok(Vec::new()) };
        let Value::Array(rows) = value else {
            bail!("`{key}` must be an array, not a {}", value.type_name());
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let Value::Array(cells) = row else {
                bail!("`{key}` must hold two-element arrays, found a {}", row.type_name());
            };
            match cells.as_slice() {
                [Value::Str(name), Value::Str(v)] => out.push((name.clone(), v.clone())),
                _ => bail!("`{key}` entries must be exactly [\"name\", \"value\"]"),
            }
        }
        Ok(out)
    }

    /// Every `key = "value"` pair, for tables whose keys are data (`[env]`).
    pub(crate) fn string_pairs(&self) -> Result<Vec<(String, String)>> {
        self.pairs
            .iter()
            .map(|(name, value)| match value {
                Value::Str(s) => Ok((name.clone(), s.clone())),
                other => bail!("`{name}` must be a string, not a {}", other.type_name()),
            })
            .collect()
    }

    /// Rejects a key the reader would otherwise ignore.
    ///
    /// A scenario with `conccurency = 3` that silently ran at the default would
    /// report a green that means nothing.
    pub(crate) fn reject_unknown(&self, keys: &[&str], sub_keys: &[&str]) -> Result<()> {
        for (name, _) in &self.pairs {
            if !keys.contains(&name.as_str()) {
                bail!("unknown key `{name}` (known: {})", keys.join(", "));
            }
        }
        for (name, _) in &self.subs {
            if !sub_keys.contains(&name.as_str()) {
                bail!("unknown table `[{name}]` (known: {})", sub_keys.join(", "));
            }
        }
        Ok(())
    }
}

/// Where a table header points. Only the two depths the scenarios use exist.
#[derive(Clone, Copy, Debug)]
enum Cursor {
    Root,
    Sub(usize),
    SubSub(usize, usize),
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

pub(crate) fn parse(text: &str) -> Result<Table> {
    let mut parser = Parser { src: text.as_bytes(), pos: 0 };
    let mut root = Table::default();
    let mut cursor = Cursor::Root;

    loop {
        parser.skip_trivia();
        if parser.pos >= parser.src.len() {
            break;
        }
        if parser.peek() == b'[' {
            let (path, array) = parser.header()?;
            cursor = install(&mut root, &path, array)
                .map_err(|error| anyhow::anyhow!("{}", parser.at(&error.to_string())))?;
            continue;
        }
        let key = parser.key()?;
        parser.skip_inline_trivia();
        if parser.peek() != b'=' {
            bail!("{}", parser.at(&format!("expected `=` after `{key}`")));
        }
        parser.pos += 1;
        let value = parser.value()?;
        table_at(&mut root, cursor).pairs.push((key, value));
    }
    Ok(root)
}

/// Resolves `[a]` / `[[a]]` / `[[a.b]]` to the table new keys land in.
fn install(root: &mut Table, path: &[String], array: bool) -> Result<Cursor> {
    match path {
        [first] => {
            let index = sub_index(root, first);
            if array || root.subs[index].1.is_empty() {
                root.subs[index].1.push(Table::default());
            }
            Ok(Cursor::Sub(index))
        }
        [first, second] => {
            if !array {
                bail!("`[{first}.{second}]` must be written `[[{first}.{second}]]`");
            }
            let outer = sub_index(root, first);
            let Some(parent) = root.subs[outer].1.last_mut() else {
                bail!("`[[{first}.{second}]]` appeared before any `[[{first}]]`");
            };
            let inner = sub_index(parent, second);
            parent.subs[inner].1.push(Table::default());
            let last = parent.subs[inner].1.len() - 1;
            let _ = last;
            Ok(Cursor::SubSub(outer, inner))
        }
        _ => bail!("table headers may nest at most two deep"),
    }
}

fn sub_index(table: &mut Table, name: &str) -> usize {
    if let Some(index) = table.subs.iter().position(|(existing, _)| existing == name) {
        return index;
    }
    table.subs.push((name.to_owned(), Vec::new()));
    table.subs.len() - 1
}

fn table_at(root: &mut Table, cursor: Cursor) -> &mut Table {
    match cursor {
        Cursor::Root => root,
        // Every cursor was produced by `install`, which created what it names.
        Cursor::Sub(index) => root.subs[index].1.last_mut().expect("table exists"),
        Cursor::SubSub(outer, inner) => {
            let parent = root.subs[outer].1.last_mut().expect("table exists");
            parent.subs[inner].1.last_mut().expect("table exists")
        }
    }
}

impl Parser<'_> {
    fn peek(&self) -> u8 {
        self.src.get(self.pos).copied().unwrap_or(0)
    }

    fn peek_at(&self, offset: usize) -> u8 {
        self.src.get(self.pos + offset).copied().unwrap_or(0)
    }

    fn line(&self) -> usize {
        // Segments, not separators: an empty prefix is one segment, which is
        // line 1.
        self.src[..self.pos.min(self.src.len())].split(|&byte| byte == b'\n').count()
    }

    fn at(&self, message: &str) -> String {
        format!("line {}: {message}", self.line())
    }

    /// Whitespace, newlines and comments.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                b'#' => {
                    while self.pos < self.src.len() && self.peek() != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// Whitespace within a line only.
    fn skip_inline_trivia(&mut self) {
        while matches!(self.peek(), b' ' | b'\t') {
            self.pos += 1;
        }
    }

    fn header(&mut self) -> Result<(Vec<String>, bool)> {
        let array = self.peek_at(1) == b'[';
        self.pos += if array { 2 } else { 1 };
        let start = self.pos;
        while self.pos < self.src.len() && self.peek() != b']' && self.peek() != b'\n' {
            self.pos += 1;
        }
        if self.peek() != b']' {
            bail!("{}", self.at("unterminated table header"));
        }
        let path = std::str::from_utf8(&self.src[start..self.pos])?.trim_matches(' ').to_owned();
        self.pos += 1;
        if array {
            if self.peek() != b']' {
                bail!("{}", self.at("expected `]]`"));
            }
            self.pos += 1;
        }
        if path.is_empty() {
            bail!("{}", self.at("empty table header"));
        }
        Ok((path.split('.').map(|part| part.trim_matches(' ').to_owned()).collect(), array))
    }

    fn key(&mut self) -> Result<String> {
        let start = self.pos;
        while matches!(self.peek(), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-') {
            self.pos += 1;
        }
        if self.pos == start {
            bail!("{}", self.at(&format!("expected a key, found {:?}", self.peek() as char)));
        }
        Ok(String::from_utf8(self.src[start..self.pos].to_vec())?)
    }

    fn value(&mut self) -> Result<Value> {
        self.skip_trivia();
        match self.peek() {
            b'"' => {
                if self.peek_at(1) == b'"' && self.peek_at(2) == b'"' {
                    self.multiline_string()
                } else {
                    self.basic_string()
                }
            }
            b'\'' => self.literal_string(),
            b'[' => self.array(),
            b't' | b'f' => self.boolean(),
            _ => self.number(),
        }
    }

    fn basic_string(&mut self) -> Result<Value> {
        self.pos += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            match self.peek() {
                0 if self.pos >= self.src.len() => bail!("{}", self.at("unterminated string")),
                b'\n' => bail!("{}", self.at("newline inside a single-line string")),
                b'"' => {
                    self.pos += 1;
                    return Ok(Value::Str(String::from_utf8(out)?));
                }
                b'\\' => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                byte => {
                    out.push(byte);
                    self.pos += 1;
                }
            }
        }
    }

    fn multiline_string(&mut self) -> Result<Value> {
        self.pos += 3;
        // TOML drops a newline immediately after the opening delimiter, which is
        // what lets an inlined JSON payload start on its own line.
        if self.peek() == b'\r' {
            self.pos += 1;
        }
        if self.peek() == b'\n' {
            self.pos += 1;
        }
        let mut out: Vec<u8> = Vec::new();
        loop {
            if self.pos >= self.src.len() {
                bail!("{}", self.at("unterminated multi-line string"));
            }
            if self.peek() == b'"' && self.peek_at(1) == b'"' && self.peek_at(2) == b'"' {
                self.pos += 3;
                return Ok(Value::Str(String::from_utf8(out)?));
            }
            if self.peek() == b'\\' {
                self.pos += 1;
                self.escape(&mut out)?;
                continue;
            }
            out.push(self.peek());
            self.pos += 1;
        }
    }

    fn literal_string(&mut self) -> Result<Value> {
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.src.len() && self.peek() != b'\'' && self.peek() != b'\n' {
            self.pos += 1;
        }
        if self.peek() != b'\'' {
            bail!("{}", self.at("unterminated literal string"));
        }
        let text = String::from_utf8(self.src[start..self.pos].to_vec())?;
        self.pos += 1;
        Ok(Value::Str(text))
    }

    fn escape(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let code = self.peek();
        self.pos += 1;
        match code {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'0' => out.push(0),
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'u' => {
                let hex = std::str::from_utf8(
                    self.src.get(self.pos..self.pos + 4).unwrap_or_default(),
                )?;
                let scalar = u32::from_str_radix(hex, 16)?;
                self.pos += 4;
                let ch = char::from_u32(scalar)
                    .ok_or_else(|| anyhow::anyhow!("\\u{hex} is not a scalar value"))?;
                let mut buffer = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
            }
            other => bail!("{}", self.at(&format!("unknown escape \\{}", other as char))),
        }
        Ok(())
    }

    fn array(&mut self) -> Result<Value> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            if self.peek() == b']' {
                self.pos += 1;
                return Ok(Value::Array(items));
            }
            if self.pos >= self.src.len() {
                bail!("{}", self.at("unterminated array"));
            }
            items.push(self.value()?);
            self.skip_trivia();
            if self.peek() == b',' {
                self.pos += 1;
            }
        }
    }

    fn boolean(&mut self) -> Result<Value> {
        if self.src[self.pos..].starts_with(b"true") {
            self.pos += 4;
            return Ok(Value::Bool(true));
        }
        if self.src[self.pos..].starts_with(b"false") {
            self.pos += 5;
            return Ok(Value::Bool(false));
        }
        bail!("{}", self.at("expected a value"))
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.pos;
        if matches!(self.peek(), b'-' | b'+') {
            self.pos += 1;
        }
        let mut float = false;
        while matches!(self.peek(), b'0'..=b'9' | b'.' | b'e' | b'E' | b'-' | b'+') {
            float |= matches!(self.peek(), b'.' | b'e' | b'E');
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.src[start..self.pos])?;
        if text.is_empty() {
            bail!("{}", self.at("expected a value"));
        }
        if float {
            Ok(Value::Float(text.parse()?))
        } else {
            Ok(Value::Int(text.parse()?))
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Str(s) => f.write_str(s),
            Self::Int(n) => write!(f, "{n}"),
            // No `{}` on an `f64` anywhere near the parity output; scenario
            // floats exist only to be read back as milliseconds.
            Self::Float(v) => write!(f, "{}", edm_core::js::js_number(*v)),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Array(items) => {
                f.write_str("[")?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_shapes_the_scenarios_use() {
        let doc = parse(
            r#"
            name = "demo"
            argv = ["market", "--json"]
            live = true
            [env]
            MARKET_ID = "7"
            [[route]]
            path = "/2.0/elite/market/list"
            [[route.reply]]
            status = 200
            headers = [["nonce", "aabbccddeeff"]]
            [[route.reply]]
            status = 500
            [[route]]
            path = "/upload/"
            "#,
        )
        .unwrap();

        assert_eq!(doc.string("name", "").unwrap(), "demo");
        assert_eq!(doc.str_array("argv").unwrap(), ["market", "--json"]);
        assert!(doc.boolean("live", false).unwrap());
        assert_eq!(
            doc.table("env").unwrap().string_pairs().unwrap(),
            [("MARKET_ID".to_owned(), "7".to_owned())]
        );
        let routes = doc.tables("route");
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].tables("reply").len(), 2);
        assert_eq!(routes[0].tables("reply")[1].int("status", 0).unwrap(), 500);
        assert_eq!(
            routes[0].tables("reply")[0].pair_array("headers").unwrap(),
            [("nonce".to_owned(), "aabbccddeeff".to_owned())]
        );
    }

    #[test]
    fn multiline_strings_drop_the_opening_newline() {
        let doc = parse("body = \"\"\"\n{\"a\": 1}\n\"\"\"\n").unwrap();
        assert_eq!(doc.string("body", "").unwrap(), "{\"a\": 1}\n");
    }

    #[test]
    fn a_mistyped_key_is_an_error_not_a_default() {
        let doc = parse("conccurency = 3\n").unwrap();
        assert!(doc.reject_unknown(&["concurrency"], &[]).is_err());
    }
}
