//! A JSON value with JavaScript's semantics, not Rust's.
//!
//! `serde_json::Value` is the obvious choice here and it is the wrong one, for
//! two reasons that are both observable in this program's output.
//!
//! **Key order.** `Object.entries()` does not enumerate in document order.
//! ECMAScript enumerates *canonical array-index* keys — decimal, no leading
//! zero, value at most 2³²−2 — in ascending numeric order first, and only then
//! the remaining keys in insertion order. The program iterates two maps keyed
//! by number: commodities (ids around 128 049 204, all indices) and a system's
//! markets (ids around 4 306 502 403, all *past* the index limit). So the
//! commodity list is silently renumbered into ascending id order while the
//! market list keeps document order — and that ordering reaches the sweep
//! queue, the progress lines, the `SWEEP RESULTS` rows and the EDDN
//! `commodities` array. `serde_json`'s `preserve_order` gives document order
//! and would be wrong for the first; `BTreeMap` gives lexicographic order and
//! would be wrong for both.
//!
//! **Number rendering.** Every JSON number in JavaScript is an `f64`, and
//! `JSON.stringify` prints an integral one with no decimal point.
//! `serde_json` prints `1.0`. The EDDN gateway validates draft-04
//! `"type": "integer"` with CPython's `jsonschema`, where `isinstance(1.0, int)`
//! is `False` — so a port that let `serde_json` serialize the payload would be
//! rejected with HTTP 400 on every single upload, and the EDDN spec forbids
//! retrying that. `clippy.toml` bans `serde_json::to_string` for this reason.
//!
//! `serde_json` is still used here, but only as a lexer: a `Visitor` funnels
//! every number into `f64` and every object into insertion-ordered pairs, and
//! this module owns the ordering and the serialization.

use std::collections::HashSet;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use super::{js_number, write_js_number};

/// A parsed JSON document.
#[derive(Clone, Debug, PartialEq)]
pub enum JsValue {
    Null,
    Bool(bool),
    /// Always an `f64`, including for integers. See the module docs — the
    /// program depends on `>2^53` values rounding.
    Num(f64),
    Str(Box<str>),
    Arr(Vec<JsValue>),
    Obj(JsObject),
}

/// A JSON object whose entries are held in ECMAScript enumeration order.
///
/// The ordering is applied once, at construction, so every later traversal is
/// a plain slice walk.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JsObject {
    entries: Vec<(Box<str>, JsValue)>,
}

/// Is `key` a canonical array index — the thing that makes ECMAScript hoist it
/// to the front of the enumeration?
///
/// The test is stricter than "parses as a number". `"01"`, `"1.5"`, `"1e3"`,
/// `"-1"`, `" 1"` and `"4294967295"` are all ordinary string keys; `"0"` and
/// `"4294967294"` are indices. The boundary is not decorative: market ids sit
/// on the far side of it and commodity ids on the near side, which is why the
/// two maps enumerate differently.
#[must_use]
pub fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 {
        return None;
    }
    if bytes[0] == b'0' && bytes.len() > 1 {
        return None;
    }
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n: u64 = key.parse().ok()?;
    // Array indices run to 2^32 - 2; 2^32 - 1 is a plain key.
    (n <= 0xFFFF_FFFE).then_some(n as u32)
}

impl JsObject {
    /// Builds an object from entries in document order, applying ECMAScript's
    /// duplicate rule (**last value wins, in the first key's slot**) and then
    /// its enumeration order.
    #[must_use]
    pub fn from_document_order(raw: Vec<(Box<str>, JsValue)>) -> Self {
        // Real payloads never repeat a key, and the deduplicating path is the
        // only quadratic-ish step in parsing, so establish up front whether it
        // is needed. The set borrows the keys rather than cloning them.
        let has_duplicates = {
            let mut seen = HashSet::with_capacity(raw.len());
            !raw.iter().all(|(key, _)| seen.insert(key.as_ref()))
        };

        let deduplicated = if has_duplicates {
            let mut entries: Vec<(Box<str>, JsValue)> = Vec::with_capacity(raw.len());
            for (key, value) in raw {
                match entries.iter_mut().find(|(existing, _)| *existing == key) {
                    Some(slot) => slot.1 = value,
                    None => entries.push((key, value)),
                }
            }
            entries
        } else {
            raw
        };

        // Indices first in numeric order, then everything else in insertion
        // order. `sort_by_key` is stable, which matters only for the
        // impossible case of two equal indices, but costs nothing.
        let (mut indexed, plain): (Vec<_>, Vec<_>) = deduplicated
            .into_iter()
            .map(|entry| (array_index(&entry.0), entry))
            .partition(|(index, _)| index.is_some());
        indexed.sort_by_key(|(index, _)| *index);

        Self {
            entries: indexed
                .into_iter()
                .chain(plain)
                .map(|(_, entry)| entry)
                .collect(),
        }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&JsValue> {
        self.entries
            .iter()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, v)| v)
    }

    /// `key in object` — a presence probe, distinct from "is not null".
    ///
    /// The distinction is load-bearing: `"credits" in payload` is true when the
    /// value is `null`, and the program then reads it as `0`, which clamps
    /// every buy to nothing. See R18.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k.as_ref() == key)
    }

    /// `Object.entries()` — in ECMAScript enumeration order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &JsValue)> {
        self.entries.iter().map(|(k, v)| (k.as_ref(), v))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<'a> IntoIterator for &'a JsObject {
    type Item = (&'a str, &'a JsValue);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (Box<str>, JsValue)>,
        fn(&'a (Box<str>, JsValue)) -> (&'a str, &'a JsValue),
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn project(pair: &(Box<str>, JsValue)) -> (&str, &JsValue) {
            (pair.0.as_ref(), &pair.1)
        }
        self.entries.iter().map(project)
    }
}

// ---------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------

impl JsValue {
    #[must_use]
    pub fn as_object(&self) -> Option<&JsObject> {
        match self {
            Self::Obj(o) => Some(o),
            _ => None,
        }
    }

    /// `asRecord` (ts:562) — an object, and specifically not an array or null.
    #[must_use]
    pub fn as_record(&self) -> Option<&JsObject> {
        self.as_object()
    }

    #[must_use]
    pub fn as_array(&self) -> Option<&[JsValue]> {
        match self {
            Self::Arr(a) => Some(a),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The value if it is a JSON number, without any coercion.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Why a document could not be parsed.
///
/// Two shapes are accepted by JavaScript and rejected here, both recorded as
/// C15: a lone surrogate escape (`"\uD800"`), and a numeric literal that
/// overflows to infinity (`1e999`). Neither occurs in Companion API data, and
/// both route into the same "could not decode, print it raw" path the
/// TypeScript uses for any other unparseable body, so the degradation is
/// identical even though the cause differs.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(String);

impl JsValue {
    /// `JSON.parse`.
    pub fn parse(source: &str) -> Result<Self, ParseError> {
        let mut de = serde_json::Deserializer::from_str(source);
        let value = Self::deserialize(&mut de).map_err(|e| ParseError(e.to_string()))?;
        de.end().map_err(|e| ParseError(e.to_string()))?;
        Ok(value)
    }
}

impl<'de> Deserialize<'de> for JsValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsValueVisitor)
    }
}

struct JsValueVisitor;

impl<'de> Visitor<'de> for JsValueVisitor {
    type Value = JsValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(JsValue::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(JsValue::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(JsValue::Bool(v))
    }

    // Every integer becomes an `f64`, which is where `JSON.parse`'s precision
    // loss above 2^53 comes from. That loss is not incidental damage: the
    // faction lookup at ts:2703 depends on it, comparing ids that no longer
    // round-trip. See R2 and R19.
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(JsValue::Num(v as f64))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(JsValue::Num(v as f64))
    }

    fn visit_i128<E: de::Error>(self, v: i128) -> Result<Self::Value, E> {
        Ok(JsValue::Num(v as f64))
    }

    fn visit_u128<E: de::Error>(self, v: u128) -> Result<Self::Value, E> {
        Ok(JsValue::Num(v as f64))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(JsValue::Num(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(JsValue::Str(v.into()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(JsValue::Str(v.into_boxed_str()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(JsValue::Arr(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut raw = Vec::with_capacity(map.size_hint().unwrap_or(0));
        while let Some((key, value)) = map.next_entry::<String, JsValue>()? {
            raw.push((key.into_boxed_str(), value));
        }
        Ok(JsValue::Obj(JsObject::from_document_order(raw)))
    }
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

impl JsValue {
    /// `JSON.stringify(value)`.
    #[must_use]
    pub fn stringify_compact(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None, 0);
        out
    }

    /// `JSON.stringify(value, null, indent)`.
    #[must_use]
    pub fn stringify(&self, indent: usize) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(indent), 0);
        out
    }

    fn write(&self, out: &mut String, indent: Option<usize>, depth: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            // R4: NaN and both infinities serialize as `null`, not as an error
            // and not as `NaN`.
            Self::Num(n) if !n.is_finite() => out.push_str("null"),
            Self::Num(n) => write_js_number(out, *n),
            Self::Str(s) => write_json_string(out, s),
            Self::Arr(items) => {
                if items.is_empty() {
                    // An empty collection stays on one line even when
                    // indenting, which a naive pretty-printer gets wrong.
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_newline_indent(out, indent, depth + 1);
                    item.write(out, indent, depth + 1);
                }
                write_newline_indent(out, indent, depth);
                out.push(']');
            }
            Self::Obj(object) => {
                if object.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (key, value)) in object.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_newline_indent(out, indent, depth + 1);
                    write_json_string(out, key);
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }
                    value.write(out, indent, depth + 1);
                }
                write_newline_indent(out, indent, depth);
                out.push('}');
            }
        }
    }
}

fn write_newline_indent(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(width) = indent {
        out.push('\n');
        out.extend(core::iter::repeat_n(' ', width * depth));
    }
}

/// `QuoteJSONString`. Note what is *not* escaped: everything above U+001F goes
/// out as raw UTF-8, including DEL and every non-ASCII character.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{9}' => out.push_str("\\t"),
            '\u{A}' => out.push_str("\\n"),
            '\u{C}' => out.push_str("\\f"),
            '\u{D}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                // Lowercase hex, as JavaScript emits it.
                use fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl fmt::Display for JsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.stringify_compact())
    }
}

/// `String(value)` applied to a JSON value, as template interpolation does.
#[must_use]
pub fn to_js_string(value: &JsValue) -> String {
    match value {
        JsValue::Null => "null".to_owned(),
        JsValue::Bool(true) => "true".to_owned(),
        JsValue::Bool(false) => "false".to_owned(),
        JsValue::Num(n) => js_number(*n),
        JsValue::Str(s) => s.to_string(),
        // Arrays join with commas and objects stringify to a fixed tag; neither
        // is reached by this program, but totality beats a panic.
        JsValue::Arr(items) => items.iter().map(to_js_string).collect::<Vec<_>>().join(","),
        JsValue::Obj(_) => "[object Object]".to_owned(),
    }
}
